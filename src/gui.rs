use iced::widget::{button, checkbox, column, container, pick_list, row, text, text_input};
use iced::{Element, Length, Task};

pub fn run_gui() -> crate::error::Result<()> {
    iced::application(
        "Choosable - Bootable USB Creator",
        ChoosableApp::update,
        ChoosableApp::view,
    )
    .run_with(|| (ChoosableApp::default(), Task::none()))
    .map_err(|e| crate::error::ChoosableError::Generic(format!("GUI error: {e}")))
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
        f.write_str(match self {
            FsType::ExFat => "exFAT",
            FsType::Ntfs => "NTFS",
            FsType::Fat32 => "FAT32",
        })
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
        write!(
            f,
            "{} ({}, {} GiB{})",
            self.path,
            self.model,
            self.size_gb,
            if self.is_usb { ", USB" } else { "" }
        )
    }
}

struct ChoosableApp {
    disks: Vec<DiskEntry>,
    selected_disk_index: Option<usize>,
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
            selected_disk_index: None,
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
                return Task::perform(refresh_disks(), Message::DisksLoaded);
            }
            Message::DisksLoaded(disks) => {
                self.disks = disks;
                self.loading = false;
                self.status = format!("{} disks found.", self.disks.len());
                self.selected_disk_index = self
                    .selected_disk_index
                    .filter(|&i| i < self.disks.len())
                    .or_else(|| (!self.disks.is_empty()).then_some(0));
            }
            Message::SelectDisk(display) => {
                self.selected_disk_index = self
                    .disks
                    .iter()
                    .position(|d| d.to_string() == display);
            }
            Message::ToggleGpt(val) => self.use_gpt = val,
            Message::ToggleSecureBoot(val) => self.secure_boot = val,
            Message::ToggleForce(val) => self.force = val,
            Message::LabelChanged(s) => self.label = s,
            Message::ReserveSpaceChanged(s) => self.reserve_space = s,
            Message::NonDestructiveToggled(val) => self.non_destructive = val,
            Message::FsTypeChanged(fs) => self.fs_type = fs,
            Message::InstallClicked => {
                if let Some(index) = self.selected_disk_index {
                    let disk_path = self.disks[index].path.clone();
                    let reserve: u64 = self.reserve_space.parse().unwrap_or(0);
                    self.loading = true;
                    self.status = String::from("Installing...");
                    return Task::perform(
                        run_install(
                            disk_path,
                            self.use_gpt,
                            self.secure_boot,
                            self.force,
                            self.label.clone(),
                            self.fs_type,
                            self.non_destructive,
                            reserve,
                        ),
                        Message::StatusMessage,
                    );
                }
                self.status = String::from("No disk selected.");
            }
            Message::UpdateClicked => {
                if let Some(index) = self.selected_disk_index {
                    let secure_boot = Some(self.secure_boot);
                    self.loading = true;
                    self.status = String::from("Updating...");
                    return Task::perform(
                        run_update(self.disks[index].path.clone(), secure_boot),
                        Message::StatusMessage,
                    );
                }
                self.status = String::from("No disk selected.");
            }
            Message::ListClicked => {
                if let Some(index) = self.selected_disk_index {
                    self.loading = true;
                    self.status = String::from("Reading info...");
                    return Task::perform(
                        run_list(self.disks[index].path.clone()),
                        Message::StatusMessage,
                    );
                }
            }
            Message::StatusMessage(msg) => {
                self.loading = false;
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
            let selected = self
                .selected_disk_index
                .and_then(|i| entries.get(i))
                .cloned();

            pick_list(entries, selected, Message::SelectDisk)
                .placeholder("Select a disk...")
                .width(Length::Fill)
                .into()
        };

        let refresh_btn = button(text("Refresh")).on_press(Message::RefreshDisks);

        let options = column![
            checkbox("GPT Partition Style", self.use_gpt).on_toggle(Message::ToggleGpt),
            checkbox("Secure Boot Support", self.secure_boot).on_toggle(Message::ToggleSecureBoot),
            checkbox("Force Install", self.force).on_toggle(Message::ToggleForce),
            checkbox("Non-destructive Install", self.non_destructive)
                .on_toggle(Message::NonDestructiveToggled),
            row![
                text("Filesystem: "),
                pick_list(FsType::ALL, Some(self.fs_type), Message::FsTypeChanged),
            ]
            .spacing(8),
            row![
                text("Label: "),
                text_input("Choosable", &self.label)
                    .on_input(Message::LabelChanged)
                    .width(Length::Fixed(150.0)),
            ]
            .spacing(8),
            row![
                text("Reserve (MiB): "),
                text_input("0", &self.reserve_space)
                    .on_input(Message::ReserveSpaceChanged)
                    .width(Length::Fixed(100.0)),
            ]
            .spacing(8),
        ]
        .spacing(8);

        let actions = if self.loading {
            column![
                button(text("Install")).style(button::danger),
                button(text("Update")),
                button(text("List Info")),
            ]
            .spacing(4)
        } else {
            column![
                button(text("Install"))
                    .on_press(Message::InstallClicked)
                    .style(button::danger),
                button(text("Update")).on_press(Message::UpdateClicked),
                button(text("List Info")).on_press(Message::ListClicked),
            ]
            .spacing(4)
        };

        let status_color = if self.loading {
            iced::Color::from_rgb(0.2, 0.5, 0.8)
        } else {
            iced::Color::from_rgba(0.0, 0.0, 0.0, 1.0)
        };

        let content = column![
            row![text("Choosable").size(24),].padding(8),
            row![disk_picker, refresh_btn]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            options,
            row![actions].spacing(8),
            status_text,
        ]
        .spacing(12)
        .padding(16);

        container(content).center_x(Length::Fill).into()
    }
}

