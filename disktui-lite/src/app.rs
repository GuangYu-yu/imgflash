use std::fs;
use std::process::Child;

use crate::disk::DiskInfo;
use crate::notification::Notification;
use crate::theme::Theme;

use anyhow::Result;
use ratatui::widgets::TableState;

pub type AppResult<T> = Result<T>;

// ── Screens (state machine) ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Screen {
    DiskList,
    Confirmation,
    Writing,
    WriteError,
    Success,
}

// ── Exit actions ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ExitAction {
    #[default]
    None,
    PowerOff,
    Reboot,
}

// ── Dialog states ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ConfirmButton {
    #[default]
    No,
    Yes,
}

impl ConfirmButton {
    pub fn toggle(&mut self) {
        *self = match self {
            Self::No => Self::Yes,
            Self::Yes => Self::No,
        };
    }
}

// ── Success screen actions ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SuccessAction {
    #[default]
    Reboot,
    Back,
}

impl SuccessAction {
    pub fn toggle(&mut self) {
        *self = match self {
            Self::Reboot => Self::Back,
            Self::Back => Self::Reboot,
        };
    }
}

// ── Write progress tracking ─────────────────────────────────────────────

#[derive(Debug)]
pub struct WriteProgress {
    pub(crate) disk_name: String,
    pub(crate) disk_model: String,
    pub(crate) total_bytes: u64,
    pub(crate) written_bytes: u64,
    /// Instantaneous speed in MB/s.
    pub(crate) speed: f64,
    dd_child: Option<Child>,
    dd_pid: u32,
    pub(crate) finished: bool,
    pub(crate) success: bool,
    pub(crate) spinner_index: usize,
}

impl WriteProgress {
    pub fn new(disk: &DiskInfo, total_bytes: u64, child: Child) -> Self {
        let pid = child.id();
        Self {
            disk_name: disk.name.clone(),
            disk_model: disk.model.clone().unwrap_or_default(),
            total_bytes,
            written_bytes: 0,
            speed: 0.0,
            dd_child: Some(child),
            dd_pid: pid,
            finished: false,
            success: false,
            spinner_index: 0,
        }
    }

    pub fn update_progress(&mut self, new_written: u64, tick_interval_secs: f64) {
        if new_written > self.written_bytes {
            let delta = new_written - self.written_bytes;
            self.speed = (delta as f64) / (tick_interval_secs * 1048576.0);
            self.written_bytes = new_written;
        }
    }

    pub fn pct(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.written_bytes as f64 / self.total_bytes as f64).min(1.0)
    }

    /// Read wchar from /proc/$PID/io for the dd child process.
    pub fn read_written_bytes(&self) -> u64 {
        let io_content = match fs::read_to_string(format!("/proc/{}/io", self.dd_pid)) {
            Ok(c) => c,
            Err(_) => return self.written_bytes, // fallback: keep last value
        };
        for line in io_content.lines() {
            if line.starts_with("wchar:")
                && let Some(val) = line.split_whitespace().nth(1)
                && let Ok(n) = val.parse::<u64>()
            {
                return n;
            }
        }
        self.written_bytes
    }

    /// Atomically check process status and read IO bytes to avoid PID reuse race.
    pub fn check_and_read_io(&mut self) -> (Option<bool>, u64) {
        // Read IO first, then check status — avoids PID-reuse race
        let io = self.read_written_bytes();

        let process_status = if let Some(ref mut child) = self.dd_child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.finished = true;
                    self.success = status.success();
                    self.dd_child = None;
                    Some(self.success)
                }
                Ok(None) => None,
                Err(_) => {
                    self.finished = true;
                    self.success = false;
                    Some(false)
                }
            }
        } else {
            Some(true)
        };

        (process_status, io)
    }

    /// Kill the dd child process and reap it.
    pub fn abort(&mut self) {
        if let Some(ref mut child) = self.dd_child {
            let _ = child.kill();
            let _ = child.wait(); // reap zombie
            self.dd_child = None;
        }
        self.finished = true;
        self.success = false;
    }
}

// ── Main App ────────────────────────────────────────────────────────────

pub struct App {
    pub running: bool,
    pub(crate) screen: Screen,
    pub(crate) disks: Vec<DiskInfo>,
    pub(crate) disks_state: TableState,
    pub(crate) confirm_button: ConfirmButton,
    pub(crate) progress: Option<WriteProgress>,
    pub(crate) success_action: SuccessAction,
    pub(crate) reboot_counting: bool,
    pub(crate) reboot_countdown: u8,
    pub(crate) reboot_last_tick: u64,
    pub(crate) notifications: Vec<Notification>,
    pub(crate) show_help: bool,
    pub(crate) theme: Theme,
    pub(crate) tick_count: u64,
    pub exit_action: ExitAction,
}

