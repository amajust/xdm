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
use std::time::{Duration, Instant};

use iced::font::{Font, Weight};
use iced::widget::{
    button, column, container, mouse_area, progress_bar, rule, row, scrollable, space::Space,
    stack, text, text_input,
};
use iced::{Alignment, Color, Element, Length, Subscription, Task, Theme};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xdm_core::engine::download;
use xdm_core::http::{DownloadConfig, DownloadError, Progress};

/// Material palette close to the Java XDM look.
mod palette {
    use iced::Color;

    pub const SIDEBAR: Color = Color::from_rgb8(0x2B, 0x39, 0x45);
    pub const SIDEBAR_HOVER: Color = Color::from_rgb8(0x36, 0x46, 0x54);
    pub const ACCENT: Color = Color::from_rgb8(0x21, 0x96, 0xF3);
    pub const CARD: Color = Color::from_rgb8(0xFF, 0xFF, 0xFF);
    pub const CARD_BORDER: Color = Color::from_rgb8(0xE4, 0xE7, 0xEB);
    pub const CONTENT: Color = Color::from_rgb8(0xF4, 0xF5, 0xF7);
    pub const ROW_SELECTED: Color = Color::from_rgb8(0xE3, 0xF2, 0xFD);
    pub const TEXT: Color = Color::from_rgb8(0x21, 0x25, 0x29);
    pub const TEXT_DIM: Color = Color::from_rgb8(0x6E, 0x76, 0x81);
    pub const TEXT_DISABLED: Color = Color::from_rgb8(0xB6, 0xBC, 0xC4);
    pub const TRACK: Color = Color::from_rgb8(0xE3, 0xE6, 0xEA);
    pub const GREEN: Color = Color::from_rgb8(0x43, 0xA0, 0x47);
    pub const AMBER: Color = Color::from_rgb8(0xFB, 0x8C, 0x00);
    pub const RED: Color = Color::from_rgb8(0xE5, 0x39, 0x35);
    pub const WHITE: Color = Color::from_rgb8(0xFF, 0xFF, 0xFF);
}

/// The engine needs a real tokio runtime (tokio::fs, tokio::spawn, timers);
/// iced's own executor does not provide one, so we own it explicitly.
static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

