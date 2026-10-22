//! iced GUI mirroring the Java XDM main window: a toolbar on top, a dark
//! category sidebar on the left, download rows with progress bars in the
//! middle and a status bar at the bottom, plus a modal "New download" dialog.
//!
//! Architecture: the engine runs on a dedicated tokio runtime ([`rt`]); the
//! GUI polls each download's `watch::Receiver` on a 250 ms tick subscription
//! and mirrors state into plain view snapshots.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use iced::font::{Font, Weight};
use iced::widget::{
    button, column, container, mouse_area, progress_bar, rule, row, scrollable, space::Space,
    stack, text, text_input,
};
use iced::window::{Id, Mode};
use iced::{Alignment, Color, Element, Length, Subscription, Task, Theme};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xdm_core::engine::download;
use xdm_core::http::{DownloadConfig, DownloadError, DownloadState, Progress};

/// How often a failed download is re-spawned automatically before giving up.
const MAX_AUTO_RETRIES: u32 = 8;

/// Backoff before auto-retry attempt `n` (1-based): 2 s, 4 s, … capped at 30 s.
fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(u64::from(2 * attempt)).min(Duration::from_secs(30))
}

/// Java XDM 7.2.11 look (ColorResource.java): white main area, a near-black
/// toolbar band, Holo-cyan selection/progress accent, dark dialogs.
mod palette {
    use iced::Color;

    pub const TOOLBAR: Color = Color::from_rgb8(0x0E, 0x14, 0x19); // (14, 20, 25)
    pub const TOOLBAR_HOVER: Color = Color::from_rgb8(105, 105, 105); // Color.DARK_GRAY
    pub const TOOLBAR_PRESSED: Color = Color::from_rgb8(0x2B, 0x2B, 0x2B); // (43, 43, 43)
    pub const SIDEBAR: Color = Color::from_rgb8(0xFF, 0xFF, 0xFF);
    pub const SIDEBAR_HOVER: Color = Color::from_rgb8(0xF5, 0xF5, 0xF5);
    pub const SIDEBAR_ACTIVE: Color = Color::from_rgb8(0xF2, 0xF2, 0xF2); // activeTabColor
    pub const ACCENT: Color = Color::from_rgb8(0x33, 0xB5, 0xE5); // selectionColor
    pub const CARD: Color = Color::from_rgb8(0xFF, 0xFF, 0xFF);
    pub const CARD_BORDER: Color = Color::from_rgb8(0xEB, 0xEB, 0xEB); // (235, 235, 235) lineLbl
    pub const CONTENT: Color = Color::from_rgb8(0xFF, 0xFF, 0xFF);
    pub const ROW_SELECTED: Color = Color::from_rgb8(0x33, 0xB5, 0xE5);
    pub const TEXT: Color = Color::from_rgb8(0x00, 0x00, 0x00);
    pub const TEXT_DIM: Color = Color::from_rgb8(0x3C, 0x3C, 0x3C);
    pub const TEXT_DISABLED: Color = Color::from_rgb8(0xA0, 0xA0, 0xA0); // deepFontColor
    pub const TRACK: Color = Color::from_rgb8(0x80, 0x80, 0x80); // Color.GRAY
    pub const DIALOG_BG: Color = Color::from_rgb8(0x32, 0x32, 0x32); // darkerBgColor
    pub const DIALOG_INPUT: Color = Color::from_rgb8(0x1E, 0x1E, 0x1E); // darkestBgColor
    pub const DIALOG_BORDER: Color = Color::from_rgb8(0x49, 0x49, 0x49); // darkBgColor
    pub const DIALOG_TEXT: Color = Color::from_rgb8(0xEB, 0xEB, 0xEB); // whiteColor
    pub const DIALOG_DIM: Color = Color::from_rgb8(0xBE, 0xBE, 0xBE); // lightFontColor
    pub const GREEN: Color = Color::from_rgb8(0x43, 0xA0, 0x47);
    pub const AMBER: Color = Color::from_rgb8(0xFB, 0x8C, 0x00);
    pub const RED: Color = Color::from_rgb8(0xE5, 0x39, 0x35);
    pub const WHITE: Color = Color::from_rgb8(0xFF, 0xFF, 0xFF);
}

/// System tray integration (StatusNotifierItem, native to KDE Plasma).
///
/// The tray lives on the shared tokio runtime; callbacks push commands into a
/// static slot which the GUI tick drains ([`tray::take_command`]). Keeping the
/// link one-directional avoids any blocking call on either side.
mod tray {
    use ksni::menu::StandardItem;
    use ksni::{Icon, MenuItem, ToolTip, Tray};
    use std::sync::{Mutex, OnceLock};

    pub const APP_ID: &str = "xdm-app";

    /// One-shot mailbox between tray callbacks and the GUI tick.
    #[derive(Debug, Clone, Copy)]
    pub enum TrayCmd {
        Show,
        Quit,
    }

    fn slot() -> &'static Mutex<Option<TrayCmd>> {
        static SLOT: OnceLock<Mutex<Option<TrayCmd>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    fn push(cmd: TrayCmd) {
        let mut guard = slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(cmd);
    }

