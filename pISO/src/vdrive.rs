use action;
use bitmap;
use config;
use controller;
use displaymanager::{DisplayManager, Position, Widget, Window, WindowId};
use error::{ErrorKind, Result, ResultExt};
use font;
use input;
use iso;
use lvm;
use usb;
use utils;
use render;
use state;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

const VDRIVE_MOUNT_ROOT: &str = "/mnt";
const ISO_FOLDER: &str = "ISOS";

pub struct MountInfo {
    pub loopback_path: PathBuf,
    pub part_mount_paths: Vec<PathBuf>,
    pub isos: Vec<iso::Iso>,
}

pub enum MountState {
    Unmounted,
    Internal(MountInfo),
    External(usb::StorageID),
}

#[derive(Serialize, Deserialize)]
pub struct PersistVDriveState {
    pub external_mount: bool,
    pub readonly: bool,
    pub removable: bool,
}

impl Default for PersistVDriveState {
    fn default() -> PersistVDriveState {
        PersistVDriveState {
            external_mount: false,
            readonly: false,
            removable: true,
        }
    }
}

pub struct VirtualDrive {
    pub state: MountState,
    pub usb: Arc<Mutex<usb::UsbGadget>>,
    pub volume: lvm::LogicalVolume,
    pub window: WindowId,
    pub persist: PersistVDriveState,
    pub config: config::Config,
}

impl VirtualDrive {
    pub fn new(
        disp: &mut DisplayManager,
        usb: Arc<Mutex<usb::UsbGadget>>,
        volume: lvm::LogicalVolume,
        config: &config::Config,
    ) -> Result<VirtualDrive> {
        let our_window = disp.add_child(Position::Normal)?;
        Ok(VirtualDrive {
            window: our_window,
            state: MountState::Unmounted,
            usb: usb,
            volume: volume,
            persist: PersistVDriveState::default(),
            config: config.clone(),
        })
    }

    pub fn name(&self) -> &str {
        &self.volume.name
    }

    pub fn size(&self) -> u64 {
        self.volume.size
    }

    pub fn mount_external(&mut self) -> Result<()> {
        match self.state {
            MountState::External(_) => Ok(()),
            MountState::Unmounted => {
                let id = self.usb
                    .lock()?
                    .export_file(
                        &self.volume.path,
                        false,
                        self.persist.readonly,
                        self.persist.removable,
                    )
                    .chain_err(|| "failed to mount drive external")?;
                self.state = MountState::External(id);
                self.persist.external_mount = true;
                Ok(())
            }
            MountState::Internal(_) => {
                Err("Attempt to mount_external while mounted internal".into())
            }
        }
    }

    pub fn unmount_external(&mut self) -> Result<()> {
        match self.state {
            MountState::Unmounted => {}
            MountState::Internal(_) => {
                return Err("Attempt to unmount_external while mounted internal".into());
            }
            MountState::External(ref id) => {
                self.usb
                    .lock()?
                    .unexport_file(id)
                    .chain_err(|| "failed to unmount external")?;
            }
        }
        self.state = MountState::Unmounted;
        self.persist.external_mount = false;
        Ok(())
    }

    pub fn unmount(&mut self) -> Result<()> {
        match self.state {
            MountState::Unmounted => Ok(()),
            MountState::Internal(_) => self.unmount_internal(),
            MountState::External(_) => self.unmount_external(),
        }
    }

    fn mount_partition<P1, P2>(&self, device: P1, target: P2) -> Result<()>
    where
        P1: AsRef<Path>,
        P2: AsRef<Path>,
    {
        let mounters = &["mount", "mount.exfat", "mount.ntfs-3g"];
        for mounter in mounters {
            let fsmount = utils::run_check_output(mounter, &[device.as_ref(), target.as_ref()]);
            if fsmount.is_ok() {
                return Ok(());
            }
        }
        Err(format!(
            "Failed to mount: {} to {}",
            device.as_ref().display(),
            target.as_ref().display()
        ).into())
    }