// ── Async tasks (yes=true to avoid stdin prompts in GUI) ────────────────

async fn refresh_disks() -> Vec<DiskEntry> {
    crate::disk::enumerate_disks()
        .map(|disks| {
            disks
                .into_iter()
                .map(|d| DiskEntry {
                    path: d.disk_path,
                    model: d.model,
                    size_gb: crate::disk::human_readable_gb(d.size_bytes),
                    is_usb: d.is_usb,
                    removable: d.removable,
                })
                .collect()
        })
        .unwrap_or_default()
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
    let ft = fs_type_to_installer(fs_type);

    let result = if non_destructive {
        crate::installer::non_destructive_install(&disk_path, &label, ft, secure_boot, true)
    } else {
        crate::installer::install_choosable(&disk_path, gpt, secure_boot, reserve, &label, ft, force, true)
    };

    format_result(result, "Installation")
}

async fn run_update(disk_path: String, secure_boot: Option<bool>) -> String {
    format_result(crate::installer::update_choosable(&disk_path, secure_boot, true), "Update")
}

async fn run_list(disk_path: String) -> String {
    match crate::installer::list_choosable(&disk_path) {
        Ok(()) => String::from("Info displayed in terminal (stdout)."),
        Err(e) => format!("List failed: {e}"),
    }
}

fn fs_type_to_installer(fs: FsType) -> crate::installer::FilesystemType {
    match fs {
        FsType::ExFat => crate::installer::FilesystemType::ExFat,
        FsType::Ntfs => crate::installer::FilesystemType::Ntfs,
        FsType::Fat32 => crate::installer::FilesystemType::Fat32,
    }
}

fn format_result(result: crate::error::Result<()>, op: &str) -> String {
    match result {
        Ok(()) => format!("{op} completed successfully!"),
        Err(e) => {
            let mut msg = format!("{op} failed: {e}");
            if e.to_string().contains("Permission denied") {
                msg.push_str("\nHint: Run Choosable with sudo (sudo choosable) to get disk access.");
            }
            msg
        }
    }
}