    pub fn take_command() -> Option<TrayCmd> {
        slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub struct TrayState;

    impl Tray for TrayState {
        fn id(&self) -> String {
            APP_ID.to_owned()
        }

        fn title(&self) -> String {
            "Xtreme Download Manager Rust".to_owned()
        }

        fn tool_tip(&self) -> ToolTip {
            ToolTip {
                icon_name: String::new(),
                icon_pixmap: Vec::new(),
                title: self.title(),
                description: "Download manager running in the background".to_owned(),
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            push(TrayCmd::Show);
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            vec![icon()]
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Show Window".to_owned(),
                    activate: Box::new(|_| push(TrayCmd::Show)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit".to_owned(),
                    activate: Box::new(|_| push(TrayCmd::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// The tray icon: a dark XDM disc with the cyan download arrow.
    pub fn icon() -> Icon {
        const S: i32 = 32;
        let mut data = Vec::with_capacity((S * S * 4) as usize);
        for y in 0..S {
            for x in 0..S {
                let (a, r, g, b) = pixel(x as f32, y as f32);
                data.extend_from_slice(&[a, r, g, b]);
            }
        }
        Icon {
            width: S,
            height: S,
            data,
        }
    }

    /// One pixel of [`icon`], as ARGB32 components (alpha, red, green, blue).
    fn pixel(x: f32, y: f32) -> (u8, u8, u8, u8) {
        let transparent = || (0u8, 0u8, 0u8, 0u8);
        // Dark disc (toolbar color), slightly inset for the tray mask.
        let dx = x - 15.5;
        let dy = y - 15.5;
        if dx * dx + dy * dy > 14.5 * 14.5 {
            return transparent();
        }
        // Cyan download arrow: triangle tapering downward from y 8 to the
        // apex at y 18, then a stem to y 24 (both centered on x 16.5).
        let arrowhead = (8.0..18.0).contains(&y) && (x - 16.5).abs() <= 7.5 * ((18.0 - y) / 10.0);
        let stem = (18.0..24.0).contains(&y) && (x - 16.5).abs() <= 5.5;
        if arrowhead || stem {
            (255, 0x33, 0xB5, 0xE5)
        } else {
            (255, 0x0E, 0x14, 0x19)
        }
    }
}

/// The engine needs a real tokio runtime (tokio::fs, tokio::spawn, timers);
/// iced's own executor does not provide one, so we own it explicitly.
static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

pub fn run() -> iced::Result {
    // The tray service lives on the shared tokio runtime; its callbacks only
    // push commands into a static slot that the GUI tick polls, so a tray
    // activation can never block the D-Bus menu thread on UI work.
    rt().spawn(async {
        match ksni::TrayMethods::spawn(tray::TrayState).await {
            Ok(handle) => std::mem::forget(handle),
            Err(e) => eprintln!("xdm-app: system tray unavailable: {e:?}"),
        }
    });
    iced::application(Gui::default, Gui::update, Gui::view)
        .title(|_: &Gui| "Xtreme Download Manager Rust".to_owned())
        .window(iced::window::Settings {
            size: (1020, 640).into(),
            // The close button is intercepted: we ask whether to minimize to
            // the tray instead of leaving without notice.
            exit_on_close_request: false,
            ..iced::window::Settings::default()
        })
        .subscription(Gui::subscription)
        .theme(|_: &Gui| Theme::Light)
        .run()
}

#[derive(Debug, Clone)]
pub enum Message {
    UrlInput(String),
    DestInput(String),
    Add,
    NewDownload,
    CancelModal,
    Tick,
    Select(usize),
    Category(Category),
    Pause(usize),
    Resume(usize),
    Remove(usize),
    OpenFolder(usize),
    AutoRetry(usize),
    /// Result of the first-tick `window::latest()` lookup.
    GotWindowId(Option<Id>),
    /// The window manager delivered a close request (X button).
    WinClose,
    /// Close dialog: keep running hidden in the tray.
    HideToTray,
    /// Close dialog: really quit.
    Quit,
    /// Close dialog dismissed without choosing (backdrop / cancel).
    CancelQuit,
}

/// Engine-side status, mirrored through the shared `AtomicU8`.
mod st {
    pub const RUNNING: u8 = 0;
    pub const PAUSED: u8 = 1;
    pub const FINISHED: u8 = 2;
    pub const FAILED: u8 = 3;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Running,
    Paused,
    Finished,
    Failed,
}

/// Sidebar categories, XDM-style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    All,
    Videos,
    Music,
    Programs,
    Compressed,
    Documents,
    Others,
    Completed,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::All => "All",
            Category::Videos => "Videos",
            Category::Music => "Music",
            Category::Programs => "Programs",
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Others => "Others",
            Category::Completed => "Completed",
        }
    }

    fn dot(self) -> Color {
        match self {
            Category::Videos => Color::from_rgb8(0x5C, 0x6B, 0xC0),
            Category::Music => Color::from_rgb8(0xAB, 0x47, 0xBC),
            Category::Programs => Color::from_rgb8(0x26, 0xA6, 0x9A),
            Category::Compressed => Color::from_rgb8(0x8D, 0x6E, 0x63),
            Category::Documents => Color::from_rgb8(0x42, 0xA5, 0xF5),
            Category::Others => Color::from_rgb8(0x78, 0x90, 0x9C),
            Category::All | Category::Completed => palette::ACCENT,
        }
    }

    /// Badge background for a download row of this category.
    fn badge(self) -> Color {
        self.dot()
    }
}

/// Maps a file name to its sidebar category, XDM-style.
fn category_for(name: &str) -> Category {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "ts" | "flv" | "wmv" | "m4v" | "mpg"
        | "mpeg" => Category::Videos,
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "opus" | "wma" => Category::Music,
        "exe" | "msi" | "dmg" | "iso" | "deb" | "rpm" | "appimage" | "jar" | "bin" | "pkg"
        | "apk" => Category::Programs,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" => Category::Compressed,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "epub" | "mobi"
        | "rtf" | "odt" => Category::Documents,
        _ => Category::Others,
    }
}

/// Short badge label for a file name: uppercase extension, at most 4 chars.
fn badge_label(name: &str) -> String {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("file");
    ext.to_ascii_uppercase().chars().take(4).collect()
}

/// Formats a byte count the way XDM does: `512 B`, `12.3 KB`, `42.06 GB`.
fn fmt_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

/// Formats a speed in bytes per second: `0 B/s`, `850 B/s`, `2.4 MB/s`.
fn fmt_speed(bps: f64) -> String {
    if bps < 1.0 {
        "0 B/s".to_owned()
    } else {
        format!("{}/s", fmt_size(bps as u64))
    }
}

/// Human ETA: `<1s`, `42s`, `4m 32s`, `1h 12m`.
fn fmt_eta(secs: u64) -> String {
    if secs < 1 {
        return "<1s".to_owned();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m {:02}s", secs % 60);
    }
    format!("{}h {:02}m", mins / 60, mins % 60)
}

/// ETA text from remaining bytes and speed; `None` when not computable.
fn eta_text(downloaded: u64, total: u64, speed_bps: f64) -> Option<String> {
    if total <= downloaded || speed_bps < 1.0 {
        return None;
    }
    Some(fmt_eta(((total - downloaded) as f64 / speed_bps).ceil() as u64))
}

#[derive(Default)]
struct DownloadItem {
    url: String,
    dest: PathBuf,
    /// Display name (file name of the destination).
    name: String,
    /// Receiver of the current (or last) run; replaced on resume.
    rx_slot: Arc<Mutex<Option<watch::Receiver<Progress>>>>,
    status: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    cancel: Option<CancellationToken>,
    // View snapshot, refreshed on Tick.
    progress: Progress,
    state: State,
    error_text: Option<String>,
    /// Speed estimate (EMA over ticks), bytes per second.
    speed_bps: f64,
    prev_downloaded: u64,
    last_tick: Option<Instant>,
    /// Driven by another process (e.g. the CLI); only observed, not controlled.
    external: bool,
    /// Consecutive engine failures without progress (auto-retry budget).
    retry_count: u32,
    /// A retry task is pending; prevents stacking one retry per tick.
    retry_scheduled: bool,
}

impl DownloadItem {
    /// Snapshot for persistence.
    fn saved(&self) -> SavedEntry {
        SavedEntry {
            url: self.url.clone(),
            dest: self.dest.to_string_lossy().into_owned(),
            name: self.name.clone(),
            downloaded: self.progress.downloaded,
            total: self.progress.total,
        }
    }

    /// Advances the smoothed speed estimate with a fresh progress sample.
    /// Callers sample at their own cadence (250 ms tick for owned downloads,
    /// ~2 s state-file scans for external ones).
    fn sample_progress(&mut self, now: Instant) {
        match self.last_tick {
            Some(last) => {
                let dt = now.duration_since(last).as_secs_f64();
                if dt >= 0.05 {
                    let instant =
                        (self.progress.downloaded.saturating_sub(self.prev_downloaded)) as f64 / dt;
                    self.speed_bps = if self.speed_bps <= 0.0 {
                        instant
                    } else {
                        self.speed_bps * 0.6 + instant * 0.4
                    };
                    self.prev_downloaded = self.progress.downloaded;
                    self.last_tick = Some(now);
                }
            }
            None => {
                self.prev_downloaded = self.progress.downloaded;
                self.last_tick = Some(now);
            }
        }
    }
}

struct Gui {
    url: String,
    dest: String,
    downloads: Vec<DownloadItem>,
    selected: Option<usize>,
    category: Category,
    modal_open: bool,
    /// Tick counter; state files are re-scanned every 8th tick (~2 s).
    scan_counter: u32,
    /// Destinations removed by the user; hidden from the state-file scanner
    /// until their engine state files disappear.
    removed: std::collections::HashSet<String>,
    /// Main window id, captured on the first tick; needed for tray commands.
    window_id: Option<Id>,
    /// The close button was pressed; show the minimize-to-tray dialog.
    confirm_quit: bool,
}

impl Default for Gui {
    fn default() -> Self {
        // Restores the last session's rows. Everything comes back external
        // (no engine is owned by a fresh process); the folder scanner then
        // re-classifies each row against real state files / destinations.
        let Some(list) = read_list(&list_path()) else {
            return Self {
                url: String::new(),
                dest: String::new(),
                downloads: Vec::new(),
                selected: None,
                category: Category::default(),
                modal_open: false,
                scan_counter: 0,
                removed: std::collections::HashSet::new(),
                window_id: None,
                confirm_quit: false,
            };
        };
        let downloads = restore_downloads(&list);
        let removed: std::collections::HashSet<String> = list.removed.into_iter().collect();
        Self {
            url: String::new(),
            dest: String::new(),
            downloads,
            selected: None,
            category: Category::default(),
            modal_open: false,
            scan_counter: 0,
            removed,
            window_id: None,
            confirm_quit: false,
        }
    }
}

/// Rebuilds GUI rows from the persisted list. Rows whose engine state file is
/// gone and whose destination exists on disk are already finished.
fn restore_downloads(list: &SavedList) -> Vec<DownloadItem> {
    let mut items = Vec::with_capacity(list.downloads.len());
    let mut seen = std::collections::HashSet::new();
    for entry in &list.downloads {
        let dest = PathBuf::from(&entry.dest);
        if !seen.insert(dest.clone()) {
            continue; // defensive: duplicate rows on disk
        }
        let dest_exists = dest.exists();
        let state_file_exists = dest
            .parent()
            .is_some_and(|dir| state_file_path(dir, &entry.name).exists());
        items.push(DownloadItem {
            url: entry.url.clone(),
            dest,
            name: entry.name.clone(),
            progress: Progress {
                downloaded: entry.downloaded,
                total: entry.total,
            },
            // Without a state file there is nothing to resume: a surviving
            // destination means the engine finished and cleaned up.
            state: if state_file_exists && dest_exists {
                State::Paused
            } else {
                State::Finished
            },
            // Match the status atom to the restored state; the RUNNING default
            // would render the row as an active download for the first tick.
            status: Arc::new(AtomicU8::new(if state_file_exists && dest_exists {
                st::PAUSED
            } else {
                st::FINISHED
            })),
            external: true,
            ..DownloadItem::default()
        });
    }
    items
}

impl Gui {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::UrlInput(s) => {
                self.url = s;
                Task::none()
            }
            Message::DestInput(s) => {
                self.dest = s;
                Task::none()
            }
            Message::NewDownload => {
                self.modal_open = true;
                Task::none()
            }
            Message::CancelModal => {
                self.modal_open = false;
                Task::none()
            }
            Message::Add => {
                let url = self.url.trim().to_owned();
                if url.is_empty() {
                    return Task::none();
                }
                let dest = destination(&url, &self.dest);
                // A fresh add clears any earlier remove-tombstone for it.
                self.removed.remove(dest.to_string_lossy().as_ref());
                // Re-adding an existing destination resumes that row instead
                // of spawning a second engine on the same files.
                if let Some(pos) = self.downloads.iter().position(|it| it.dest == dest) {
                    let running = self.downloads[pos].state == State::Running;
                    let finished = self.downloads[pos].state == State::Finished;
                    self.selected = Some(pos);
                    self.modal_open = false;
                    self.url.clear();
                    // A finished download has no engine state left to resume:
                    // spawning would wipe the file and re-download everything
                    // from byte 0. Completed rows are left alone.
                    if !running && !finished {
                        if let Some(item) = self.downloads.get_mut(pos) {
                            item.external = false;
                            item.retry_count = 0;
                            item.retry_scheduled = false;
                        }
                        self.spawn_run(pos, CancellationToken::new());
                    }
                    return Task::none();
                }
                let name = dest
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| filename_from_url(&url));
                self.downloads.push(DownloadItem {
                    url: url.clone(),
                    dest,
                    name,
                    ..DownloadItem::default()
                });
                let i = self.downloads.len() - 1;
                self.selected = Some(i);
                self.modal_open = false;
                self.url.clear();
                self.spawn_run(i, CancellationToken::new());
                self.persist();
                Task::none()
            }
            Message::Tick => {
                let mut tasks: Vec<Task<Message>> = Vec::new();
                // The main window id is needed for any tray command; capture
                // it once instead of carrying an id through every message.
                if self.window_id.is_none() {
                    tasks.push(iced::window::latest().map(Message::GotWindowId));
                }
                // Tray requests (window show, quit) arrive out of band.
                tasks.extend(tray::take_command().map(|cmd| match cmd {
                    tray::TrayCmd::Show => {
                        self.confirm_quit = false;
                        self.window_id
                            .map(|id| iced::window::set_mode(id, Mode::Windowed))
                            .unwrap_or_else(Task::none)
                    }
                    tray::TrayCmd::Quit => {
                        self.persist();
                        self.quit_task()
                    }
                }));
                let retry = self.refresh();
                self.scan_counter = self.scan_counter.wrapping_add(1);
                if self.scan_counter % 8 == 1 {
                    self.scan_state_files();
                }
                if let Some(retry) = retry {
                    tasks.push(retry);
                }
                Task::batch(tasks)
            }
            Message::GotWindowId(id) => {
                self.window_id = id;
                Task::none()
            }
            Message::WinClose => {
                // X button pressed: ask before leaving (XDM-style close dialog).
                self.confirm_quit = true;
                Task::none()
            }
            Message::HideToTray => {
                self.confirm_quit = false;
                self.window_id
                    .map(|id| iced::window::set_mode(id, Mode::Hidden))
                    .unwrap_or_else(Task::none)
            }
            Message::Quit => {
                self.persist();
                self.quit_task()
            }
            Message::CancelQuit => {
                self.confirm_quit = false;
                Task::none()
            }
            Message::Select(i) => {
                self.selected = Some(i);
                Task::none()
            }
            Message::Category(c) => {
                self.category = c;
                Task::none()
            }
            Message::Pause(i) => {
                // An external download is driven by another process; we can
                // only observe it, not pause it.
                if let Some(item) = self.downloads.get_mut(i)
                    && !item.external
                {
                    if let Some(cancel) = &item.cancel {
                        cancel.cancel();
                    }
                    item.retry_scheduled = false;
                }
                Task::none()
            }
            Message::Resume(i) => {
                // Only Paused/Failed rows can be resumed. A Finished row has no
                // engine state left: spawning would re-download the whole file
                // from byte 0 and overwrite the completed download.
                let state = self.downloads.get(i).map(|it| it.state);
                if matches!(state, Some(State::Paused) | Some(State::Failed)) {
                    if let Some(item) = self.downloads.get_mut(i) {
                        item.external = false;
                        item.retry_count = 0;
                        item.retry_scheduled = false;
                    }
                    self.spawn_run(i, CancellationToken::new());
                }
                Task::none()
            }
            Message::Remove(i) => {
                if let Some(item) = self.downloads.get(i)
                    && let Some(cancel) = &item.cancel
                {
                    cancel.cancel();
                }
                if self.selected == Some(i) {
                    self.selected = None;
                }
                let item = self.downloads.remove(i);
                // The engine's state file may outlive this removal; remember
                // the destination so the scanner cannot resurrect the row.
                self.removed.insert(item.dest.to_string_lossy().into_owned());
                self.persist();
                Task::none()
            }
            Message::OpenFolder(i) => {
                if let Some(item) = self.downloads.get(i) {
                    let dir = item
                        .dest
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("."));
                    rt().spawn(async move {
                        let _ = tokio::process::Command::new("xdg-open")
                            .arg(dir)
                            .status()
                            .await;
                    });
                }
                Task::none()
            }
            Message::AutoRetry(i) => {
                if let Some(item) = self.downloads.get_mut(i)
                    && item.retry_scheduled
                {
                    item.retry_scheduled = false;
                    self.spawn_run(i, CancellationToken::new());
                }
                Task::none()
            }
        }
    }

    /// Starts (or resumes) the engine for item `i`. Resume works because the
    /// engine persists per-chunk state and reloads it in `run`.
    fn spawn_run(&mut self, i: usize, cancel: CancellationToken) {
        let Some(item) = self.downloads.get_mut(i) else {
            return;
        };
        let (tx, rx) = watch::channel(Progress::default());
        *item.rx_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
        item.status.store(st::RUNNING, Ordering::Relaxed);
        item.state = State::Running;
        item.error_text = None;
        *item.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        item.cancel = Some(cancel.clone());
        item.speed_bps = 0.0;
        item.prev_downloaded = 0;
        item.last_tick = None;

        let url = item.url.clone();
        let dest = item.dest.clone();
        let status = item.status.clone();
        let error = item.error.clone();
        rt().spawn(async move {
            let result = download(&url, dest, DownloadConfig::default(), cancel, tx).await;
            match result {
                Ok(_) => status.store(st::FINISHED, Ordering::Relaxed),
                Err(DownloadError::Cancelled) => status.store(st::PAUSED, Ordering::Relaxed),
                Err(e) => {
                    *error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.to_string());
                    status.store(st::FAILED, Ordering::Relaxed);
                }
            }
        });
    }

    /// Actually exits the application: `window::close` terminates the process
    /// (bypassing the close-request interception, which would otherwise pop
    /// the dialog again). Abrupt engine shutdown is safe — the state file
    /// survives and the download resumes on the next launch.
    fn quit_task(&self) -> Task<Message> {
        self.window_id
            .map(iced::window::close)
            .unwrap_or_else(|| {
                std::process::exit(0);
            })
    }

    fn refresh(&mut self) -> Option<Task<Message>> {
        let mut scheduled: Option<Task<Message>> = None;
        for (i, item) in self.downloads.iter_mut().enumerate() {
            if let Some(rx) = item.rx_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
                item.progress = *rx.borrow();
            }
            item.state = match item.status.load(Ordering::Relaxed) {
                st::RUNNING => State::Running,
                st::PAUSED => State::Paused,
                st::FINISHED => State::Finished,
                _ => State::Failed,
            };
            if item.state == State::Failed && item.error_text.is_none() {
                item.error_text = item.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            }

            let now = Instant::now();
            if item.state == State::Running && !item.external {
                // Making progress: the auto-retry budget resets.
                if item.progress.downloaded > item.prev_downloaded {
                    item.retry_count = 0;
                }
                item.sample_progress(now);
            } else if !(item.state == State::Running && item.external) {
                // Externally-driven running rows are sampled by the scanner
                // instead; everything else has no speed to show.
                item.speed_bps = 0.0;
                item.prev_downloaded = item.progress.downloaded;
                item.last_tick = Some(now);
            }

            // Auto-retry unexpected engine failures with backoff (XDM-style
            // self-healing). User pauses and externally-driven rows are left
            // alone.
            if item.state == State::Failed
                && !item.external
                && !item.retry_scheduled
                && item.retry_count < MAX_AUTO_RETRIES
            {
                item.retry_count += 1;
                item.retry_scheduled = true;
                let attempt = item.retry_count;
                scheduled = Some(Task::perform(
                    tokio::time::sleep(retry_delay(attempt)),
                    move |_| Message::AutoRetry(i),
                ));
            }
        }
        scheduled
    }

    /// Watches the downloads directory for engine state files written by other
    /// processes (CLI runs, a previous GUI session). New files appear as
    /// external rows; vanished files become `Finished` (or are dropped). Files
    /// whose destination is already tracked are skipped.
    /// Writes the current rows + removed set to disk (best effort; a failed
    /// save must never take the UI down).
    fn persist(&self) {
        let list = SavedList {
            downloads: self.downloads.iter().map(DownloadItem::saved).collect(),
            removed: self.removed.iter().cloned().collect(),
        };
        let _ = write_list(&list_path(), &list);
    }

    fn scan_state_files(&mut self) {
        let dir = downloads_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };

        let mut found: Vec<(PathBuf, DownloadState, bool)> = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(base) = state_file_base(&name) else {
                continue;
            };
            let dest = dir.join(base);
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<DownloadState>(&bytes) else {
                continue;
            };
            let fresh = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(is_fresh)
                .unwrap_or(false);
            found.push((dest, state, fresh));
        }

        // Refresh progress of already-tracked external items.
        for item in &mut self.downloads {
            if !item.external {
                continue;
            }
            if let Some((_, state, fresh)) = found.iter().find(|(dest, _, _)| *dest == item.dest) {
                item.progress.downloaded = state.chunks.iter().map(|c| c.downloaded).sum();
                item.progress.total = state.total;
                item.state = if *fresh { State::Running } else { State::Paused };
                if *fresh {
                    item.sample_progress(Instant::now());
                }
            }
        }

        // Track newly discovered state files, except destinations the user
        // explicitly removed from the list.
        for (dest, state, fresh) in found {
            if self.downloads.iter().any(|it| it.dest == dest) {
                continue;
            }
            if self.removed.contains(dest.to_string_lossy().as_ref()) {
                continue;
            }
            let name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "download".to_owned());
            self.downloads.push(DownloadItem {
                url: state.url,
                dest,
                name,
                progress: Progress {
                    downloaded: state.chunks.iter().map(|c| c.downloaded).sum(),
                    total: state.total,
                },
                state: if fresh { State::Running } else { State::Paused },
                external: true,
                ..DownloadItem::default()
            });
        }

        // Drop external items whose state file vanished (finished and cleaned
        // up, or the destination was deleted by hand).
        self.downloads.retain_mut(|item| {
            if !item.external {
                return true;
            }
            let state_path = state_file_path(&dir, &item.name);
            if state_path.exists() {
                return true;
            }
            if item.dest.exists() {
                item.state = State::Finished;
                true
            } else {
                false
            }
        });

        // A removed destination is only memorable while its engine state file
        // still lives; once that file is gone there is nothing to re-enter.
        self.removed.retain(|dest| {
            let p = Path::new(dest);
            p.parent().is_some_and(|dir| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                name.is_some_and(|n| state_file_path(dir, &n).exists())
            })
        });

        // Snapshot after every classification pass (~2 s) so a crash or kill
        // loses at most that much state.
        self.persist();
    }

    fn category_shows(&self, item: &DownloadItem, cat: Category) -> bool {
        match cat {
            Category::All => true,
            Category::Completed => item.state == State::Finished,
            _ => category_for(&item.name) == cat,
        }
    }

    fn count(&self, cat: Category) -> usize {
        self.downloads
            .iter()
            .filter(|it| self.category_shows(it, cat))
            .count()
    }

    fn view(&self) -> Element<'_, Message> {
        let layout = column![
            self.toolbar(),
            row![self.sidebar(), self.content()]
                .width(Length::Fill)
                .height(Length::Fill),
            self.status_bar(),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        if self.confirm_quit {
            stack![layout, self.modal_backdrop(Message::CancelQuit), self.quit_card()]
                .into()
        } else if self.modal_open {
            stack![layout, self.modal_backdrop(Message::CancelModal), self.modal_card()].into()
        } else {
            layout.into()
        }
    }

    fn toolbar(&self) -> Element<'_, Message> {
        let sel = self.selected;
        let sel_state = sel.and_then(|i| self.downloads.get(i)).map(|it| it.state);
        let can_resume = matches!(sel_state, Some(State::Paused) | Some(State::Failed));
        let can_pause = sel_state == Some(State::Running);
        let can_target = sel.is_some();

        let divider = || {
            container("").width(1).height(22).style(|_| container::Style {
                background: Some(palette::DIALOG_BORDER.into()),
                ..container::Style::default()
            })
        };

        let bar = row![
            tool_btn("New", true, Message::NewDownload),
            divider(),
            tool_btn("Resume", can_resume, Message::Resume(sel.unwrap_or(0))),
            tool_btn("Pause", can_pause, Message::Pause(sel.unwrap_or(0))),
            divider(),
            tool_btn("Remove", can_target, Message::Remove(sel.unwrap_or(0))),
            tool_btn("Folder", can_target, Message::OpenFolder(sel.unwrap_or(0))),
        ]
        .padding([6, 10])
        .spacing(2)
        .align_y(Alignment::Center);

        column![
            container(bar)
                .width(Length::Fill)
                .style(|_| container::Style {
                    background: Some(palette::TOOLBAR.into()),
                    ..container::Style::default()
                }),
            rule::horizontal(1.0).style(|_| rule::Style {
                color: palette::CARD_BORDER,
                radius: 0.0.into(),
                fill_mode: rule::FillMode::Full,
                snap: true,
            }),
        ]
        .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let cats = [
            Category::All,
            Category::Videos,
            Category::Music,
            Category::Programs,
            Category::Compressed,
            Category::Documents,
            Category::Others,
            Category::Completed,
        ];
        let items: Vec<Element<'_, Message>> = cats.iter().map(|c| self.side_item(*c)).collect();

        container(column(items).spacing(2))
            .width(196)
            .height(Length::Fill)
            .padding([14, 8])
            .style(|_| container::Style {
                background: Some(palette::SIDEBAR.into()),
                ..container::Style::default()
            })
            .into()
    }

    fn side_item(&self, cat: Category) -> Element<'_, Message> {
        let active = self.category == cat;
        let count = self.count(cat);

        let content = row![
            container("").width(9).height(9).style(move |_| container::Style {
                background: Some(cat.dot().into()),
                border: iced::Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: 5.0.into(),
                },
                ..container::Style::default()
            }),
            text(cat.label()).size(13),
            Space::new().width(Length::Fill),
            text(count.to_string()).size(11),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .width(Length::Fill);

        button(container(content).padding([7, 10]))
            .width(Length::Fill)
            .on_press(Message::Category(cat))
            .style(move |_: &Theme, status| {
                let bg = if active {
                    Some(palette::SIDEBAR_ACTIVE.into())
                } else if matches!(status, button::Status::Hovered) {
                    Some(palette::SIDEBAR_HOVER.into())
                } else {
                    None
                };
                button::Style {
                    background: bg,
                    text_color: palette::TEXT,
                    border: iced::Border {
                        color: Color::TRANSPARENT,
                        width: 0.0,
                        radius: 6.0.into(),
                    },
                    ..button::Style::default()
                }
            })
            .into()
    }

    fn content(&self) -> Element<'_, Message> {
        // Java XDM separates flat white rows with a 1 px hairline instead of
        // spacing.
        let mut stack: Vec<Element<'_, Message>> = Vec::new();
        let visible: Vec<(usize, &DownloadItem)> = self
            .downloads
            .iter()
            .enumerate()
            .filter(|(_, it)| self.category_shows(it, self.category))
            .collect();
        let last = visible.len().saturating_sub(1);
        for (pos, (i, it)) in visible.into_iter().enumerate() {
            stack.push(self.download_row(i, it));
            if pos != last {
                stack.push(
                    container("")
                        .width(Length::Fill)
                        .height(1)
                        .style(|_| container::Style {
                            background: Some(palette::CARD_BORDER.into()),
                            ..container::Style::default()
                        })
                        .into(),
                );
            }
        }

        let body: Element<'_, Message> = if stack.is_empty() {
            container(
                column![
                    text("Nothing here yet").size(16).color(palette::TEXT_DIM),
                    text("Click  + New  to start a download")
                        .size(12)
                        .color(palette::TEXT_DIM),
                ]
                .spacing(6)
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into()
        } else {
            container(
                scrollable(column(stack))
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(16)
            .into()
        };

        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(palette::CONTENT.into()),
                ..container::Style::default()
            })
            .into()
    }

    fn download_row<'a>(&'a self, i: usize, item: &'a DownloadItem) -> Element<'a, Message> {
        let selected = self.selected == Some(i);
        let cat = category_for(&item.name);
        let pct = match item.progress.total {
            Some(total) if total > 0 => (item.progress.downloaded as f32 / total as f32).min(1.0),
            _ => 0.0,
        };

        let size_line = match item.progress.total {
            Some(total) => format!(
                "{} of {} ({:.1}%)",
                fmt_size(item.progress.downloaded),
                fmt_size(total),
                pct * 100.0
            ),
            None => format!("{} downloaded", fmt_size(item.progress.downloaded)),
        };

        let retrying = item.state == State::Failed && !item.external && item.retry_scheduled;
        let (status_label, status_color) = match item.state {
            State::Running if item.external => ("Downloading (ext)".to_owned(), palette::ACCENT),
            State::Running => ("Downloading".to_owned(), palette::ACCENT),
            State::Paused => ("Paused".to_owned(), palette::AMBER),
            State::Finished => ("Completed".to_owned(), palette::GREEN),
            State::Failed if retrying => (
                format!("Retrying ({}/{}…)", item.retry_count, MAX_AUTO_RETRIES),
                palette::AMBER,
            ),
            State::Failed => ("Failed".to_owned(), palette::RED),
        };

        let bar_color = match item.state {
            State::Running => palette::ACCENT,
            State::Paused => palette::AMBER,
            // Java XDM paints progress bars in the cyan accent throughout.
            State::Finished => palette::ACCENT,
            State::Failed if retrying => palette::AMBER,
            State::Failed => palette::RED,
        };

        let live = item.state == State::Running;
        let speed_text = if live { fmt_speed(item.speed_bps) } else { String::new() };
        let eta = if live {
            item.progress
                .total
                .and_then(|total| eta_text(item.progress.downloaded, total, item.speed_bps))
                .map(|e| format!("{e} left"))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let badge = container(
            text(badge_label(&item.name))
                .size(10)
                .color(palette::WHITE),
        )
        .width(44)
        .height(44)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_| container::Style {
            background: Some(cat.badge().into()),
            border: iced::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        });

        let mut info = column![
            text(item.name.as_str())
                .size(14)
                .color(
                    if selected {
                        palette::WHITE
                    } else {
                        palette::TEXT
                    }
                )
                .font(Font {
                    weight: Weight::Semibold,
                    ..Font::default()
                }),
            container(progress_bar(0.0..=1.0, pct).style(move |_| progress_bar::Style {
                background: palette::TRACK.into(),
                bar: bar_color.into(),
                border: iced::Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: 3.0.into(),
                },
            }))
            .height(6)
            .width(Length::Fill),
            row![
                text(size_line).size(11).color(
                    if selected {
                        palette::WHITE
                    } else {
                        palette::TEXT_DIM
                    }
                ),
                Space::new().width(Length::Fill),
                text(speed_text).size(11).color(
                    if selected {
                        palette::WHITE
                    } else {
                        palette::TEXT_DIM
                    }
                ),
                text(eta).size(11).color(
                    if selected {
                        palette::WHITE
                    } else {
                        palette::TEXT_DIM
                    }
                ),
            ]
            .spacing(10)
            .width(Length::Fill),
        ]
        .spacing(5)
        .width(Length::Fill);

        if item.state == State::Failed
            && !retrying
            && let Some(err) = &item.error_text
        {
            info = info.push(text(err.as_str()).size(11).color(palette::RED));
        }

        let side = column![text(status_label).size(12).color(
            if selected {
                palette::WHITE
            } else {
                status_color
            }
        )]
        .width(96)
        .align_x(Alignment::End);

        // Java XDM rows are flat white with a hairline divider underneath;
        // the selection is a solid accent block with white text.
        let card = container(
            row![badge, info, side]
                .spacing(14)
                .align_y(Alignment::Center)
                .width(Length::Fill),
        )
        .width(Length::Fill)
        .padding([10, 14])
        .style(move |_| container::Style {
            background: Some(
                if selected {
                    palette::ROW_SELECTED
                } else {
                    palette::CARD
                }
                .into(),
            ),
            border: iced::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        });

        mouse_area(card).on_press(Message::Select(i)).into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        let active = self
            .downloads
            .iter()
            .filter(|it| it.state == State::Running)
            .count();
        let total_speed: f64 = self
            .downloads
            .iter()
            .filter(|it| it.state == State::Running)
            .map(|it| it.speed_bps)
            .sum();
        let right = if active > 0 {
            format!("{active} active • {} total", fmt_speed(total_speed))
        } else {
            "idle".to_owned()
        };

        container(
            row![
                text(format!("{} items", self.downloads.len()))
                    .size(11)
                    .color(palette::TEXT_DIM),
                Space::new().width(Length::Fill),
                text(right).size(11).color(palette::TEXT_DIM),
            ]
            .spacing(10)
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .padding([5, 14])
        .style(|_| container::Style {
            background: Some(palette::CARD.into()),
            border: iced::Border {
                color: palette::CARD_BORDER,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        })
        .into()
    }

    fn modal_backdrop(&self, on_press: Message) -> Element<'_, Message> {
        mouse_area(
            container("")
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()),
                    ..container::Style::default()
                }),
        )
        .on_press(on_press)
        .into()
    }

    /// X-button close dialog: minimize to tray or really quit, XDM-style.
    fn quit_card(&self) -> Element<'_, Message> {
        container(
            column![
                text("Exit Xtreme Download Manager Rust?")
                    .size(17)
                    .color(palette::DIALOG_TEXT),
                text("Keep running hidden in the tray, or quit completely?")
                    .size(13)
                    .color(palette::DIALOG_DIM),
                row![
                    Space::new().width(Length::Fill),
                    button(text("Minimize to tray").size(13).color(palette::TEXT))
                        .padding([7, 16])
                        .style(|_: &Theme, _| button::Style {
                            background: Some(palette::ACCENT.into()),
                            text_color: palette::TEXT,
                            border: iced::Border {
                                color: Color::TRANSPARENT,
                                width: 0.0,
                                radius: 4.0.into(),
                            },
                            ..button::Style::default()
                        })
                        .on_press(Message::HideToTray),
                    button(text("Quit").size(13).color(palette::DIALOG_TEXT))
                        .padding([7, 16])
                        .style(|_: &Theme, status| {
                            let bg = matches!(status, button::Status::Hovered)
                                .then_some(palette::DIALOG_BORDER)
                                .map(iced::Background::from);
                            button::Style {
                                background: bg,
                                text_color: palette::DIALOG_TEXT,
                                border: iced::Border {
                                    color: palette::DIALOG_BORDER,
                                    width: 1.0,
                                    radius: 4.0.into(),
                                },
                                ..button::Style::default()
                            }
                        })
                        .on_press(Message::Quit),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(12),
        )
        .width(460)
        .padding(22)
        .style(|_| container::Style {
            background: Some(palette::DIALOG_BG.into()),
            border: iced::Border {
                color: palette::DIALOG_BORDER,
                width: 1.0,
                radius: 0.0.into(),
            },
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.25),
                offset: iced::Vector::new(0.0, 6.0),
                blur_radius: 24.0,
            },
            ..container::Style::default()
        })
        .into()
    }

    fn modal_card(&self) -> Element<'_, Message> {
        let input_style = move |_: &Theme, _: text_input::Status| text_input::Style {
            background: palette::DIALOG_INPUT.into(),
            border: iced::Border {
                color: palette::DIALOG_BORDER,
                width: 1.0,
                radius: 4.0.into(),
            },
            icon: palette::DIALOG_DIM,
            placeholder: palette::DIALOG_DIM,
            value: palette::DIALOG_TEXT,
            selection: iced::Color::from_rgba8(0x33, 0xB5, 0xE5, 0.35),
        };

        container(
            column![
                text("New download").size(17).color(palette::DIALOG_TEXT),
                text_input("https://example.com/file.zip", &self.url)
                    .on_input(Message::UrlInput)
                    .on_submit(Message::Add)
                    .size(13)
                    .style(input_style),
                text_input("destination folder (empty = ~/Downloads)", &self.dest)
                    .on_input(Message::DestInput)
                    .on_submit(Message::Add)
                    .size(13)
                    .style(input_style),
                row![
                    Space::new().width(Length::Fill),
                    button(text("Cancel").size(13).color(palette::DIALOG_DIM))
                        .padding([7, 16])
                        .style(|_: &Theme, status| {
                            let bg = matches!(status, button::Status::Hovered)
                                .then_some(palette::DIALOG_BORDER)
                                .map(iced::Background::from);
                            button::Style {
                                background: bg,
                                text_color: palette::DIALOG_TEXT,
                                border: iced::Border {
                                    color: palette::DIALOG_BORDER,
                                    width: 1.0,
                                    radius: 4.0.into(),
                                },
                                ..button::Style::default()
                            }
                        })
                        .on_press(Message::CancelModal),
                    button(text("Download").size(13).color(palette::TEXT))
                        .padding([7, 16])
                        .style(|_: &Theme, _| button::Style {
                            background: Some(palette::ACCENT.into()),
                            text_color: palette::TEXT,
                            border: iced::Border {
                                color: Color::TRANSPARENT,
                                width: 0.0,
                                radius: 4.0.into(),
                            },
                            ..button::Style::default()
                        })
                        .on_press(Message::Add),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(12),
        )
        .width(540)
        .padding(22)
        .style(|_| container::Style {
            background: Some(palette::DIALOG_BG.into()),
            border: iced::Border {
                color: palette::DIALOG_BORDER,
                width: 1.0,
                radius: 0.0.into(),
            },
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.25),
                offset: iced::Vector::new(0.0, 6.0),
                blur_radius: 24.0,
            },
            ..container::Style::default()
        })
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick),
            // Close requests (X button) as messages: exit_on_close_request is
            // disabled, so the runtime forwards them to the app instead.
            iced::window::close_requests().map(|_| Message::WinClose),
        ])
    }
}