pub fn run() -> iced::Result {
    iced::application(Gui::default, Gui::update, Gui::view)
        .title(|_: &Gui| "Xtreme Download Manager".to_owned())
        .window(iced::window::Settings {
            size: (1020, 640).into(),
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
}

#[derive(Default)]
struct Gui {
    url: String,
    dest: String,
    downloads: Vec<DownloadItem>,
    selected: Option<usize>,
    category: Category,
    modal_open: bool,
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
                Task::none()
            }
            Message::Tick => {
                self.refresh();
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
                if let Some(item) = self.downloads.get(i)
                    && let Some(cancel) = &item.cancel
                {
                    cancel.cancel();
                }
                Task::none()
            }
            Message::Resume(i) => {
                let running = self
                    .downloads
                    .get(i)
                    .is_some_and(|it| it.state == State::Running);
                if !running {
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
                self.downloads.remove(i);
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

    fn refresh(&mut self) {
        for item in &mut self.downloads {
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
            if item.state == State::Running {
                match item.last_tick {
                    Some(last) => {
                        let dt = now.duration_since(last).as_secs_f64();
                        if dt >= 0.05 {
                            let instant = (item.progress.downloaded.saturating_sub(item.prev_downloaded)) as f64 / dt;
                            item.speed_bps = if item.speed_bps <= 0.0 {
                                instant
                            } else {
                                item.speed_bps * 0.6 + instant * 0.4
                            };
                            item.prev_downloaded = item.progress.downloaded;
                            item.last_tick = Some(now);
                        }
                    }
                    None => {
                        item.prev_downloaded = item.progress.downloaded;
                        item.last_tick = Some(now);
                    }
                }
            } else {
                item.speed_bps = 0.0;
                item.prev_downloaded = item.progress.downloaded;
                item.last_tick = Some(now);
            }
        }
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

        if self.modal_open {
            stack![layout, self.modal_backdrop(), self.modal_card()].into()
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
            rule::vertical(24.0).style(|_| rule::Style {
                color: palette::CARD_BORDER,
                radius: 0.0.into(),
                fill_mode: rule::FillMode::Full,
                snap: true,
            })
        };

        let bar = row![
            tool_btn("+", "New", true, Message::NewDownload),
            divider(),
            tool_btn("▶", "Resume", can_resume, Message::Resume(sel.unwrap_or(0))),
            tool_btn("‖", "Pause", can_pause, Message::Pause(sel.unwrap_or(0))),
            divider(),
            tool_btn("✕", "Remove", can_target, Message::Remove(sel.unwrap_or(0))),
            tool_btn("⌂", "Folder", can_target, Message::OpenFolder(sel.unwrap_or(0))),
        ]
        .padding([6, 10])
        .spacing(2)
        .align_y(Alignment::Center);

        column![
            container(bar)
                .width(Length::Fill)
                .style(|_| container::Style {
                    background: Some(palette::CARD.into()),
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
                    Some(palette::ACCENT.into())
                } else if matches!(status, button::Status::Hovered) {
                    Some(palette::SIDEBAR_HOVER.into())
                } else {
                    None
                };
                button::Style {
                    background: bg,
                    text_color: palette::WHITE,
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
        let rows: Vec<Element<'_, Message>> = self
            .downloads
            .iter()
            .enumerate()
            .filter(|(_, it)| self.category_shows(it, self.category))
            .map(|(i, it)| self.download_row(i, it))
            .collect();

        let body: Element<'_, Message> = if rows.is_empty() {
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
                scrollable(column(rows).spacing(10))
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

        let (status_label, status_color) = match item.state {
            State::Running => ("Downloading", palette::ACCENT),
            State::Paused => ("Paused", palette::AMBER),
            State::Finished => ("Completed", palette::GREEN),
            State::Failed => ("Failed", palette::RED),
        };

        let bar_color = match item.state {
            State::Running => palette::ACCENT,
            State::Paused => palette::AMBER,
            State::Finished => palette::GREEN,
            State::Failed => palette::RED,
        };

        let speed_text = if item.state == State::Running {
            fmt_speed(item.speed_bps)
        } else {
            String::new()
        };
        let eta = if item.state == State::Running {
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
                .color(palette::TEXT)
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
                text(size_line).size(11).color(palette::TEXT_DIM),
                Space::new().width(Length::Fill),
                text(speed_text).size(11).color(palette::TEXT_DIM),
                text(eta).size(11).color(palette::TEXT_DIM),
            ]
            .spacing(10)
            .width(Length::Fill),
        ]
        .spacing(5)
        .width(Length::Fill);

        if item.state == State::Failed
            && let Some(err) = &item.error_text
        {
            info = info.push(text(err.as_str()).size(11).color(palette::RED));
        }

        let side = column![text(status_label).size(12).color(status_color)]
            .width(96)
            .align_x(Alignment::End);

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
                color: if selected {
                    palette::ACCENT
                } else {
                    palette::CARD_BORDER
                },
                width: 1.0,
                radius: 8.0.into(),
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

    fn modal_backdrop(&self) -> Element<'_, Message> {
        mouse_area(
            container("")
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()),
                    ..container::Style::default()
                }),
        )
        .on_press(Message::CancelModal)
        .into()
    }

    fn modal_card(&self) -> Element<'_, Message> {
        container(
            column![
                text("New download").size(17).color(palette::TEXT),
                text_input("https://example.com/file.zip", &self.url)
                    .on_input(Message::UrlInput)
                    .on_submit(Message::Add)
                    .size(13),
                text_input("destination folder (empty = ~/Downloads)", &self.dest)
                    .on_input(Message::DestInput)
                    .on_submit(Message::Add)
                    .size(13),
                row![
                    Space::new().width(Length::Fill),
                    button(text("Cancel").size(13))
                        .padding([7, 16])
                        .style(|_: &Theme, status| {
                            let bg = matches!(status, button::Status::Hovered)
                                .then_some(Color::from_rgb8(0xF0, 0xF1, 0xF3))
                                .map(iced::Background::from);
                            button::Style {
                                background: bg,
                                text_color: palette::TEXT_DIM,
                                border: iced::Border {
                                    color: palette::CARD_BORDER,
                                    width: 1.0,
                                    radius: 6.0.into(),
                                },
                                ..button::Style::default()
                            }
                        })
                        .on_press(Message::CancelModal),
                    button(text("Download").size(13).color(palette::WHITE))
                        .padding([7, 16])
                        .style(|_: &Theme, _| button::Style {
                            background: Some(palette::ACCENT.into()),
                            text_color: palette::WHITE,
                            border: iced::Border {
                                color: Color::TRANSPARENT,
                                width: 0.0,
                                radius: 6.0.into(),
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
            background: Some(palette::CARD.into()),
            border: iced::Border {
                color: palette::CARD_BORDER,
                width: 1.0,
                radius: 10.0.into(),
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
        iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick)
    }
}

/// Toolbar button: symbol over label, grayed out when disabled.
fn tool_btn(
    symbol: &'static str,
    label: &'static str,
    enabled: bool,
    msg: Message,
) -> Element<'static, Message> {
    let content = column![text(symbol).size(16), text(label).size(10)]
        .spacing(2)
        .align_x(Alignment::Center);

    button(container(content).width(62).padding([4, 0]))
        .padding(0)
        .on_press_maybe(enabled.then_some(msg))
        .style(move |_: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Some(Color::from_rgb8(0xF0, 0xF1, 0xF3)),
                button::Status::Pressed => Some(Color::from_rgb8(0xE4, 0xE6, 0xEA)),
                _ => None,
            }
            .map(iced::Background::from);
            let fg = if enabled {
                palette::TEXT
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
}
