//! HLS/DASH streaming downloads.
//!
//! Port of `StreamingDownloaderTask` + `HlsDownloaderTask` + `DashDownloaderTask`
//! (subset). Segments download to a temp directory with bounded concurrency,
//! AES-128 HLS segments are decrypted in place, and the result is assembled by
//! concatenation (see [`assemble`]) — no external tools.
//!
//! Resume: per-segment state is persisted to `.<name>.xdm-stream.json`
//! alongside the destination; on resume the manifest is re-fetched, the plan
//! must match the saved one, and done segments (with their files on disk) are
//! skipped.

pub mod assemble;
pub mod dash;
pub mod hls;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::RANGE;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::http::{DownloadConfig, DownloadError, MAX_ATTEMPTS, Progress, backoff};

const USER_AGENT: &str = concat!("xdm-rs/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How often segment completion is flushed to the state file.
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Hls,
    Dash,
}

/// Persisted streaming resume state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamState {
    pub url: String,
    pub kind: StreamKind,
    pub output_ext: String,
    pub init_url: Option<String>,
    pub init_done: bool,
    pub segments: Vec<SegmentState>,
    /// Fetched AES-128 keys (key URL → raw key bytes).
    pub keys: Vec<KeyState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentState {
    pub url: String,
    pub done: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyState {
    pub url: String,
    pub key: Vec<u8>,
}

/// Immutable plan data for one segment.
struct SegmentPlanItem {
    url: String,
    range: Option<(u64, u64)>,
    key_idx: Option<usize>,
    /// Resolved IV (explicit from the playlist, or derived from the sequence).
    iv: Option<[u8; 16]>,
}

/// Runtime form of a segment with live completion tracking.
struct SegmentJob {
    url: String,
    range: Option<(u64, u64)>,
    key_idx: Option<usize>,
    iv: Option<[u8; 16]>,
    done: AtomicBool,
    size: AtomicU64,
}

/// The full download plan, rebuilt from the (re-fetched) manifest every run.
struct StreamPlan {
    init: Option<(String, Option<(u64, u64)>)>,
    items: Vec<SegmentPlanItem>,
    key_urls: Vec<String>,
    output_ext: String,
}

pub struct StreamingDownload {
    client: Client,
    url: String,
    kind: StreamKind,
    dest: PathBuf,
    state_path: PathBuf,
    seg_dir: PathBuf,
    max_connections: usize,
}

impl StreamingDownload {
    pub fn hls(url: impl Into<String>, dest: impl Into<PathBuf>, config: DownloadConfig) -> Result<Self, DownloadError> {
        Self::new(url, dest, StreamKind::Hls, config)
    }

    pub fn dash(url: impl Into<String>, dest: impl Into<PathBuf>, config: DownloadConfig) -> Result<Self, DownloadError> {
        Self::new(url, dest, StreamKind::Dash, config)
    }

    fn new(url: impl Into<String>, dest: impl Into<PathBuf>, kind: StreamKind, config: DownloadConfig) -> Result<Self, DownloadError> {
        let dest = dest.into();
        let stem = dest.file_name().and_then(|n| n.to_str()).unwrap_or("download").to_owned();
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        let state_path = parent.join(format!(".{stem}.xdm-stream.json"));
        let seg_dir = parent.join(format!(".{stem}.xdm-stream"));
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(DownloadError::Http)?;
        Ok(Self {
            client,
            url: url.into(),
            kind,
            dest,
            state_path,
            seg_dir,
            max_connections: config.max_chunks,
        })
    }

    /// Runs the streaming download to completion.
    ///
    /// Same contract as [`crate::http::HttpDownload::run`]: progress on
    /// `progress`, cancel via `cancel`. Returns the total bytes written.
    pub async fn run(
        self,
        cancel: CancellationToken,
        progress: watch::Sender<Progress>,
    ) -> Result<Option<u64>, DownloadError> {
        let internal = cancel.child_token();
        let saved: Option<StreamState> = self.load_state().await;

        // Rebuild the plan from the (re-fetched) manifest.
        let plan = match self.kind {
            StreamKind::Hls => self.build_hls_plan(&internal).await?,
            StreamKind::Dash => self.build_dash_plan(&internal).await?,
        };

        let jobs: Arc<Vec<Arc<SegmentJob>>> = Arc::new(
            plan.items
                .iter()
                .map(|j| {
                    Arc::new(SegmentJob {
                        url: j.url.clone(),
                        range: j.range,
                        key_idx: j.key_idx,
                        iv: j.iv,
                        done: AtomicBool::new(false),
                        size: AtomicU64::new(0),
                    })
                })
                .collect(),
        );

        // Reconcile: the saved plan must describe the same download, else we
        // start fresh (the old temp segments are worthless).
        let total_downloaded = Arc::new(AtomicU64::new(0));
        let mut init_done = plan.init.is_none();
        let mut keys: Vec<KeyState> = Vec::new();
        let saved_matched = saved.as_ref().filter(|s| {
            s.url == self.url
                && s.kind == self.kind
                && s.segments.len() == plan.items.len()
                && s.segments
                    .iter()
                    .zip(plan.items.iter())
                    .all(|(s, j)| s.url == j.url)
                && s.init_url == plan.init.as_ref().map(|(u, _)| u.clone())
        });

        if let Some(saved) = saved_matched {
            for (i, (job, ss)) in jobs.iter().zip(&saved.segments).enumerate() {
                if !ss.done {
                    continue;
                }
                let path = self.seg_dir.join(format!("seg-{i:05}"));
                if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                    job.done.store(true, Ordering::Relaxed);
                    job.size.store(size, Ordering::Relaxed);
                    total_downloaded.fetch_add(size, Ordering::Relaxed);
                }
            }
            if let Some((_, _)) = &plan.init {
                let init_path = self.seg_dir.join("init");
                if saved.init_done && tokio::fs::try_exists(&init_path).await.unwrap_or(false) {
                    let size = tokio::fs::metadata(&init_path).await.map(|m| m.len()).unwrap_or(0);
                    total_downloaded.fetch_add(size, Ordering::Relaxed);
                    init_done = true;
                }
            }
            keys = saved.keys.clone();
        } else {
            let _ = tokio::fs::remove_dir_all(&self.seg_dir).await;
        }
        tokio::fs::create_dir_all(&self.seg_dir).await?;

        let _ = progress.send(Progress {
            downloaded: total_downloaded.load(Ordering::Relaxed),
            total: None,
        });

        // Init segment (fMP4 EXT-X-MAP / DASH initialization); never encrypted.
        if !init_done
            && let Some((init_url, init_range)) = &plan.init
        {
            let n = fetch_to_file(&self.client, init_url, *init_range, &self.seg_dir.join("init"), &internal).await?;
            total_downloaded.fetch_add(n, Ordering::Relaxed);
            init_done = true;
        }

        // Fetch AES keys (reusing saved ones).
        for url in &plan.key_urls {
            if keys.iter().any(|k| &k.url == url) {
                continue;
            }
            let raw = fetch_bytes(&self.client, url, &internal).await?;
            if raw.len() != 16 {
                return Err(DownloadError::Decrypt(format!(
                    "key {url} is {} bytes, expected 16",
                    raw.len()
                )));
            }
            keys.push(KeyState { url: url.clone(), key: raw.to_vec() });
        }

        // Periodic state persister (crash-safety window: one save interval).
        let persister = {
            let state_path = self.state_path.clone();
            let url = self.url.clone();
            let kind = self.kind;
            let output_ext = plan.output_ext.clone();
            let init_url = plan.init.as_ref().map(|(u, _)| u.clone());
            let jobs = jobs.clone();
            let keys = keys.clone();
            let cancel_p = internal.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(STATE_SAVE_INTERVAL);
                interval.tick().await; // first tick fires immediately; skip it
                loop {
                    tokio::select! {
                        _ = interval.tick() => {}
                        _ = cancel_p.cancelled() => break,
                    }
                    let state = StreamState {
                        url: url.clone(),
                        kind,
                        output_ext: output_ext.clone(),
                        init_url: init_url.clone(),
                        init_done,
                        segments: jobs
                            .iter()
                            .map(|j| SegmentState {
                                url: j.url.clone(),
                                done: j.done.load(Ordering::Relaxed),
                                size: j.size.load(Ordering::Relaxed),
                            })
                            .collect(),
                        keys: keys.clone(),
                    };
                    save_state(&state_path, &state).await;
                }
            })
        };

        // Segment workers over a shared index counter.
        let next = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let error_slot: Arc<Mutex<Option<DownloadError>>> = Arc::new(Mutex::new(None));
        let keys_arc = Arc::new(keys.clone());

        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        let workers = self.max_connections.max(1).min(jobs.len().max(1));
        for _ in 0..workers {
            let client = self.client.clone();
            let jobs = jobs.clone();
            let next = next.clone();
            let failed = failed.clone();
            let error_slot = error_slot.clone();
            let internal = internal.clone();
            let seg_dir = self.seg_dir.clone();
            let keys = keys_arc.clone();
            let total = total_downloaded.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    if internal.is_cancelled() || failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        return;
                    }
                    let job = &jobs[i];
                    if job.done.load(Ordering::Relaxed) {
                        continue;
                    }
                    let path = seg_dir.join(format!("seg-{i:05}"));
                    match fetch_to_file(&client, &job.url, job.range, &path, &internal).await {
                        Ok(n) => {
                            if let (Some(ki), Some(iv)) = (job.key_idx, job.iv) {
                                let key = keys[ki].key.clone();
                                if let Err(e) = decrypt_file(&path, &key, &iv).await {
                                    error_slot
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .get_or_insert(DownloadError::Decrypt(format!("segment {i}: {e}")));
                                    failed.store(true, Ordering::Relaxed);
                                    return;
                                }
                            }
                            job.size.store(n, Ordering::Relaxed);
                            job.done.store(true, Ordering::Relaxed);
                            total.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(e) => {
                            if matches!(e, DownloadError::Cancelled) {
                                return;
                            }
                            error_slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get_or_insert(e);
                            failed.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }));
        }

        // Progress ticker.
        let ticker = {
            let counter = total_downloaded.clone();
            let sender = progress.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(PROGRESS_TICK);
                loop {
                    interval.tick().await;
                    let _ = sender.send(Progress {
                        downloaded: counter.load(Ordering::Relaxed),
                        total: None,
                    });
                }
            })
        };

        for handle in handles {
            let _ = handle.await;
        }
        ticker.abort();
        persister.abort();

        let outcome = if failed.load(Ordering::Relaxed) {
            error_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or_else(|| DownloadError::ExhaustedRetries {
                    attempts: MAX_ATTEMPTS,
                    last_error: "segment failed".to_owned(),
                })
        } else if cancel.is_cancelled() {
            DownloadError::Cancelled
        } else {
            // All segments done: assemble and publish.
            let mut parts = Vec::new();
            if plan.init.is_some() {
                parts.push(self.seg_dir.join("init"));
            }
            for (i, job) in jobs.iter().enumerate() {
                if !job.done.load(Ordering::Relaxed) {
                    return Err(DownloadError::Manifest(format!("segment {i} did not complete")));
                }
                parts.push(self.seg_dir.join(format!("seg-{i:05}")));
            }
            let part_file = self.dest.with_file_name(format!(
                ".{}.xdm-part",
                self.dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
            ));
            assemble::concatenate(&parts, &part_file).await?;
            tokio::fs::rename(&part_file, &self.dest).await?;
            let _ = tokio::fs::remove_dir_all(&self.seg_dir).await;
            let _ = tokio::fs::remove_file(&self.state_path).await;
            return Ok(Some(total_downloaded.load(Ordering::Relaxed)));
        };

        // Persist state for a later resume.
        let state = StreamState {
            url: self.url.clone(),
            kind: self.kind,
            output_ext: plan.output_ext.clone(),
            init_url: plan.init.as_ref().map(|(u, _)| u.clone()),
            init_done,
            segments: jobs
                .iter()
                .map(|j| SegmentState {
                    url: j.url.clone(),
                    done: j.done.load(Ordering::Relaxed),
                    size: j.size.load(Ordering::Relaxed),
                })
                .collect(),
            keys,
        };
        save_state(&self.state_path, &state).await;
        Err(outcome)
    }

    async fn load_state(&self) -> Option<StreamState> {
        let bytes = tokio::fs::read(&self.state_path).await.ok()?;
        let state: StreamState = serde_json::from_slice(&bytes).ok()?;
        (state.url == self.url).then_some(state)
    }

    async fn build_hls_plan(&self, internal: &CancellationToken) -> Result<StreamPlan, DownloadError> {
        let base = url::Url::parse(&self.url).map_err(|e| DownloadError::Manifest(format!("url: {e}")))?;
        let text = fetch_text(&self.client, &self.url, internal).await?;

        let (playlist_url, media_text) = if hls::is_master_playlist(&text) {
            let variants = hls::parse_master(&text, &base).map_err(DownloadError::Manifest)?;
            let best = variants
                .iter()
                .max_by_key(|v| v.bandwidth)
                .ok_or_else(|| DownloadError::Manifest("master playlist has no variants".into()))?;
            let variant_url =
                url::Url::parse(&best.url).map_err(|e| DownloadError::Manifest(format!("url: {e}")))?;
            let variant_text = fetch_text(&self.client, &best.url, internal).await?;
            (variant_url, variant_text)
        } else {
            (base, text)
        };

        let pl = hls::parse_media(&media_text, &playlist_url).map_err(DownloadError::Manifest)?;
        if pl.segments.is_empty() {
            return Err(DownloadError::Manifest("media playlist has no segments".into()));
        }

        // Dedupe key URLs; resolve each segment's key index and IV.
        let mut key_urls: Vec<String> = Vec::new();
        let mut items = Vec::with_capacity(pl.segments.len());
        for (i, seg) in pl.segments.iter().enumerate() {
            let key_idx = match &seg.key {
                None => None,
                Some(k) => {
                    let idx = match key_urls.iter().position(|u| u == &k.url) {
                        Some(idx) => idx,
                        None => {
                            key_urls.push(k.url.clone());
                            key_urls.len() - 1
                        }
                    };
                    Some(idx)
                }
            };
            let iv = seg
                .key
                .as_ref()
                .map(|k| k.iv.unwrap_or_else(|| hls::default_iv(pl.media_sequence + i as u64)));
            items.push(SegmentPlanItem {
                url: seg.url.clone(),
                range: seg.byte_range,
                key_idx,
                iv,
            });
        }

        let output_ext = match &pl.init_segment {
            Some(_) => "mp4".to_owned(),
            None => pl
                .segments
                .first()
                .map(|s| assemble::ext_from_url(&s.url))
                .unwrap_or_else(|| "ts".to_owned()),
        };

        Ok(StreamPlan { init: pl.init_segment, items, key_urls, output_ext })
    }

    async fn build_dash_plan(&self, internal: &CancellationToken) -> Result<StreamPlan, DownloadError> {
        let base = url::Url::parse(&self.url).map_err(|e| DownloadError::Manifest(format!("url: {e}")))?;
        let text = fetch_text(&self.client, &self.url, internal).await?;
        let repr = dash::parse_mpd(&text, &base).map_err(DownloadError::Manifest)?;

        let ext = if repr.mime.contains("webm") { "webm" } else { "mp4" }.to_owned();

        let items = repr
            .segments
            .iter()
            .map(|u| SegmentPlanItem { url: u.clone(), range: None, key_idx: None, iv: None })
            .collect();

        Ok(StreamPlan {
            init: repr.init_url.map(|u| (u, None)),
            items,
            key_urls: Vec::new(),
            output_ext: ext,
        })
    }
}