/// Toolbar button: plain text label, grayed out when disabled.
fn tool_btn(label: &'static str, enabled: bool, msg: Message) -> Element<'static, Message> {
    button(text(label).size(13))
        .padding([6, 14])
        .on_press_maybe(enabled.then_some(msg))
        .style(move |_: &Theme, status| {
            let bg = match status {
                button::Status::Hovered if enabled => Some(palette::TOOLBAR_HOVER.into()),
                button::Status::Pressed if enabled => Some(palette::TOOLBAR_PRESSED.into()),
                _ => None,
            };
            let fg = if enabled {
                palette::DIALOG_TEXT
            } else {
                palette::TEXT_DISABLED
            };
            button::Style {
                background: bg,
                text_color: fg,
                border: iced::Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: 6.0.into(),
                },
                ..button::Style::default()
            }
        })
        .into()
}

/// Resolves the destination from the optional user input, expanding `~/`
/// and treating a trailing `/` as "into this folder".
fn destination(url: &str, dest_input: &str) -> PathBuf {
    let home = || std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    let input = dest_input.trim();

    if input.is_empty() {
        return PathBuf::from(home()).join("Downloads").join(filename_from_url(url));
    }

    let expanded = if let Some(rest) = input.strip_prefix("~/") {
        PathBuf::from(home()).join(rest)
    } else {
        PathBuf::from(input)
    };
    if input.ends_with('/') {
        expanded.join(filename_from_url(url))
    } else {
        expanded
    }
}