impl App {
    pub const IMAGE_FILE: &'static str = "/image/image.img";
    const REBOOT_SECONDS: u8 = 5;

    pub fn new() -> AppResult<Self> {
        let disks = DiskInfo::enumerate()?;
        let mut disks_state = TableState::default();
        if !disks.is_empty() {
            disks_state.select(Some(0));
        }

        Ok(Self {
            running: true,
            screen: Screen::DiskList,
            disks,
            disks_state,
            confirm_button: ConfirmButton::default(),
            progress: None,
            success_action: SuccessAction::default(),
            reboot_counting: false,
            reboot_countdown: Self::REBOOT_SECONDS,
            reboot_last_tick: 0,
            notifications: Vec::new(),
            show_help: false,
            theme: Theme::default(),
            tick_count: 0,
            exit_action: ExitAction::None,
        })
    }

    // ── Disk queries ────────────────────────────────────────────────────

    pub fn refresh_disks(&mut self) -> AppResult<()> {
        let selected = self.disks_state.selected();
        self.disks = DiskInfo::enumerate()?;
        if let Some(idx) = selected {
            if idx < self.disks.len() {
                self.disks_state.select(Some(idx));
            } else if !self.disks.is_empty() {
                self.disks_state.select(Some(0));
            } else {
                self.disks_state.select(None);
            }
        }
        Ok(())
    }

    pub fn selected_disk(&self) -> Option<&DiskInfo> {
        self.disks_state.selected().and_then(|i| self.disks.get(i))
    }

    pub fn has_disks(&self) -> bool {
        !self.disks.is_empty()
    }

    pub fn image_file_size(&self) -> Option<u64> {
        std::fs::metadata(Self::IMAGE_FILE).ok().map(|m| m.len())
    }

    pub fn image_exists(&self) -> bool {
        std::path::Path::new(Self::IMAGE_FILE).exists()
    }

    // ── Tick ────────────────────────────────────────────────────────────

    pub fn tick(&mut self) {
        self.tick_count += 1;

        // Decay notifications
        self.notifications.retain(|n| n.ttl > 0);
        for n in &mut self.notifications {
            n.ttl -= 1;
        }

        // Rotate spinner when writing
        if self.screen == Screen::Writing {
            let p = self.progress.as_mut().unwrap();
            p.spinner_index = (p.spinner_index + 1) % 10;
        }

        // Reboot countdown (10 ticks = 1 second at 100ms poll)
        if self.screen == Screen::Success && self.reboot_counting
            && self.tick_count - self.reboot_last_tick >= 10 && self.reboot_countdown > 0
        {
            self.reboot_last_tick = self.tick_count;
            self.reboot_countdown -= 1;
            if self.reboot_countdown == 0 {
                self.exit_action = ExitAction::Reboot;
                self.running = false;
            }
        }
    }

    // ── Notifications ───────────────────────────────────────────────────

    pub fn notify(&mut self, message: impl Into<String>, level: crate::notification::NotificationLevel) {
        let ttl = match level {
            crate::notification::NotificationLevel::Error => 100,
            crate::notification::NotificationLevel::Warning => 60,
            crate::notification::NotificationLevel::Info => 40,
        };
        self.notifications.push(Notification {
            message: message.into(),
            level,
            ttl,
        });
    }

    // ── State transitions ───────────────────────────────────────────────

    pub fn goto_disk_list(&mut self) {
        self.screen = Screen::DiskList;
        self.progress = None;
    }

    pub fn goto_confirmation(&mut self) {
        self.confirm_button = ConfirmButton::No;
        self.screen = Screen::Confirmation;
    }

    pub fn goto_writing(&mut self, progress: WriteProgress) {
        self.progress = Some(progress);
        self.screen = Screen::Writing;
    }

    pub fn goto_write_error(&mut self) {
        self.screen = Screen::WriteError;
    }

    pub fn goto_success(&mut self) {
        self.success_action = SuccessAction::default();
        self.reboot_counting = false;
        self.reboot_countdown = Self::REBOOT_SECONDS;
        self.reboot_last_tick = self.tick_count;
        self.screen = Screen::Success;
    }

    pub fn skip_reboot_countdown(&mut self) {
        self.exit_action = ExitAction::Reboot;
        self.running = false;
    }

    pub fn quit(&mut self) {
        if let Some(ref mut p) = self.progress {
            p.abort();
        }
        self.exit_action = ExitAction::PowerOff;
        self.running = false;
    }
}