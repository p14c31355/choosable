use iced::widget::{button, checkbox, column, container, pick_list, row, text, text_input};
use iced::{Element, Length, Task};

pub fn run_gui() -> crate::error::Result<()> {
    iced::application("Choosable - Bootable USB Creator", ChoosableApp::update, ChoosableApp::view)
        .run_with(|| (ChoosableApp::default(), Task::none()))
        .map_err(|e| crate::error::ChoosableError::Generic(format!("GUI error: {}", e)))
}

#[derive(Debug, Clone)]
enum Message {
    RefreshDisks,
    DisksLoaded(Vec<DiskEntry>),
    SelectDisk(String),
    ToggleGpt(bool),
    ToggleSecureBoot(bool),
    ToggleForce(bool),
    LabelChanged(String),
    ReserveSpaceChanged(String),
    InstallClicked,
    UpdateClicked,
    ListClicked,
    NonDestructiveToggled(bool),
    FsTypeChanged(FsType),
    StatusMessage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsType {
    ExFat,
    Ntfs,
    Fat32,
}

impl FsType {
    const ALL: &[FsType] = &[FsType::ExFat, FsType::Ntfs, FsType::Fat32];
}

impl std::fmt::Display for FsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsType::ExFat => write!(f, "exFAT"),
            FsType::Ntfs => write!(f, "NTFS"),
            FsType::Fat32 => write!(f, "FAT32"),
        }
    }
}

#[derive(Debug, Clone)]
struct DiskEntry {
    path: String,
    model: String,
    size_gb: u64,
    is_usb: bool,
    removable: bool,
}

impl std::fmt::Display for DiskEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {} GiB{})", self.path, self.model, self.size_gb,
            if self.is_usb { ", USB" } else { "" }
        )
    }
}

struct ChoosableApp {
    disks: Vec<DiskEntry>,
    selected_disk_path: Option<String>,
    use_gpt: bool,
    secure_boot: bool,
    force: bool,
    label: String,
    reserve_space: String,
    non_destructive: bool,
    fs_type: FsType,
    status: String,
    loading: bool,
}

impl Default for ChoosableApp {
    fn default() -> Self {
        Self {
            disks: Vec::new(),
            selected_disk_path: None,
            use_gpt: false,
            secure_boot: true,
            force: false,
            label: String::from("Choosable"),
            reserve_space: String::new(),
            non_destructive: false,
            fs_type: FsType::ExFat,
            status: String::from("Press 'Refresh' to list disks."),
            loading: false,
        }
    }
}