fn filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download");
    if name.is_empty() {
        "download".to_owned()
    } else {
        name.to_owned()
    }
}

/// The default directory watched for engine state files.
fn downloads_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned())).join("Downloads")
}

/// Persisted list entry: everything the GUI needs to redraw a row after a
/// restart. Progress is the last observed snapshot; the folder scanner
/// corrects it against state files (and marks finished downloads) later.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedEntry {
    url: String,
    dest: String,
    name: String,
    downloaded: u64,
    total: Option<u64>,
}

/// The whole on-disk list. `removed` holds destinations the user deleted whose
/// engine state files may still exist — those must not re-appear as
/// externally-driven rows on the next start.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedList {
    #[serde(default)]
    downloads: Vec<SavedEntry>,
    #[serde(default)]
    removed: Vec<String>,
}

/// Where the GUI list lives (XDM keeps data under ~/.local/share).
fn list_path() -> PathBuf {
    let data = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned()))
                .join(".local/share")
        });
    data.join("xdm").join("gui-list.json")
}

/// Serializes the list atomically (write + rename so a crash cannot corrupt it).
fn write_list(path: &Path, list: &SavedList) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let Ok(json) = serde_json::to_string_pretty(list) else {
        return false;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        std::fs::rename(&tmp, path).is_ok()
    } else {
        false
    }
}