async fn fetch_text(client: &Client, url: &str, internal: &CancellationToken) -> Result<String, DownloadError> {
    let bytes = fetch_bytes(client, url, internal).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn fetch_bytes(client: &Client, url: &str, internal: &CancellationToken) -> Result<Vec<u8>, DownloadError> {
    let mut attempt: u32 = 0;
    let mut last_error;
    loop {
        if internal.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        attempt += 1;
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(b) => return Ok(b.to_vec()),
                        Err(e) => last_error = e.to_string(),
                    }
                } else {
                    last_error = format!("unexpected status {status}");
                }
            }
            Err(e) => last_error = e.to_string(),
        }
        if attempt >= MAX_ATTEMPTS {
            return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
}

/// GETs `url` (optionally a byte range) streaming into `path`; returns bytes written.
async fn fetch_to_file(
    client: &Client,
    url: &str,
    range: Option<(u64, u64)>,
    path: &Path,
    internal: &CancellationToken,
) -> Result<u64, DownloadError> {
    let mut attempt: u32 = 0;
    let mut last_error;
    loop {
        if internal.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        attempt += 1;

        let mut req = client.get(url);
        if let Some((off, len)) = range {
            req = req.header(RANGE, format!("bytes={off}-{}", off + len - 1));
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_error = e.to_string();
                if attempt >= MAX_ATTEMPTS {
                    return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
                }
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        let status = resp.status();
        if status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT {
            let mut file = tokio::fs::File::create(path).await?;
            let mut written: u64 = 0;
            let mut stream_error: Option<String> = None;
            loop {
                if internal.is_cancelled() {
                    return Err(DownloadError::Cancelled);
                }
                match resp.chunk().await {
                    Ok(Some(bytes)) => {
                        file.write_all(&bytes).await?;
                        written += bytes.len() as u64;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        stream_error = Some(e.to_string());
                        break;
                    }
                }
            }
            let _ = file.flush().await;
            let _ = file.sync_all().await;
            match stream_error {
                None => return Ok(written),
                Some(e) => {
                    last_error = e;
                    if attempt >= MAX_ATTEMPTS {
                        return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            }
        }
        last_error = format!("unexpected status {status}");
        if attempt >= MAX_ATTEMPTS {
            return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
}

async fn decrypt_file(path: &Path, key: &[u8], iv: &[u8]) -> Result<(), String> {
    let data = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    let plain = hls::decrypt_aes128_cbc(key, iv, &data)?;
    tokio::fs::write(path, plain).await.map_err(|e| e.to_string())
}

/// Generic atomic state write: tmp file + fsync + rename.
async fn save_state<S: Serialize>(path: &Path, state: &S) {
    let Ok(json) = serde_json::to_vec_pretty(state) else {
        tracing::warn!("state serialization failed");
        return;
    };
    let tmp = path.with_extension("json.tmp");
    let path = path.to_path_buf();
    if let Err(e) = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })
    .await
    {
        tracing::warn!(error = %e, "failed to persist download state");
    }
}