impl ChoosableApp {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::RefreshDisks => {
                self.loading = true;
                self.status = String::from("Scanning disks...");
                return Task::perform(
                    refresh_disks(),
                    Message::DisksLoaded,
                );
            }
            Message::DisksLoaded(disks) => {
                self.disks = disks;
                self.loading = false;
                self.status = format!("{} disks found.", self.disks.len());
                if self.selected_disk_path.is_none() && !self.disks.is_empty() {
                    self.selected_disk_path = Some(self.disks[0].path.clone());
                }
            }
            Message::SelectDisk(path) => {
                self.selected_disk_path = Some(path);
            }
            Message::ToggleGpt(val) => self.use_gpt = val,
            Message::ToggleSecureBoot(val) => self.secure_boot = val,
            Message::ToggleForce(val) => self.force = val,
            Message::LabelChanged(s) => self.label = s,
            Message::ReserveSpaceChanged(s) => self.reserve_space = s,
            Message::NonDestructiveToggled(val) => self.non_destructive = val,
            Message::FsTypeChanged(fs) => self.fs_type = fs,
            Message::InstallClicked => {
                if let Some(ref disk_path) = self.selected_disk_path {
                    let disk_path = disk_path.clone();
                    let gpt = self.use_gpt;
                    let secure_boot = self.secure_boot;
                    let force = self.force;
                    let label = self.label.clone();
                    let fs_type = self.fs_type;
                    let non_destructive = self.non_destructive;
                    let reserve: u64 = self.reserve_space.parse().unwrap_or(0);

                    self.status = String::from("Installing...");

                    return Task::perform(
                        run_install(disk_path, gpt, secure_boot, force, label, fs_type, non_destructive, reserve),
                        Message::StatusMessage,
                    );
                }
                self.status = String::from("No disk selected.");
            }
            Message::UpdateClicked => {
                if let Some(ref disk_path) = self.selected_disk_path {
                    let disk_path = disk_path.clone();
                    let secure_boot = if self.secure_boot { Some(true) } else { Some(false) };
                    self.status = String::from("Updating...");

                    return Task::perform(
                        run_update(disk_path, secure_boot),
                        Message::StatusMessage,
                    );
                }
                self.status = String::from("No disk selected.");
            }
            Message::ListClicked => {
                if let Some(ref disk_path) = self.selected_disk_path {
                    let disk_path = disk_path.clone();
                    self.status = String::from("Reading info...");

                    return Task::perform(
                        run_list(disk_path),
                        Message::StatusMessage,
                    );
                }
            }
            Message::StatusMessage(msg) => {
                self.status = msg;
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let disk_picker: Element<Message> = if self.disks.is_empty() {
            text("No disks found. Click Refresh to scan.").into()
        } else {
            let entries: Vec<String> = self.disks.iter().map(|d| d.to_string()).collect();
            let selected = self.selected_disk_path.clone().unwrap_or_default();

            pick_list(entries, Some(selected), Message::SelectDisk)
                .placeholder("Select a disk...")
                .width(Length::Fill)
                .into()
        };

        let refresh_btn = button(text("Refresh")).on_press(Message::RefreshDisks);

        let options = column![
            checkbox("GPT Partition Style", self.use_gpt).on_toggle(Message::ToggleGpt),
            checkbox("Secure Boot Support", self.secure_boot).on_toggle(Message::ToggleSecureBoot),
            checkbox("Force Install", self.force).on_toggle(Message::ToggleForce),
            checkbox("Non-destructive Install", self.non_destructive).on_toggle(Message::NonDestructiveToggled),
            row![
                text("Filesystem: "),
                pick_list(FsType::ALL, Some(self.fs_type), Message::FsTypeChanged),
            ].spacing(8),
            row![
                text("Label: "),
                text_input("Choosable", &self.label).on_input(Message::LabelChanged).width(Length::Fixed(150.0)),
            ].spacing(8),
            row![
                text("Reserve (MiB): "),
                text_input("0", &self.reserve_space).on_input(Message::ReserveSpaceChanged).width(Length::Fixed(100.0)),
            ].spacing(8),
        ].spacing(8);

        let actions = column![
            button(text("Install")).on_press(Message::InstallClicked).style(button::danger),
            button(text("Update")).on_press(Message::UpdateClicked),
            button(text("List Info")).on_press(Message::ListClicked),
        ].spacing(4);

        let status_text = if self.loading {
            text(&self.status).color(iced::Color::from_rgb(0.2, 0.5, 0.8))
        } else {
            text(&self.status)
        };

        let content = column![
            row![
                text("Choosable").size(24),
            ].padding(8),
            row![disk_picker, refresh_btn].spacing(8).align_y(iced::Alignment::Center),
            options,
            row![actions].spacing(8),
            status_text,
        ].spacing(12).padding(16);

        container(content).center_x(Length::Fill).into()
    }
}

// ── Async tasks (yes=true to avoid stdin prompts in GUI) ────────────────

async fn refresh_disks() -> Vec<DiskEntry> {
    match crate::disk::enumerate_disks() {
        Ok(disks) => {
            disks.into_iter().map(|d| DiskEntry {
                path: d.disk_path,
                model: d.model,
                size_gb: crate::disk::human_readable_gb(d.size_bytes),
                is_usb: d.is_usb,
                removable: d.removable,
            }).collect()
        }
        Err(_) => Vec::new(),
    }
}

async fn run_install(
    disk_path: String,
    gpt: bool,
    secure_boot: bool,
    force: bool,
    label: String,
    fs_type: FsType,
    non_destructive: bool,
    reserve: u64,
) -> String {
    let ft = match fs_type {
        FsType::ExFat => crate::installer::FilesystemType::ExFat,
        FsType::Ntfs => crate::installer::FilesystemType::Ntfs,
        FsType::Fat32 => crate::installer::FilesystemType::Fat32,
    };

    let result = if non_destructive {
        crate::installer::non_destructive_install(&disk_path, &label, ft, secure_boot, true)
    } else {
        crate::installer::install_choosable(&disk_path, gpt, secure_boot, reserve, &label, ft, force, true)
    };

    match result {
        Ok(()) => String::from("Installation completed successfully!"),
        Err(e) => format!("Installation failed: {}", e),
    }
}

async fn run_update(disk_path: String, secure_boot: Option<bool>) -> String {
    match crate::installer::update_choosable(&disk_path, secure_boot, true) {
        Ok(()) => String::from("Update completed successfully!"),
        Err(e) => format!("Update failed: {}", e),
    }
}

async fn run_list(disk_path: String) -> String {
    match crate::installer::list_choosable(&disk_path) {
        Ok(()) => String::from("Info displayed in terminal (stdout)."),
        Err(e) => format!("List failed: {}", e),
    }
}