fn read_list(path: &Path) -> Option<SavedList> {
    let bytes = std::fs::read(path).ok()?;
    // Ignore new unknown fields from a future version: serde default.
    serde_json::from_slice(&bytes).ok()
}

/// Strips `.{name}.xdm-state.json` down to `{name}`; `None` for other files.
fn state_file_base(file_name: &str) -> Option<&str> {
    let rest = file_name.strip_prefix('.')?;
    let base = rest.strip_suffix(".xdm-state.json")?;
    (!base.is_empty()).then_some(base)
}

/// Path of the state file for a destination name within `dir`.
fn state_file_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!(".{name}.xdm-state.json"))
}

/// A state file modified within the last 10 s counts as actively downloading.
fn is_fresh(modified: SystemTime) -> Option<bool> {
    modified.elapsed().ok().map(|age| age < Duration::from_secs(10))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_defaults_to_downloads_with_url_filename() {
        let d = destination("https://example.com/path/file.zip?query=1", "");
        assert!(d.ends_with("Downloads/file.zip"));
    }

    #[test]
    fn destination_expands_home_and_trailing_slash() {
        // Edition 2024: set_var is unsafe (single-threaded test, fine).
        unsafe {
            std::env::set_var("HOME", "/tmp/xdm-test-home");
        }
        let d = destination("https://example.com/file.bin", "~/stuff/");
        assert_eq!(d, PathBuf::from("/tmp/xdm-test-home/stuff/file.bin"));
        let d = destination("https://example.com/file.bin", "~/other.iso");
        assert_eq!(d, PathBuf::from("/tmp/xdm-test-home/other.iso"));
    }

    #[test]
    fn categories_match_xdm_groups() {
        assert_eq!(category_for("movie.mkv"), Category::Videos);
        assert_eq!(category_for("MOVIE.MKV"), Category::Videos);
        assert_eq!(category_for("song.mp3"), Category::Music);
        assert_eq!(category_for("setup.exe"), Category::Programs);
        assert_eq!(category_for("game.pkg"), Category::Programs);
        assert_eq!(category_for("backup.zip"), Category::Compressed);
        assert_eq!(category_for("doc.pdf"), Category::Documents);
        assert_eq!(category_for("data.xyz"), Category::Others);
        assert_eq!(category_for("noext"), Category::Others);
    }

    #[test]
    fn badge_labels_use_extension() {
        assert_eq!(badge_label("file.mkv"), "MKV");
        assert_eq!(badge_label("a.b.c"), "C");
        assert_eq!(badge_label("noext"), "FILE");
    }

    #[test]
    fn sizes_format_like_xdm() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(2048), "2.0 KB");
        assert_eq!(fmt_size(5_242_880), "5.0 MB");
        assert_eq!(fmt_size(45_156_007_936), "42.05 GB");
    }

    #[test]
    fn speeds_and_etas_format() {
        assert_eq!(fmt_speed(0.0), "0 B/s");
        assert_eq!(fmt_speed(850.0), "850 B/s");
        assert_eq!(fmt_speed(2_500_000.0), "2.4 MB/s");
        assert_eq!(fmt_eta(0), "<1s");
        assert_eq!(fmt_eta(42), "42s");
        assert_eq!(fmt_eta(272), "4m 32s");
        assert_eq!(fmt_eta(4320), "1h 12m");
    }

    #[test]
    fn eta_needs_progress_and_speed() {
        assert_eq!(eta_text(0, 100, 10.0), Some("10s".to_owned()));
        assert_eq!(eta_text(0, 100, 0.0), None);
        assert_eq!(eta_text(100, 100, 10.0), None);
    }

    #[test]
    fn state_file_names_round_trip() {
        assert_eq!(state_file_base(".file.zip.xdm-state.json"), Some("file.zip"));
        assert_eq!(
            state_file_base(".%5BSPSX%5D-x.pkg.xdm-state.json"),
            Some("%5BSPSX%5D-x.pkg")
        );
        assert_eq!(state_file_base("plain.txt"), None);
        assert_eq!(state_file_base(".xdm-state.json"), None);
    }

    #[test]
    fn fresh_means_recently_modified() {
        let now = SystemTime::now();
        assert_eq!(is_fresh(now - Duration::from_secs(1)), Some(true));
        assert_eq!(is_fresh(now - Duration::from_secs(60)), Some(false));
    }

    #[test]
    fn retry_backoff_grows_and_caps() {
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(4), Duration::from_secs(8));
        assert_eq!(retry_delay(20), Duration::from_secs(30));
    }

    #[test]
    fn tray_icon_is_arbg32_disc_with_arrow() {
        let icon = tray::icon();
        assert_eq!((icon.width, icon.height), (32, 32));
        assert_eq!(icon.data.len(), 32 * 32 * 4);

        // Pixel 0 (corner): outside the disc, fully transparent.
        assert_eq!(&icon.data[..4], &[0, 0, 0, 0]);
        // A disc point away from the arrow: dark background.
        let bg = (5 * 4) + 16 * 4 * 32;
        assert_eq!(&icon.data[bg..bg + 4], &[255, 0x0E, 0x14, 0x19]);
        // Arrowhead pixel and stem pixel: cyan.
        let arrow = (9 * 4) + 8 * 4 * 32;
        assert_eq!(&icon.data[arrow..arrow + 4], &[255, 0x33, 0xB5, 0xE5]);
        let stem = (12 * 4) + 20 * 4 * 32;
        assert_eq!(&icon.data[stem..stem + 4], &[255, 0x33, 0xB5, 0xE5]);
    }

    #[test]
    fn list_roundtrips_and_tolerates_bad_files() {
        let path = std::env::temp_dir().join(format!("xdm-gui-list-{}.json", std::process::id()));
        let entries = SavedEntry {
            url: "https://example.com/d/a.pkg".to_owned(),
            dest: "/tmp/a.pkg".to_owned(),
            name: "a.pkg".to_owned(),
            downloaded: 5,
            total: Some(10),
        };
        let list = SavedList {
            downloads: vec![entries],
            removed: vec![String::from("/tmp/dead.bin")],
        };
        assert!(write_list(&path, &list));
        assert_eq!(read_list(&path), Some(list));
        // A future version's file (unknown fields) still reads back.
        let _ = std::fs::write(&path, r#"{"downloads": [], "removed": [], "extra": 1}"#);
        assert_eq!(read_list(&path), Some(SavedList::default()));
        // Corrupt files simply yield nothing; the UI must not panic.
        let _ = std::fs::write(&path, "not json");
        assert_eq!(read_list(&path), None);
        let _ = std::fs::remove_file(&path);
    }
}
