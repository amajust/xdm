//! iced GUI slice: add downloads, watch live progress, pause/resume/remove.
//!
//! Architecture: the engine runs on a dedicated tokio runtime ([`rt`]); the
//! GUI polls each download's `watch::Receiver` on a 250 ms tick subscription
//! and mirrors state into plain view snapshots.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use iced::widget::{button, column, container, progress_bar, row, scrollable, text, text_input};
use iced::{Element, Subscription, Task, Theme};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xdm_core::http::{DownloadError, HttpDownload, Progress};

const MIB: f64 = 1024.0 * 1024.0;

/// The engine needs a real tokio runtime (tokio::fs, tokio::spawn, timers);
/// iced's own executor does not provide one, so we own it explicitly.
static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

pub fn run() -> iced::Result {
    iced::application(Gui::default, Gui::update, Gui::view)
        .subscription(Gui::subscription)
        .theme(|_: &Gui| Theme::Dark)
        .run()
}

#[derive(Debug, Clone)]
pub enum Message {
    UrlInput(String),
    DestInput(String),
    Add,
    Tick,
    Pause(usize),
    Resume(usize),
    Remove(usize),
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

#[derive(Default)]
struct DownloadItem {
    url: String,
    dest: PathBuf,
    /// Receiver of the current (or last) run; replaced on resume.
    rx_slot: Arc<Mutex<Option<watch::Receiver<Progress>>>>,
    status: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    cancel: Option<CancellationToken>,
    // View snapshot, refreshed on Tick.
    progress: Progress,
    state: State,
    error_text: Option<String>,
}

#[derive(Default)]
struct Gui {
    url: String,
    dest: String,
    downloads: Vec<DownloadItem>,
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
            Message::Add => {
                let url = self.url.trim().to_owned();
                if url.is_empty() {
                    return Task::none();
                }
                let dest = destination(&url, &self.dest);
                self.downloads.push(DownloadItem {
                    url: url.clone(),
                    dest,
                    ..DownloadItem::default()
                });
                let i = self.downloads.len() - 1;
                self.spawn_run(i, CancellationToken::new());
                self.url.clear();
                Task::none()
            }
            Message::Tick => {
                self.refresh();
                Task::none()
            }
            Message::Pause(i) => {
                if let Some(item) = self.downloads.get(i) {
                    if let Some(cancel) = &item.cancel {
                        cancel.cancel();
                    }
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
                if let Some(item) = self.downloads.get(i) {
                    if let Some(cancel) = &item.cancel {
                        cancel.cancel();
                    }
                }
                self.downloads.remove(i);
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
        *item.rx_slot.lock().unwrap() = Some(rx);
        item.status.store(st::RUNNING, Ordering::Relaxed);
        item.state = State::Running;
        item.error_text = None;
        *item.error.lock().unwrap() = None;
        item.cancel = Some(cancel.clone());

        let url = item.url.clone();
        let dest = item.dest.clone();
        let status = item.status.clone();
        let error = item.error.clone();
        rt().spawn(async move {
            let result = HttpDownload::new(url, dest).run(cancel, tx).await;
            match result {
                Ok(_) => status.store(st::FINISHED, Ordering::Relaxed),
                Err(DownloadError::Cancelled) => status.store(st::PAUSED, Ordering::Relaxed),
                Err(e) => {
                    *error.lock().unwrap() = Some(e.to_string());
                    status.store(st::FAILED, Ordering::Relaxed);
                }
            }
        });
    }

    fn refresh(&mut self) {
        for item in &mut self.downloads {
            if let Some(rx) = item.rx_slot.lock().unwrap().as_ref() {
                item.progress = *rx.borrow();
            }
            item.state = match item.status.load(Ordering::Relaxed) {
                st::RUNNING => State::Running,
                st::PAUSED => State::Paused,
                st::FINISHED => State::Finished,
                _ => State::Failed,
            };
            if item.state == State::Failed && item.error_text.is_none() {
                item.error_text = item.error.lock().unwrap().clone();
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let add_row = row![
            text_input("https://example.com/file.zip", &self.url)
                .on_input(Message::UrlInput)
                .on_submit(Message::Add)
                .size(14),
            text_input("destination (empty = ~/Downloads)", &self.dest)
                .on_input(Message::DestInput)
                .size(14),
            button(text("Add").size(13))
                .on_press(Message::Add)
                .padding(8),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let list: Vec<Element<Message>> = self
            .downloads
            .iter()
            .enumerate()
            .map(|(i, item)| item_view(i, item))
            .collect();

        container(
            column![
                add_row,
                scrollable(column(list).spacing(6)).height(iced::Fill),
            ]
            .spacing(12),
        )
        .padding(16)
        .width(iced::Fill)
        .height(iced::Fill)
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick)
    }
}

fn item_view(i: usize, item: &DownloadItem) -> Element<'_, Message> {
    let pct = match item.progress.total {
        Some(total) if total > 0 => item.progress.downloaded as f32 / total as f32,
        _ => 0.0,
    };
    let state_label = match item.state {
        State::Running => "running",
        State::Paused => "paused",
        State::Finished => "done",
        State::Failed => "failed",
    };
    let bytes = match item.progress.total {
        Some(total) => format!(
            "{:.1} / {:.1} MiB",
            item.progress.downloaded as f64 / MIB,
            total as f64 / MIB
        ),
        None => format!("{} bytes", item.progress.downloaded),
    };
    let error_line = item.error_text.clone().unwrap_or_default();

    let action = match item.state {
        State::Running => button(text("Pause").size(12))
            .on_press(Message::Pause(i))
            .padding(6),
        State::Paused => button(text("Resume").size(12))
            .on_press(Message::Resume(i))
            .padding(6),
        _ => button(text(state_label).size(12)).padding(6),
    };

    row![
        column![
            row![text(item.url.as_str()).size(13), text(state_label).size(11)]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            progress_bar(0.0..=1.0, pct),
            row![text(bytes).size(11), text(error_line).size(11)].spacing(8),
        ]
        .spacing(3)
        .width(iced::Fill),
        action,
        button(text("Remove").size(12))
            .on_press(Message::Remove(i))
            .padding(6),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center)
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
}