    pub fn mount_internal<'a, 'b>(
        &'a mut self,
        disp: &'b mut DisplayManager,
    ) -> Result<&'a MountInfo> {
        match self.state {
            MountState::Unmounted => {
                let volume_path = &self.volume.path.to_string_lossy();
                let loopback_path =
                    PathBuf::from(utils::run_check_output("losetup", &["-f"])?.trim_right());
                let loopback_name: String = loopback_path
                    .file_name()
                    .ok_or(ErrorKind::Msg("loopback path has no file name".into()))?
                    .to_string_lossy()
                    .into();

                utils::run_check_output("losetup", &["-fP", volume_path])?;

                let mut mounted_partitions = vec![];
                let mut isos = vec![];
                for entry in fs::read_dir("/dev")? {
                    let entry = entry?;
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&loopback_name)
                    {
                        let dev_name = entry.file_name().to_string_lossy().into_owned();
                        // Skip the base loopback device
                        if dev_name == loopback_name {
                            continue;
                        }

                        let part_num = dev_name.split("p").last().ok_or(ErrorKind::Msg(
                            "Failed to determine partition number".into(),
                        ))?;

                        let part_name = utils::translate_drive_name(&self.name(), &self.config);
                        let mount_folder_name = format!("{} (partition {})", part_name, part_num);

                        let mount_point = Path::new(VDRIVE_MOUNT_ROOT).join(mount_folder_name);
                        fs::create_dir_all(&mount_point)?;
                        match self.mount_partition(&entry.path(), &mount_point) {
                            Ok(_) => {
                                mounted_partitions.push(mount_point.to_path_buf());

                                let isopath = mount_point.join(ISO_FOLDER);
                                if isopath.exists() {
                                    for iso in fs::read_dir(isopath)? {
                                        let iso = iso?;
                                        if iso.file_name()
                                            .into_string()
                                            .map_err(|_| ErrorKind::Msg("Invalid file name".into()))?
                                            .starts_with(".")
                                        {
                                            continue;
                                        }
                                        isos.push(iso::Iso::new(
                                            disp,
                                            self.usb.clone(),
                                            iso.path(),
                                        )?);
                                    }
                                }
                            }
                            Err(e) => println!("An error occured while mounting: {}", e),
                        }
                    }
                }
                self.state = MountState::Internal(MountInfo {
                    part_mount_paths: mounted_partitions,
                    isos: isos,
                    loopback_path: loopback_path.to_path_buf(),
                });
                match &self.state {
                    &MountState::Internal(ref info) => Ok(info),
                    _ => unreachable!(),
                }
            }
            MountState::Internal(ref state) => Ok(state),
            MountState::External(_) => {
                Err("Attempt to mount_internal while mounted external".into())
            }
        }
    }

    pub fn unmount_internal(&mut self) -> Result<()> {
        match self.state {
            MountState::Unmounted => {}
            MountState::Internal(ref mut info) => {
                for iso in info.isos.iter_mut() {
                    iso.unmount()?;
                }
                for part in info.part_mount_paths.iter() {
                    utils::run_check_output("umount", &[&part])?;
                    fs::remove_dir_all(&part)?;
                }
                utils::run_check_output("losetup", &["-d", &info.loopback_path.to_string_lossy()])?;
            }
            MountState::External(_) => {
                return Err("Attempt to unmount_internal while mounted external".into());
            }
        };
        self.state = MountState::Unmounted;
        Ok(())
    }

    pub fn toggle_mount(&mut self, disp: &mut DisplayManager) -> Result<()> {
        match self.state {
            // For now, just switch to external if unmounted
            MountState::Unmounted => self.mount_external(),
            MountState::Internal(_) => {
                self.unmount_internal()?;
                self.mount_external()
            }
            MountState::External(_) => {
                self.unmount_external()?;
                self.mount_internal(disp)?;
                Ok(())
            }
        }
    }
}

impl render::Render for VirtualDrive {
    fn render(&self, _manager: &DisplayManager, window: &Window) -> Result<bitmap::Bitmap> {
        let mut base = bitmap::Bitmap::new(10, 1);
        let short_size = self.size() as f64 / (1024 * 1024 * 1024) as f64;

        // Render the 'newname' from the config
        let render_name = utils::translate_drive_name(&self.name(), &self.config);

        let label = format!("{} ({:.1}GB)", render_name, short_size);
        base.blit(&font::render_text(label), (12, 0));
        match self.state {
            MountState::External(_) => {
                base.blit(&bitmap::Bitmap::from_slice(font::SQUARE), (6, 0));
            }
            _ => (),
        };
        if window.focus {
            base.blit(&bitmap::Bitmap::from_slice(font::ARROW), (0, 0));
        }
        Ok(base)
    }
}

impl input::Input for VirtualDrive {
    fn on_event(&mut self, event: &controller::Event) -> Result<(bool, Vec<action::Action>)> {
        match *event {
            controller::Event::Select => {
                Ok((true, vec![action::Action::ToggleVDriveMount(self.window)]))
            }
            _ => Ok((false, vec![])),
        }
    }

    fn do_action(
        &mut self,
        disp: &mut DisplayManager,
        action: &action::Action,
    ) -> Result<(bool, Vec<action::Action>)> {
        match *action {
            action::Action::ToggleVDriveMount(id) if id == self.window => {
                self.toggle_mount(disp)?;
                Ok((true, vec![]))
            }
            action::Action::ToggleDriveReadOnly(ref name) if name == self.name() => {
                self.persist.readonly = !self.persist.readonly;
                Ok((true, vec![]))
            }
            action::Action::ToggleDriveNonRemovable(ref name) if name == self.name() => {
                self.persist.removable = !self.persist.removable;
                Ok((true, vec![]))
            }
            _ => Ok((false, vec![])),
        }
    }
}

impl state::Stateful for VirtualDrive {
    type State = PersistVDriveState;
    fn state(&self) -> &Self::State {
        &self.persist
    }
    fn state_mut(&mut self) -> &mut Self::State {
        &mut self.persist
    }
    fn key(&self) -> String {
        self.name().into()
    }
    fn on_load(&mut self, disp: &mut DisplayManager) -> Result<()> {
        if self.persist.external_mount {
            self.mount_external()
        } else {
            self.mount_internal(disp)?;
            if *self.config
                .system
                .as_ref()
                .map(|s| s.auto_fstrim.as_ref().unwrap_or(&false))
                .unwrap_or(&false)
            {
                match self.state {
                    MountState::Internal(ref mount) => {
                        for path in mount.part_mount_paths.iter().cloned() {
                            thread::spawn(move || {
                                let _ = utils::run_check_output("fstrim", &[path]);
                            });
                        }
                    }
                    _ => (),
                }
            }
            Ok(())
        }
    }
}

impl Widget for VirtualDrive {
    fn mut_children(&mut self) -> Vec<&mut Widget> {
        match self.state {
            MountState::Internal(ref mut info) => {
                info.isos.iter_mut().map(|iso| iso as &mut Widget).collect()
            }
            _ => vec![],
        }
    }

    fn children(&self) -> Vec<&Widget> {
        match self.state {
            MountState::Internal(ref info) => info.isos.iter().map(|iso| iso as &Widget).collect(),
            _ => vec![],
        }
    }

    fn windowid(&self) -> WindowId {
        self.window
    }
}
