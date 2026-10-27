//! Browser extension compatibility: the HTTP server the v9
//! "XDM Browser Monitor" extension talks to.
//!
//! Port of xdm-app's `BrowserIntegration.kt` + `VideoHelper.kt`
//! (v9.0.2-pre.7). There is no native messaging in v9: the extension
//! connects to `http://127.0.0.1:8597`, long-polls `/poll` for state, and
//! POSTs one JSON `ExtensionMessage` per event (`/download`, `/media`,
//! `/vid`, `/batch`, ...). Every reply is a fresh `ConfigDto` snapshot, so
//! the extension converges after any event; parked `/poll`s wake through the
//! state `version`.
//!
//! Video capture ports the parts of `VideoHelper` that matter:
//! content-type / plain-extension detection, manifest handling (one item per
//! manifest — our engine parses HLS/DASH itself), duplicate-manifest and
//! fragment-referer suppression, and XDM's MIME→extension map for output
//! names.
//!
//! Security mirrors the Kotlin server: an `Origin` header outside the
//! extension schemes is rejected with 403, and state-changing paths must be
//! POST (405). Requests *without* an `Origin` stay open so `/sync` pings
//! (single-instance probes, health checks, scripts) keep working.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// The fixed extension port (connector.js `APP_BASE_URL`).
pub const PORT: u16 = 8597;

/// Origin schemes allowed to talk to the server (extension pages).
const ORIGIN_SCHEMES: [&str; 4] = [
    "chrome-extension://",
    "moz-extension://",
    "extension://",
    "safari-web-extension://",
];

/// Paths that mutate state must be POSTed; `GET /sync` stays open for
/// Origin-less pings (single-instance probes, plain HTTP checks).
const STATE_CHANGING: [&str; 9] = [
    "/download", "/media", "/vid", "/batch", "/clear", "/clear-tab", "/tab-update", "/show", "/poll",
];

/// How long a parked `/poll` waits for a change before answering 204 (the
/// extension immediately re-polls). Also bounds every read, so a stuck peer
/// cannot pin a task forever.
const PARK: Duration = Duration::from_secs(30);

/// A request line + headers beyond this is garbage.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// 5000 batch items × a few hundred bytes each stay well inside this.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Java's manifest-refetch protection: a manifest captured for the same tab
/// inside this window is skipped, and player/fragment suppressions decay on
/// the same clock so the capture bookkeeping never grows forever.
const MEDIA_WINDOW_MS: u64 = 30_000;

/// Java `config.minVideoSize` default: 1024 KB.
const MIN_VIDEO_SIZE_KB: u64 = 1024;

/// Java `defFileExtensions` (AppConfig.kt): URLs or names with these
/// extensions are taken over by the browser.
const FILE_EXTS: [&str; 53] = [
    "3GP", "7Z", "AVI", "BZ2", "DEB", "DOC", "DOCX", "EXE", "ISO", "DMG", "MSI", "PDF", "PPT",
    "PPTX", "RAR", "RPM", "XLS", "XLSX", "TAR", "JAR", "ZIP", "XZ", "PKG", "GZ", "TGZ", "ZST",
    "CAB", "APK", "XAPK", "APPIMAGE", "MSIX", "BIN", "IMG", "EPUB", "ODT", "ODS", "ODP", "MP4",
    "MKV", "WEBM", "MOV", "M4V", "WMV", "FLV", "MPG", "MPEG", "MP3", "M4A", "AAC", "FLAC", "WAV",
    "OGG", "OPUS",
];

/// Java `defVideoExtensions` — reported as `requestFileExts`: media URLs the
/// extension watches for even without a video Content-Type.
const REQUEST_FILE_EXTS: [&str; 23] = [
    "MP4", "M3U8", "F4M", "WEBM", "OGG", "MP3", "AAC", "FLV", "MKV", "DIVX", "MOV", "MPG", "MPEG",
    "OPUS", "MPD", "M4V", "M4A", "OGV", "OGA", "WAV", "FLAC", "3GP", "AVI",
];

/// Java `mimeList` — media Content-Types the extension flags.
const MEDIA_TYPES: [&str; 4] = ["audio/", "video/", "mpeg", "dash"];

/// Java `defBlockedHosts` — hosts the extension leaves to the browser.
const BLOCKED_HOSTS: [&str; 3] = ["update.microsoft.com", "windowsupdate.com", "thwawte.com"];

/// Plain media extensions taken as video when a response has no
/// Content-Type (Java `plainMediaExts`).
const PLAIN_MEDIA_EXTS: [&str; 20] = [
    "mp4", "m4v", "m4a", "mkv", "mov", "3gp", "avi", "divx", "wmv", "mpg", "mpeg", "flv", "ogv",
    "ogg", "oga", "opus", "mp3", "aac", "wav", "flac",
];

// ---------------------------------------------------------------------------
// GUI command channel
// ---------------------------------------------------------------------------

/// One thing the server decided the GUI should do, queued for the next tick.
#[derive(Debug, Clone)]
pub enum MonitorCmd {
    /// A download to add to the list (and start): `/download`, a captured
    /// video (`/vid`), or a `/batch` item.
    AddDownload(DownloadCandidate),
    /// `/show`: raise (or reopen) the main window.
    ShowWindow,
}

/// A fully resolved download request: everything the GUI needs to spawn the
/// engine without further lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadCandidate {
    pub url: String,
    /// Pre-resolved, sanitized file name.
    pub name: String,
    /// Size from the capture (shown before the engine's probe reports).
    pub total: Option<u64>,
    /// Replayed request headers (cookie, referer, UA, ...).
    pub headers: Vec<(String, String)>,
}

fn slot() -> &'static Mutex<Vec<MonitorCmd>> {
    static SLOT: OnceLock<Mutex<Vec<MonitorCmd>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_command(cmd: MonitorCmd) {
    let mut guard = slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.push(cmd);
}

/// Drained by the GUI tick (a `/batch` may queue dozens at once).
pub fn take_commands() -> Vec<MonitorCmd> {
    let mut guard = slot().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *guard)
}

// ---------------------------------------------------------------------------
// Extension message shapes
// ---------------------------------------------------------------------------

/// `ExtensionMessage` from the extension (`request-watcher.js::
/// createRequestData` and friends). All fields optional; unknown fields
/// (like the takeover flag `download`) ignored. Header values arrive as
/// arrays of primitives (Kotlin wraps them in a lenient `SafeStr`), so they
/// are deserialized as `serde_json::Value` and stringified through
/// [`primitive_to_string`].
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ExtMsg {
    url: Option<String>,
    cookie: Option<String>,
    request_headers: Option<HashMap<String, Vec<serde_json::Value>>>,
    response_headers: Option<HashMap<String, Vec<serde_json::Value>>>,
    filename: Option<String>,
    file: Option<String>,
    #[allow(dead_code)]
    method: Option<String>,
    user_agent: Option<String>,
    tab_url: Option<String>,
    tab_id: Option<String>,
    tab_title: Option<String>,
    referer: Option<String>,
    file_size: Option<i64>,
    mime_type: Option<String>,
    /// Tolerant field: the extension sends it stringified (`vid: id + ""`).
    vid: Option<serde_json::Value>,
}

/// `POST /poll` body: who asks, and what they have already seen.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PollReq {
    #[allow(dead_code)]
    client_id: Option<String>,
    version: Option<serde_json::Value>,
}

/// `POST /batch` body (`docs/bulk-download.md`): page context, then groups,
/// each with its own cookie jar (`groups[].cookie`) and item list.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BatchMsg {
    #[allow(dead_code)]
    tab_url: Option<String>,
    #[allow(dead_code)]
    tab_title: Option<String>,
    user_agent: Option<String>,
    referer: Option<String>,
    groups: Vec<BatchGroup>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BatchGroup {
    cookie: Option<String>,
    items: Vec<BatchItem>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BatchItem {
    url: String,
    filename: Option<String>,
    mime_type: Option<String>,
    file_size: Option<i64>,
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

/// A detected video, exactly what the popup shows (`VideoItem` in the
/// snapshot).
#[derive(Debug, Clone)]
struct VideoItem {
    id: u64,
    url: String,
    /// XDM's display/download name (often page-title derived, sanitized).
    name: String,
    ext: String,
    /// Popup `info`: for plain videos `[ext] <size>` like XDM, `HLS`/`DASH`
    /// for manifests.
    info: String,
    tab_id: String,
    tab_url: String,
    tab_title: String,
    /// True when the name took the tab title (no filename/file in the
    /// message), so a later `/tab-update` renames the item.
    title_named: bool,
    /// Known total (plain videos), never for manifests.
    size: Option<u64>,
    /// Replayed capture headers for the eventual download.
    headers: Vec<(String, String)>,
}

/// Server-side state. Guarded by a plain `Mutex`: mutation handlers do no
/// awaits while holding it; `/poll` parking happens on the `watch` channel
/// AFTER its lock scope has ended.
struct App {
    videos: Vec<VideoItem>,
    next_id: u64,
    /// Bumped on every videoList change; parked polls watch this.
    version: u64,
    /// Held only so the channel stays alive as long as the server does; the
    /// per-connection wake path uses clones of the side's receiver.
    version_tx: watch::Sender<u64>,
    /// Which page each tab's manifest came from: plain HTTP media fetched
    /// from the same page is background traffic of that player, not a
    /// separate video (Java `m3u8MpdTabs`).
    manifest_pages: HashMap<String, String>,
    /// (tab, url) → ms of last manifest capture, for the dedup window
    /// (Java `recentManifests`).
    recent_manifests: HashMap<(String, String), u64>,
    /// Directories where an `init.mp4` was seen → ms: fMP4 init segments and
    /// their fragment traffic (Java `suspectedMp4Fragments`).
    fragment_dirs: HashMap<String, u64>,
    /// The referer of a captured manifest → ms; media whose referer matches
    /// belongs to that player (Java `referersToSkip`).
    skip_referers: HashMap<String, u64>,
    /// Random per-run marker; the extension restarts paging when it changes.
    instance_id: String,
}

impl App {
    fn new(version_tx: watch::Sender<u64>) -> Self {
        Self {
            videos: Vec::new(),
            next_id: 1,
            version: 0,
            version_tx,
            manifest_pages: HashMap::new(),
            recent_manifests: HashMap::new(),
            fragment_dirs: HashMap::new(),
            skip_referers: HashMap::new(),
            instance_id: new_instance_id(),
        }
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// A mutation of the visible state: parked polls wake, the next reply
    /// carries the new snapshot.
    fn bump(&mut self) {
        self.version += 1;
        let _ = self.version_tx.send(self.version);
    }

    fn sync_json(&self) -> String {
        sync_json(self.videos.iter().map(video_dto).collect(), self.version, &self.instance_id)
    }
}

/// Bounded remember: first drop stale entries (older than the media window),
/// then insert; in the absurd worst case the whole table rotates rather than
/// growing forever (Java's bounded LRU maps, one tier simpler).
fn remember<K: Eq + Hash>(map: &mut HashMap<K, u64>, key: K, now: u64) {
    if map.len() >= 512 {
        map.retain(|_, t| now.saturating_sub(*t) < MEDIA_WINDOW_MS);
        if map.len() >= 512 {
            map.clear();
        }
    }
    map.insert(key, now);
}

/// Random-enough run marker without pulling a uuid dependency: process id
/// and boot-clock nanos through FNV-1a.
fn new_instance_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = u64::from(std::process::id()) ^ 0xcbf2_9ce4_8422_2325;
    for chunk in [nanos >> 32, nanos & 0xffff_ffff] {
        h ^= chunk;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// HTTP plumbing (Connection: close, one request per connection)
// ---------------------------------------------------------------------------

struct Req {
    method: String,
    path: String,
    origin: Option<String>,
    body: Vec<u8>,
}

/// Reads one request head + body. One request per connection, matching the
/// Java server's `keepAlive = false` for every reply; polls reconnect fresh
/// sockets by themselves.
async fn read_request(sock: &mut TcpStream) -> Option<Req> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    let head_len = loop {
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return None;
        }
        let n = match tokio::time::timeout(PARK, sock.read(&mut tmp)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => n,
            _ => return None,
        };
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_uppercase();
    let path_raw = parts.next()?;
    // Path minus any query string; every route is lowercase anyway.
    let path = path_raw.split(['?', '#']).next().unwrap_or(path_raw).to_ascii_lowercase();

    let mut origin = None;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "origin" {
            origin = Some(value.to_owned());
        } else if name == "content-length" {
            content_length = value.parse().ok();
        }
    }

    let mut body = buf[head_len + 4..].to_vec();
    if let Some(len) = content_length {
        if len > MAX_BODY_BYTES {
            return None;
        }
        // Read to the declared length, or give up on a stalled peer.
        while body.len() < len {
            let want = (len - body.len()).min(8192);
            let mut tmp = vec![0u8; want];
            let n = match tokio::time::timeout(PARK, sock.read(&mut tmp)).await {
                Ok(Ok(0)) => return None,
                Ok(Ok(n)) => n,
                _ => return None,
            };
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
    }

    Some(Req { method, path, origin, body })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn write_response(sock: &mut TcpStream, status: u16, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nCache-Control: max-age=0\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if sock.write_all(head.as_bytes()).await.is_ok() && !body.is_empty() {
        let _ = sock.write_all(body).await;
    }
    let _ = sock.flush().await;
    let _ = sock.shutdown().await;
}

/// The v9 contract: every 200 reply is an `ConfigDto` snapshot.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncDto<'a> {
    enabled: bool,
    file_exts: Vec<&'a str>,
    blocked_hosts: Vec<&'a str>,
    request_file_exts: Vec<&'a str>,
    media_types: Vec<&'a str>,
    matching_hosts: Vec<&'a str>,
    video_list: Vec<VideoDto<'a>>,
    version: u64,
    instance_id: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoDto<'a> {
    id: String,
    text: &'a str,
    info: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tab_id: Option<&'a str>,
}

fn video_dto(v: &VideoItem) -> VideoDto<'_> {
    VideoDto {
        id: v.id.to_string(),
        text: &v.name,
        info: &v.info,
        tab_id: if v.tab_id.is_empty() { None } else { Some(&v.tab_id) },
    }
}

fn sync_json(video_list: Vec<VideoDto<'_>>, version: u64, instance_id: &str) -> String {
    let dto = SyncDto {
        enabled: true,
        file_exts: FILE_EXTS.to_vec(),
        blocked_hosts: BLOCKED_HOSTS.to_vec(),
        request_file_exts: REQUEST_FILE_EXTS.to_vec(),
        media_types: MEDIA_TYPES.to_vec(),
        matching_hosts: Vec::new(),
        video_list,
        version,
        instance_id,
    };
    // Serialization of owned data cannot fail in practice; an empty object
    // is still a valid (empty) ConfigDto.
    serde_json::to_string(&dto).unwrap_or_else(|_| "{}".to_owned())
}

// ---------------------------------------------------------------------------
// Server loop
// ---------------------------------------------------------------------------

/// Binds the fixed port and serves; bind failure (a second GUI instance, or
/// another squatter on the port) logs and gives up — the GUI keeps working
/// without monitoring.
pub async fn serve() {
    let listener = match TcpListener::bind(("127.0.0.1", PORT)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("xdm-app: browser monitor server not started: {e}");
            return;
        }
    };
    eprintln!("xdm-app: browser monitor listening on 127.0.0.1:{PORT}");
    serve_on(listener).await;
}

/// Serves on an already bound listener (tests inject a port-0 socket).
async fn serve_on(listener: TcpListener) {
    let (version_tx, version_rx) = watch::channel(0u64);
    let app = Arc::new(Mutex::new(App::new(version_tx)));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let app = Arc::clone(&app);
                let rx = version_rx.clone();
                tokio::spawn(handle_conn(app, rx, stream));
            }
            Err(e) => {
                eprintln!("xdm-app: browser monitor: accept failed: {e}");
                return;
            }
        }
    }
}

async fn handle_conn(app: Arc<Mutex<App>>, rx: watch::Receiver<u64>, sock: TcpStream) {
    handle_conn_park(app, rx, sock, PARK).await
}

/// Park duration is injectable so tests can poll-cycle in milliseconds.
async fn handle_conn_park(
    app: Arc<Mutex<App>>,
    mut rx: watch::Receiver<u64>,
    mut sock: TcpStream,
    park: Duration,
) {
    let Some(req) = read_request(&mut sock).await else { return };
    match route(&app, &req) {
        Outcome::Poll(client_version) => {
            let status = wait_poll(&app, &mut rx, client_version, park).await;
            let body = if status == 200 {
                let guard = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.sync_json()
            } else {
                String::new()
            };
            write_response(&mut sock, status, body.as_bytes()).await;
        }
        Outcome::Snap(200) => {
            // Scope the guard: it must not be held across the write await.
            let body = {
                let guard = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.sync_json()
            };
            write_response(&mut sock, 200, body.as_bytes()).await;
        }
        Outcome::Snap(code) => write_response(&mut sock, code, b"[]").await,
    }
}

/// The long poll (Java `EventChannel.await`): a state newer than the
/// client's is answered immediately; otherwise wait for a change, bounded
/// by `park`, then answer `200` (with the fresh snapshot) or `204`.
async fn wait_poll(
    app: &Arc<Mutex<App>>,
    rx: &mut watch::Receiver<u64>,
    client_version: u64,
    park: Duration,
) -> u16 {
    {
        let current = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner).version;
        if client_version < current {
            return 200;
        }
    }
    // A dropped sender means the server is shutting down; treating that as
    // a timeout is as good a signal as Java's BYE (the extension just
    // re-polls).
    let _ = tokio::time::timeout(park, rx.wait_for(|v| *v > client_version)).await;
    let current = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner).version;
    if current > client_version { 200 } else { 204 }
}

/// Routing outcome. `Snap(200)` answers with a fresh snapshot, `Snap(code)`
/// with an empty body, `Poll(client version)` parks like the Java event
/// channel.
enum Outcome {
    Snap(u16),
    Poll(u64),
}

/// Whether a browser extension has talked to this monitor at least once
/// (a `/sync` ping or `/poll` long-poll that passed the origin check).
static EXTENSION_SEEN: AtomicBool = AtomicBool::new(false);

pub fn extension_seen() -> bool {
    EXTENSION_SEEN.load(Ordering::Relaxed)
}

fn mark_extension_seen() {
    EXTENSION_SEEN.store(true, Ordering::Relaxed);
}

fn route(app: &Arc<Mutex<App>>, req: &Req) -> Outcome {
    // Origin allowlist: browsers put the page scheme in `Origin`; only
    // extension pages (or requests without an Origin header at all) pass.
    if let Some(origin) = &req.origin {
        let allowed = ORIGIN_SCHEMES.iter().any(|s| origin.starts_with(s));
        if !allowed {
            eprintln!("xdm-app: monitor 403 for {} from origin {origin}", req.path);
            return Outcome::Snap(403);
        }
    }
    if STATE_CHANGING.contains(&req.path.as_str()) && req.method != "POST" {
        return Outcome::Snap(405);
    }
    // Any ping/long-poll that passed the origin check means a browser
    // extension is talking to us — recorded so the GUI can skip the
    // "install the extension" prompt.
    if matches!(req.path.as_str(), "/sync" | "/poll") {
        mark_extension_seen();
    }
    match req.path.as_str() {
        "/sync" => Outcome::Snap(200),
        "/poll" => match serde_json::from_slice::<PollReq>(&req.body) {
            Ok(p) => Outcome::Poll(p.version.as_ref().and_then(tolerant_u64).unwrap_or(0)),
            Err(_) => Outcome::Poll(0),
        },
        "/download" => match serde_json::from_slice::<ExtMsg>(&req.body) {
            Ok(msg) => {
                handle_download(&msg);
                Outcome::Snap(200)
            }
            Err(_) => Outcome::Snap(400),
        },
        "/media" => match serde_json::from_slice::<ExtMsg>(&req.body) {
            Ok(msg) => {
                handle_media(app, &msg);
                Outcome::Snap(200)
            }
            Err(_) => Outcome::Snap(400),
        },
        "/vid" => match serde_json::from_slice::<ExtMsg>(&req.body) {
            Ok(msg) => {
                handle_vid(app, &msg);
                Outcome::Snap(200)
            }
            Err(_) => Outcome::Snap(400),
        },
        "/batch" => match serde_json::from_slice::<BatchMsg>(&req.body) {
            Ok(msg) => {
                handle_batch(&msg);
                Outcome::Snap(200)
            }
            Err(_) => Outcome::Snap(400),
        },
        "/clear" => {
            let mut app = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            app.videos.clear();
            app.bump();
            Outcome::Snap(200)
        }
        "/clear-tab" => match serde_json::from_slice::<ExtMsg>(&req.body) {
            Ok(msg) => {
                let mut app = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let tab = msg.tab_id.unwrap_or_default();
                if !tab.is_empty() {
                    app.videos.retain(|v| v.tab_id != tab);
                    app.manifest_pages.remove(&tab);
                    // The tab's captures are cleared, so a reload of the
                    // same page must be allowed to find them again (Java
                    // `onTabNavigated`).
                    app.recent_manifests.retain(|key, _| key.0 != tab);
                }
                app.bump();
                Outcome::Snap(200)
            }
            Err(_) => Outcome::Snap(400),
        },
        "/tab-update" => match serde_json::from_slice::<ExtMsg>(&req.body) {
            Ok(msg) => {
                handle_tab_update(app, &msg);
                Outcome::Snap(200)
            }
            Err(_) => Outcome::Snap(400),
        },
        "/show" => {
            push_command(MonitorCmd::ShowWindow);
            Outcome::Snap(200)
        }
        _ => Outcome::Snap(404),
    }
}

// ---------------------------------------------------------------------------
// Message handlers
// ---------------------------------------------------------------------------

/// `/download`: the browser was about to download this and already stopped
/// it — the GUI adds it to the list and starts the engine.
fn handle_download(msg: &ExtMsg) {
    let Some(url) = msg.url.as_deref().map(str::trim).filter(|u| is_http_url(u)) else { return };
    let name = download_name(msg);
    eprintln!("xdm-app: extension takeover: {name}");
    let total = known_size(msg);
    let referer = effective_referer(msg);
    let headers = compose_headers(
        msg,
        msg.cookie.as_deref(),
        msg.user_agent.as_deref(),
        referer.as_deref(),
    );
    push_command(MonitorCmd::AddDownload(DownloadCandidate {
        url: url.to_owned(),
        name,
        total,
        headers,
    }));
}

/// `/vid {vid}`: start the popup-selected video's download. The item stays
/// listed after starting (XDM semantics: it disappears only on
/// `clear` / `clear-tab`).
fn handle_vid(app: &Arc<Mutex<App>>, msg: &ExtMsg) {
    let Some(vid) = msg.vid.as_ref().and_then(tolerant_u64) else { return };
    let app = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(item) = app.videos.iter().find(|v| v.id == vid) else {
        eprintln!("xdm-app: /vid for unknown video id {vid}");
        return;
    };
    push_command(MonitorCmd::AddDownload(DownloadCandidate {
        url: item.url.clone(),
        name: item.name.clone(),
        total: item.size,
        headers: item.headers.clone(),
    }));
    eprintln!("xdm-app: video download queued: {}", item.name);
}

/// `/tab-update {tabUrl, tabTitle}`: a tab with detected videos changed its
/// page title; title-named items follow.
fn handle_tab_update(app: &Arc<Mutex<App>>, msg: &ExtMsg) {
    let Some(tab_url) = msg.tab_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
        return;
    };
    let Some(new_title) = msg.tab_title.as_deref().map(str::trim).filter(|t| !t.is_empty())
    else {
        return;
    };
    let mut app = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut changed = false;
    for item in &mut app.videos {
        if item.tab_url == tab_url && item.title_named && item.tab_title != new_title {
            item.tab_title = new_title.to_owned();
            item.name = assemble_name(new_title, &item.ext);
            changed = true;
        }
    }
    if changed {
        app.bump();
    }
}

/// `/batch`: queue every batch item as a download. A group's cookie fits
/// into that group's items' `Cookie` header; the page shares
/// `User-Agent`/`Referer`. The same URL dedupes, non-http URLs are dropped,
/// up to Java's 5000-item cap.
fn handle_batch(msg: &BatchMsg) {
    const MAX_BATCH_ITEMS: usize = 5000;
    let ua = msg.user_agent.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let referer = msg.referer.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut seen = std::collections::HashSet::new();
    'groups: for group in &msg.groups {
        let cookie = group.cookie.as_deref().map(str::trim).filter(|c| !c.is_empty());
        let extra = BatchExtra { cookie, user_agent: ua, referer };
        for item in &group.items {
            if seen.len() >= MAX_BATCH_ITEMS {
                break 'groups;
            }
            let url = item.url.trim();
            if !is_http_url(url) || !seen.insert(url.to_owned()) {
                continue;
            }
            let name = batch_name(item);
            let total = item.file_size.filter(|s| *s > 0).map(|s| s as u64);
            push_command(MonitorCmd::AddDownload(DownloadCandidate {
                url: url.to_owned(),
                name,
                total,
                headers: compose_batch_headers(&extra),
            }));
        }
    }
    eprintln!("xdm-app: batch queued {} items", seen.len());
}

/// `/media`: the detection pipeline. Adds at most one item per captured
/// request and bumps the state version so parked polls wake and the popup
/// badge refreshes.
fn handle_media(app: &Arc<Mutex<App>>, msg: &ExtMsg) {
    let Some(url) = msg.url.as_deref().map(str::trim).filter(|u| is_http_url(u)) else { return };
    let ct = msg.response_headers.as_ref().and_then(|h| header(h, "content-type"));
    let len = msg
        .file_size
        .filter(|s| *s > 0)
        .map(|s| s as u64)
        .or_else(|| response_size(msg));
    let untyped_min = MIN_VIDEO_SIZE_KB * 1024;

    // No Content-Type at all: only a manifest (by URL) or a plainly
    // extended media path of at least a sensible size is worth anything
    // (Java `isUntypedMedia`).
    if ct.is_none() && !untyped_media(url, len, untyped_min) {
        return;
    }

    let is_hls = ct_some(&ct, "mpegurl") || ct_some(&ct, "m3u8") || url.contains("m3u8");
    let is_dash = ct_some(&ct, "dash") || url.contains(".mpd");
    let now = App::now();

    let mut app = app.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    if is_dash || is_hls {
        // A player refetches manifests (retries, live refreshes): one item
        // per (tab, url) inside the window (Java `recentManifests`), and
        // let the page claim its tab + referer so the player's other
        // traffic stays out.
        if !claim_manifest(&mut app, msg.tab_id.as_deref(), url, now) {
            return;
        }
        if let Some(tab) = msg.tab_id.as_deref().map(str::to_owned) {
            // Tabs are few; a hard cap keeps a hostile stream from growing it.
            if app.manifest_pages.len() >= 128 {
                app.manifest_pages.clear();
            }
            app.manifest_pages.insert(tab, msg.tab_url.as_deref().unwrap_or("").to_owned());
        }
        if let Some(referer) = effective_referer(msg) {
            remember(&mut app.skip_referers, referer, now);
        }
        let visible = if is_dash { "DASH" } else { "HLS" };
        add_video(
            &mut app,
            VideoItem {
                // Our engine names and assembles the real output itself;
                // the item shows the page-derived stem until then. The id
                // placeholder is assigned by add_video.
                id: 0,
                url: url.to_owned(),
                name: manifest_name(msg, url),
                ext: "mp4".to_owned(),
                info: visible.to_owned(),
                tab_id: msg.tab_id.clone().unwrap_or_default(),
                tab_url: msg.tab_url.clone().unwrap_or_default(),
                tab_title: msg.tab_title.clone().unwrap_or_default(),
                title_named: true,
                size: None,
                headers: compose_headers(
                    msg,
                    msg.cookie.as_deref(),
                    msg.user_agent.as_deref(),
                    effective_referer(msg).as_deref(),
                ),
            },
        );
        eprintln!("xdm-app: {visible} video captured: {url}");
        return;
    }

    // Plain HTTP media — player/fragment suppression first (Java
    // `isHttpVideo` guards + the referer skip set).
    if is_fragment_of_player(&app, msg, now)
        || excludes_player_traffic(&mut app, msg, url, len, ct.as_deref(), now)
    {
        return;
    }
    let Some(ext) = video_ext(url, ct.as_deref()) else { return };
    let info = match len {
        Some(s) => format!("[{ext}] {}", fmt_size(s)),
        None => format!("[{ext}]"),
    };
    add_video(
        &mut app,
        VideoItem {
            id: 0, // assigned by add_video
            url: url.to_owned(),
            name: video_name(msg, &ext),
            ext: ext.clone(),
            info,
            tab_id: msg.tab_id.clone().unwrap_or_default(),
            tab_url: msg.tab_url.clone().unwrap_or_default(),
            tab_title: msg.tab_title.clone().unwrap_or_default(),
            title_named: msg.filename.is_none()
                && msg.file.is_none()
                && msg.tab_title.as_deref().is_some_and(|t| !t.trim().is_empty()),
            size: len,
            headers: compose_headers(
                msg,
                msg.cookie.as_deref(),
                msg.user_agent.as_deref(),
                effective_referer(msg).as_deref(),
            ),
        },
    );
    eprintln!("xdm-app: video captured: {url} ({ext})");
}

/// Inserts a new detected video (bumping the state version), or refreshes an
/// existing capture of the same URL in the same tab (metadata may have
/// changed — size, title) without allocating a new id.
fn add_video(app: &mut App, item: VideoItem) {
    match app.videos.iter_mut().find(|v| v.url == item.url && v.tab_id == item.tab_id) {
        Some(existing) => {
            let id = existing.id;
            *existing = VideoItem { id, ..item };
        }
        None => {
            let id = app.next_id;
            app.next_id += 1;
            app.videos.push(VideoItem { id, ..item });
        }
    }
    app.bump();
}

/// True when this exact (tab, manifest url) was captured inside the window
/// (Java `claimManifestFetch` — its "skip duplicate" flip is treated as on,
/// which is the sensible default for us).
fn claim_manifest(app: &mut App, tab: Option<&str>, url: &str, now: u64) -> bool {
    let key = (tab.unwrap_or("").to_owned(), url.to_owned());
    if app
        .recent_manifests
        .get(&key)
        .is_some_and(|t| now.saturating_sub(*t) < MEDIA_WINDOW_MS)
    {
        return false;
    }
    remember(&mut app.recent_manifests, key, now);
    true
}

/// Media whose referer matches a captured manifest's referer is that
/// player's traffic (Java `referersToSkip`), inside the same window.
fn is_fragment_of_player(app: &App, msg: &ExtMsg, now: u64) -> bool {
    effective_referer(msg)
        .and_then(|referer| app.skip_referers.get(&referer))
        .is_some_and(|t| now.saturating_sub(*t) < MEDIA_WINDOW_MS)
}

/// The message's referer: the direct field first, else the captured request
/// headers — the extension sends it in either place, and both must suppress
/// (and be suppressed by) the same entry.
fn effective_referer(msg: &ExtMsg) -> Option<String> {
    msg.referer
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            msg.request_headers
                .as_ref()
                .and_then(|h| header(h, "referer"))
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
        })
}

/// Java `isHttpVideo`'s guards for plain media: too small for a video, XDM's
/// own ports, known CDN noise, fMP4 init segments and their directories, and
/// media still on the page a manifest was captured from.
fn excludes_player_traffic(
    app: &mut App,
    msg: &ExtMsg,
    url: &str,
    len: Option<u64>,
    ct: Option<&str>,
    now: u64,
) -> bool {
    if len.is_some_and(|l| l < MIN_VIDEO_SIZE_KB * 1024) {
        return true;
    }
    if ct.is_some_and(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("m4s") || c.contains("f4f")
    }) {
        return true;
    }
    let low = url.to_ascii_lowercase();
    if low.contains("127.0.0.1:9614")
        || low.contains("127.0.0.1:8597")
        || low.contains("fbcdn")
        || low.contains("abst")
        || low.contains("f4x")
    {
        return true;
    }
    // Still on the page of a captured manifest? Player traffic.
    let expected = msg.tab_url.as_deref().unwrap_or("");
    if msg
        .tab_id
        .as_deref()
        .is_some_and(|tab| app.manifest_pages.get(tab).is_some_and(|p| p == expected))
    {
        return true;
    }
    // fMP4 init segment, and everything coming from its directory.
    let dir = url_dir(&low);
    if low.contains("init.mp4") {
        remember(&mut app.fragment_dirs, dir, now);
        return true;
    }
    app.fragment_dirs.contains_key(&dir)
}

// ---------------------------------------------------------------------------
// Headers, names, sizes
// ---------------------------------------------------------------------------

/// Replayed request headers for a download candidate: the captured
/// requestHeaders (minus `Cookie`, which has its own channel), plus the
/// message's `cookie` as one composed `Cookie` header, plus
/// `user_agent`/`referer` when missing from the capture.
fn compose_headers(
    msg: &ExtMsg,
    cookie: Option<&str>,
    user_agent: Option<&str>,
    referer: Option<&str>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(headers) = &msg.request_headers {
        for (name, values) in headers {
            if name.eq_ignore_ascii_case("cookie") {
                continue;
            }
            let Some(value) = values.iter().find_map(primitive_to_string).filter(|v| !v.trim().is_empty())
            else {
                continue;
            };
            out.push((name.clone(), value.trim().to_owned()));
        }
    }
    let cookie = cookie.map(str::trim).filter(|c| !c.is_empty());
    if let Some(c) = cookie {
        out.push(("Cookie".to_owned(), c.to_owned()));
    }
    if let Some(ua) = user_agent.map(str::trim).filter(|u| !u.is_empty())
        && !has_header(&out, "user-agent")
    {
        out.push(("User-Agent".to_owned(), ua.to_owned()));
    }
    if let Some(referer) = referer.map(str::trim).filter(|r| !r.is_empty())
        && !has_header(&out, "referer")
    {
        out.push(("Referer".to_owned(), referer.to_owned()));
    }
    out
}

/// Case-insensitive header-name membership for an (ordered) header list.
fn has_header(headers: &[(String, String)], probe: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(probe))
}

struct BatchExtra<'a> {
    cookie: Option<&'a str>,
    user_agent: Option<&'a str>,
    referer: Option<&'a str>,
}

fn compose_batch_headers(extra: &BatchExtra<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(ua) = extra.user_agent {
        out.push(("User-Agent".to_owned(), ua.to_owned()));
    }
    if let Some(referer) = extra.referer {
        out.push(("Referer".to_owned(), referer.to_owned()));
    }
    if let Some(cookie) = extra.cookie {
        out.push(("Cookie".to_owned(), cookie.to_owned()));
    }
    out
}

/// Java `getFileName(msg)`: `filename` → `file` → tab title → URL last
/// segment (the Content-Disposition file name is honored first, when the
/// capture has one), sanitized; the mapped MIME extension is appended
/// without duplicating (Java `fileNameWithExt`).
fn download_name(msg: &ExtMsg) -> String {
    let base = match msg
        .response_headers
        .as_ref()
        .and_then(|h| header(h, "content-disposition"))
        .as_deref()
        .and_then(disposition_file_name)
        .map(|s| sanitize_file_name(&s))
        .filter(|n| !n.is_empty())
    {
        Some(n) => n,
        None => base_name(msg),
    };
    let mime = msg
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            msg.response_headers
                .as_ref()
                .and_then(|h| header(h, "content-type").filter(|v| !v.trim().is_empty()))
        });
    let ext = mime.as_deref().map(mime_ext).filter(|e| !e.is_empty());
    match ext {
        Some(ext) => name_with_ext(&base, ext),
        None => base,
    }
}

/// Java `getFileName(msg)`: `filename` → `file` → tab title → URL last
/// segment, sanitized.
fn base_name(msg: &ExtMsg) -> String {
    msg.filename
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .or_else(|| msg.file.as_deref().map(str::trim).filter(|c| !c.is_empty()))
        .or_else(|| msg.tab_title.as_deref().map(str::trim).filter(|c| !c.is_empty()))
        .map(sanitize_file_name)
        .unwrap_or_else(|| url_file_name(&msg.url.clone().unwrap_or_default()))
}

/// Name for a detected plain video: base + mapped extension.
fn video_name(msg: &ExtMsg, ext: &str) -> String {
    name_with_ext(&base_name(msg), ext)
}

fn name_with_ext(name: &str, ext: &str) -> String {
    if name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
        name.to_owned()
    } else {
        format!("{name}.{ext}")
    }
}

/// Manifest item name: the shown/download stem is the page title (XDM names
/// a detected video after the page it came from) or the URL's stem, tagged
/// with the engine's typical output container for streaming.
fn manifest_name(msg: &ExtMsg, url: &str) -> String {
    let candidate = msg
        .tab_title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .or_else(|| msg.filename.as_deref().map(str::trim).filter(|t| !t.is_empty()))
        .map(sanitize_file_name)
        .unwrap_or_else(|| url_stem(url));
    name_with_ext(&candidate, "mp4")
}

/// Strips an extension a URL segment would have contributed ("master.m3u8"
/// → "master"). Deliberately NOT applied to tab titles: a page titled
/// "S04.E03" must not lose its dots.
fn stem_ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
        _ => name.to_owned(),
    }
}

fn assemble_name(title: &str, ext: &str) -> String {
    let base = sanitize_file_name(title);
    if ext.is_empty() || base.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
        base
    } else {
        format!("{base}.{ext}")
    }
}

fn url_stem(url: &str) -> String {
    stem_ext(&url_file_name(url))
}

/// Last non-empty path segment, URL-decoded and sanitized.
fn url_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segment = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
    let decoded = percent_decode(segment);
    let name = decoded.trim();
    sanitize_file_name(if name.is_empty() { "download" } else { name })
}

/// `POST /batch` item name: the extension's own filename else the URL's
/// last segment, with a mapped MIME extension appended when missing.
fn batch_name(item: &BatchItem) -> String {
    let base = item
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map_or_else(|| url_file_name(&item.url), sanitize_file_name);
    match item.mime_type.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => name_with_ext(&base, mime_ext(m)),
        None => base,
    }
}

fn sanitize_file_name(name: &str) -> String {
    let name = percent_decode(name);
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        // File-system-invalid plus every control char (Java
        // `sanitizeFileName`).
        if matches!(c, '/' | '\\' | '?' | '*' | ':' | '|' | '"' | '<' | '>')
            || (c as u32) < 0x20
        {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    let out = out.trim().trim_end_matches(['.', ' ']);
    if out.is_empty() { "download".to_owned() } else { out.to_owned() }
}

fn percent_decode(s: &str) -> String {
    // A minimal %-decoder; on a malformed escape the original chars stay.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Known size: the extension's own field first, then Content-Length /
/// Content-Range of the captured response.
fn known_size(msg: &ExtMsg) -> Option<u64> {
    msg.file_size
        .filter(|s| *s > 0)
        .map(|s| s as u64)
        .or_else(|| response_size(msg))
}

/// Content-Length or, on range probes, the total in Content-Range.
fn response_size(msg: &ExtMsg) -> Option<u64> {
    let headers = msg.response_headers.as_ref()?;
    let range = header(headers, "content-range");
    let cl = header(headers, "content-length");
    range
        .as_deref()
        .and_then(|v| v.rsplit('/').next())
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "*")
        .and_then(|t| t.parse().ok())
        .or_else(|| cl.as_deref().and_then(|s| s.trim().parse().ok()))
}

fn header(headers: &HashMap<String, Vec<serde_json::Value>>, name: &str) -> Option<String> {
    headers.iter().find_map(|(k, vals)| {
        (k.eq_ignore_ascii_case(name) && !vals.is_empty())
            .then(|| primitive_to_string(&vals[0]).unwrap_or_default())
    })
}

/// `Content-Disposition`'s file name, `filename` (quoted or bare) or a
/// `filename*=UTF-8''...` with the header's value.
fn disposition_file_name(disposition: &str) -> Option<String> {
    for part in disposition.split(';') {
        let part = part.trim();
        let Some(eq) = part.find('=') else { continue };
        // Key lowercased for matching; the value keeps its original case.
        let key = part[..eq].trim().to_ascii_lowercase();
        let value = &part[eq + 1..];
        if key == "filename*" {
            // RFC 5987: `charset'language'percent-encoded-name` — preferred
            // when present and decodable.
            if let Some(rest) = value.splitn(3, '\'').nth(2) {
                let trimmed = percent_decode(rest).trim().to_owned();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        } else if key == "filename" {
            let unquoted = value.trim().trim_matches('"').trim();
            if !unquoted.is_empty() {
                return Some(unquoted.to_owned());
            }
        }
    }
    None
}

fn primitive_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Numbers and stringified numbers (`vid: id + ""`, `Long` versions) are
/// both accepted.
fn tolerant_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The extension's media Content-Type membership check.
fn ct_some(ct: &Option<String>, probe: &str) -> bool {
    ct.as_deref().is_some_and(|c| c.to_ascii_lowercase().contains(probe))
}

// ---------------------------------------------------------------------------
// Video detection helpers (VideoHelper port)
// ---------------------------------------------------------------------------

/// A response with no Content-Type is only worth a look for a manifest (by
/// URL) or a plain-path media file of at least `min` bytes.
fn untyped_media(url: &str, len: Option<u64>, min: u64) -> bool {
    if url_lower_has(url, ".m3u8") || url_lower_has(url, ".mpd") {
        return true;
    }
    path_ext_plain(url).is_some_and(|_| len.is_some_and(|l| l >= min))
}

fn url_lower_has(url: &str, needle: &str) -> bool {
    url.to_ascii_lowercase().contains(needle)
}

/// The URL's path extension when it is a known plain media type.
fn path_ext_plain(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?;
    let ext = ext.to_ascii_lowercase();
    PLAIN_MEDIA_EXTS.iter().find(|e| **e == ext).copied()
}

/// Java's `video/x-*` → extension map.
fn mime_ext(mime: &str) -> &'static str {
    let m = mime.to_ascii_lowercase();
    if m.contains("video/mp4") {
        "mp4"
    } else if m.contains("video/x-flv") {
        "flv"
    } else if m.contains("video/webm") || m.contains("matroska") || m.contains("mkv") {
        "mkv"
    } else if m.contains("audio/mpeg") || m.contains("audio/mp3") {
        "mp3"
    } else if m.contains("audio/aac") || m.contains("audio/x-aac") {
        "aac"
    } else if m.contains("audio/mp4") || m.contains("audio/x-m4a") {
        "m4a"
    } else if m.contains("video/x-m4v") {
        "m4v"
    } else if m.contains("video/quicktime") {
        "mov"
    } else if m.contains("video/3gpp") {
        "3gp"
    } else if m.contains("video/x-msvideo") || m.contains("video/avi") {
        "avi"
    } else if m.contains("video/x-ms-wmv") {
        "wmv"
    } else if m.contains("divx") {
        "divx"
    } else if m.contains("video/mpeg") {
        "mpg"
    } else if m.contains("video/ogg") {
        "ogv"
    } else if m.contains("audio/ogg") {
        "ogg"
    } else if m.contains("audio/opus") {
        "opus"
    } else if m.contains("audio/webm") {
        "webm"
    } else if m.contains("flac") {
        "flac"
    } else if m.contains("audio/") && m.contains("wav") {
        "wav"
    } else {
        ""
    }
}

/// Java `processHttpVideo`: the extension maps opaque Content-Types
/// (application/octet-stream) to the path's plain extension; the URL's own
/// `.mp4/.mkv/.ts` hints apply when even that fails; nothing { `None` }.
fn video_ext(url: &str, ct: Option<&str>) -> Option<String> {
    if let Some(ct) = ct.map(str::trim).filter(|c| !c.is_empty()) {
        let mapped = mime_ext(ct);
        if !mapped.is_empty() {
            return Some(mapped.to_owned());
        }
        // Video/audio-ish but unknown subtype, or octet-stream:
        // the URL gets a say, else nothing.
        if path_ext_plain(url).is_none() {
            let low = url.to_ascii_lowercase();
            if low.contains(".mp4") {
                return Some("mp4".to_owned());
            }
            if low.contains(".mkv") {
                return Some("mkv".to_owned());
            }
            if low.contains(".ts") {
                return Some("ts".to_owned());
            }
            return None;
        }
    }
    path_ext_plain(url).map(str::to_owned)
}

/// Directory part of a URL path (Java's `URI.resolve(".")`), lowercase.
fn url_dir(low_url: &str) -> String {
    match low_url.rsplit_once('/') {
        Some((dir, _)) => dir.rsplit_once('/').map_or(dir.to_owned(), |(scheme_slash, _)| {
            let _ = scheme_slash;
            dir.to_owned()
        }),
        None => String::new(),
    }
}

/// Human size for the popup info, `[mp4] 420.2 MB` style.
fn fmt_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every test that pushes into or drains the GUI command
    /// slot: Rust unit tests run concurrently and share the process-wide
    /// static slot. A tokio mutex because one consumer is an async test.
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn ext_msg(json: &str) -> ExtMsg {
        serde_json::from_str(json).expect("test JSON must parse")
    }

    fn app() -> (Arc<Mutex<App>>, watch::Receiver<u64>) {
        let (tx, rx) = watch::channel(0u64);
        (Arc::new(Mutex::new(App::new(tx))), rx)
    }

    #[test]
    fn ext_msg_parses_headers_and_optional_fields() {
        let msg = ext_msg(
            r#"{
                "url": "https://cdn.example.com/v/file.mp4",
                "cookie": "a=1; b=2",
                "requestHeaders": {"User-Agent": ["firefox/1"], "Referer": ["https://x.test/"]},
                "responseHeaders": {"Content-Type": ["video/mp4"], "Content-Length": ["1234"]},
                "tabId": "7", "tabUrl": "https://x.test/p", "method": "GET", "download": true
            }"#,
        );
        assert_eq!(header(msg.response_headers.as_ref().unwrap(), "content-type").as_deref(), Some("video/mp4"));
        assert_eq!(response_size(&msg), Some(1234));
        assert_eq!(msg.tab_id.as_deref(), Some("7"));
        assert!(msg.vid.is_none());
    }

    #[test]
    fn stringified_numbers_are_tolerated() {
        assert_eq!(tolerant_u64(&serde_json::json!("12")), Some(12));
        assert_eq!(tolerant_u64(&serde_json::json!(12)), Some(12));
        assert_eq!(tolerant_u64(&serde_json::json!("x")), None);
        let msg = ext_msg(r#"{"vid": "3"}"#);
        assert_eq!(msg.vid.as_ref().and_then(tolerant_u64), Some(3));
    }

    #[test]
    fn compose_headers_skips_captured_cookie_and_fills_gaps() {
        let msg = ext_msg(
            r#"{
                "requestHeaders": {"Cookie": ["inside=1"], "Referer": ["https://x/"], "Accept": ["text/html"]},
                "cookie": "real=0",
                "user_agent": "FF/2"
            }"#,
        );
        let headers = compose_headers(&msg, Some("real=0"), Some("FF/2"), msg.referer.as_deref());
        assert!(!headers.iter().any(|(n, v)| n.eq_ignore_ascii_case("cookie") && v == "inside=1"));
        assert!(headers.contains(&("Cookie".to_owned(), "real=0".to_owned())));
        assert!(headers.contains(&("User-Agent".to_owned(), "FF/2".to_owned())));
        assert_eq!(headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("referer")).count(), 1);
    }

    #[test]
    fn mime_map_matches_the_xdm_table() {
        assert_eq!(mime_ext("Video/MP4"), "mp4");
        assert_eq!(mime_ext("video/webm"), "mkv");
        assert_eq!(mime_ext("application/x-matroska"), "mkv");
        assert_eq!(mime_ext("audio/mp4"), "m4a");
        assert_eq!(mime_ext("audio/mp4"), "m4a");
        assert_eq!(mime_ext("audio/webm"), "webm");
        assert_eq!(mime_ext("application/octet-stream"), "");
        assert_eq!(mime_ext("application/pdf"), "");
    }

    #[test]
    fn video_ext_falls_back_to_the_url() {
        assert_eq!(video_ext("https://x/a/f.mp4", Some("application/octet-stream")), Some("mp4".into()));
        assert_eq!(video_ext("https://x/a/f", Some("application/octet-stream")), None);
        // webm maps only from a real webm Content-Type; it is not in Java's
        // plainMediaExts, so an unknown subtype plus a .webm path stays out.
        assert_eq!(video_ext("https://x/a/f.webm", Some("audio/webm")), Some("webm".into()));
        assert_eq!(video_ext("https://x/a/f.webm", Some("audio/unknown")), None);
        // URL hints (.mp4/.mkv/.ts) apply after an unmapped Content-Type.
        assert_eq!(video_ext("https://x/a/f.ts", Some("video/unknown")), Some("ts".into()));
        assert_eq!(video_ext("https://x/a/f.mkv", Some("video/unknown")), Some("mkv".into()));
        // Without a Content-Type only Java's plain list speaks.
        assert_eq!(video_ext("https://x/a/f.mp4", None), Some("mp4".into()));
        assert_eq!(video_ext("https://x/a/f.flac", None), Some("flac".into()));
        assert_eq!(video_ext("https://x/a/f.ts", None), None);
        assert_eq!(video_ext("https://x/a/f.xyz", None), None);
    }

    #[test]
    fn naming_goes_filename_file_title_url_and_dedupes_extensions() {
        // filename wins; its extension already .mp4 → not duplicated.
        let msg = ext_msg(r#"{"filename": "My Clip.mp4", "url": "https://x/a/v"}"#);
        assert_eq!(video_name(&msg, "mp4"), "My Clip.mp4");
        // no filename: URL segment, decoded, plus ext.
        let msg = ext_msg(r#"{"url": "https://x/My%20Video?a=1"}"#);
        assert_eq!(video_name(&msg, "mkv"), "My Video.mkv");
        // illegal chars sanitized.
        assert_eq!(sanitize_file_name("a<b>c\"d\"e:f?g"), "a_b_c_d_e_f_g");
        // tab title named for the popup text.
        let msg = ext_msg(r#"{"tabTitle": "Concert"}"#);
        assert_eq!(video_name(&msg, "mp4"), "Concert.mp4");
        // manifest name: page title first, URL stem otherwise.
        let msg = ext_msg(r#"{"tabTitle": "Live"}"#);
        assert_eq!(manifest_name(&msg, "https://x/a/master.m3u8"), "Live.mp4");
        assert_eq!(manifest_name(&ext_msg("{}"), "https://x/a/master.m3u8"), "master.mp4");
    }

    #[test]
    fn untyped_media_checks_manifest_then_size() {
        assert!(untyped_media("https://x/a/index.m3u8", None, MIN_VIDEO_SIZE_KB * 1024));
        assert!(untyped_media("https://x/a/play.mpd", None, MIN_VIDEO_SIZE_KB * 1024));
        assert!(!untyped_media("https://x/a/clip.mp4", Some(100), MIN_VIDEO_SIZE_KB * 1024));
        assert!(untyped_media("https://x/a/clip.mp4", Some(2048 * 1024), MIN_VIDEO_SIZE_KB * 1024));
        assert!(!untyped_media("https://x/a/page.js", Some(9 * 1024 * 1024), MIN_VIDEO_SIZE_KB * 1024));
    }

    #[test]
    fn batch_names_from_extension_and_mime() {
        let item: BatchItem = serde_json::from_str(r#"{"url":"https://x/1.zip","filename":"one.zip"}"#).unwrap();
        assert_eq!(batch_name(&item), "one.zip");
        let item: BatchItem = serde_json::from_str(r#"{"url":"https://x/2","mimeType":"video/mp4"}"#).unwrap();
        assert_eq!(batch_name(&item), "2.mp4");
        let item: BatchItem = serde_json::from_str(r#"{"url":"https://x/3"}"#).unwrap();
        assert_eq!(batch_name(&item), "3");
    }

    #[test]
    fn batch_headers_in_ua_referer_cookie_order() {
        let headers = compose_batch_headers(&BatchExtra {
            cookie: Some("c=1"),
            user_agent: Some("UA"),
            referer: Some("https://p/"),
        });
        assert_eq!(
            headers,
            vec![
                ("User-Agent".to_owned(), "UA".to_owned()),
                ("Referer".to_owned(), "https://p/".to_owned()),
                ("Cookie".to_owned(), "c=1".to_owned()),
            ]
        );
    }

    #[test]
    fn content_disposition_names_parse() {
        assert_eq!(
            disposition_file_name(r#"attachment; filename="setup.exe""#).as_deref(),
            Some("setup.exe")
        );
        assert_eq!(
            disposition_file_name("inline; filename=plain.bin").as_deref(),
            Some("plain.bin")
        );
        assert_eq!(
            disposition_file_name("attachment; filename*=UTF-8''My%20Ca%C3%B1a.mp4").as_deref(),
            Some("My Caña.mp4")
        );
    }

    #[test]
    fn sync_snapshot_carries_the_configdto_shape() {
        let json = sync_json(vec![], 42, "cafe");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["version"], 42);
        assert_eq!(v["instanceId"], "cafe");
        assert!(v["fileExts"].as_array().unwrap().iter().any(|e| e == "MP4"));
        assert_eq!(v["blockedHosts"].as_array().unwrap().len(), 3);
        assert!(v["requestFileExts"].as_array().unwrap().iter().any(|e| e == "M3U8"));
        assert_eq!(v["mediaTypes"].as_array().unwrap().len(), 4);
        assert_eq!(v["matchingHosts"], serde_json::json!([]));
        assert_eq!(v["videoList"], serde_json::json!([]));
    }

    #[test]
    fn plain_video_capture_and_replacement() {
        let (app, _) = app();
        let msg = ext_msg(
            r#"{
                "url": "https://cdn.test/v/movie.mp4",
                "requestHeaders": {"Referer": ["https://site.test/play"]},
                "responseHeaders": {"Content-Type": ["video/mp4"], "Content-Length": ["440400896"]},
                "tabId": "3", "tabUrl": "https://site.test/play", "tabTitle": "Movie"
            }"#,
        );
        handle_media(&app, &msg);
        {
            let guard = app.lock().unwrap();
            assert_eq!(guard.videos.len(), 1);
            let v = &guard.videos[0];
            assert_eq!(v.id, 1);
            assert_eq!(v.name, "Movie.mp4");
            assert_eq!(v.size, Some(440400896));
            assert_eq!(v.info, "[mp4] 420.0 MB");
            assert_eq!(v.headers, vec![("Referer".to_owned(), "https://site.test/play".to_owned())]);
            assert!(v.title_named, "no filename: the tab title named it");
            assert_eq!(guard.version, 1);
        }
        // Same capture again: replaced, not duplicated. Id stays.
        let first_version = app.lock().unwrap().version;
        handle_media(&app, &msg);
        let guard = app.lock().unwrap();
        assert_eq!(guard.videos.len(), 1);
        assert_eq!(guard.videos[0].id, 1);
        assert!(guard.version > first_version);
    }

    #[test]
    fn manifest_captured_once_per_window_with_fragment_suppression() {
        let (app, _) = app();
        let manifest = ext_msg(
            r#"{
                "url": "https://cdn.test/live/index.m3u8",
                "requestHeaders": {"Referer": ["https://site.test/play"]},
                "tabId": "9", "tabUrl": "https://site.test/play", "tabTitle": "Live"
            }"#,
        );
        handle_media(&app, &manifest);
        {
            let guard = app.lock().unwrap();
            assert_eq!(guard.videos.len(), 1);
            assert_eq!(guard.videos[0].name, "Live.mp4");
            assert_eq!(guard.videos[0].info, "HLS");
            assert!(guard.videos[0].size.is_none());
        }
        let version_after_capture = app.lock().unwrap().version;
        // Refetch inside the window: ignored entirely (no bump).
        handle_media(&app, &manifest);
        assert_eq!(app.lock().unwrap().version, version_after_capture);

        // A fragment with the manifest's referer: no second item.
        let fragment = ext_msg(
            r#"{"url": "https://cdn.test/live/seg1.ts", "referer": "https://site.test/play", "responseHeaders": {"Content-Type": ["video/mp2t"]}}"#,
        );
        handle_media(&app, &fragment);
        assert_eq!(app.lock().unwrap().videos.len(), 1);
        assert_eq!(app.lock().unwrap().version, version_after_capture);
    }

    #[test]
    fn fmp4_init_then_fragments_suppressed() {
        let (app, _) = app();
        let init = ext_msg(
            r#"{"url": "https://cdn.test/live/audio/init.mp4", "responseHeaders": {"Content-Length": ["3048576"]}}"#,
        );
        handle_media(&app, &init);
        // A 3 MB "init" passes the size gate, so the directory really gets
        // marked here (a real init.mp4 is small and simply fails the size
        // gate first; either way no item may ever list).
        assert_eq!(app.lock().unwrap().videos.len(), 0);
        let fragment = ext_msg(
            r#"{"url": "https://cdn.test/live/audio/seg.m4s", "responseHeaders": {"Content-Length": ["2048000"]}}"#,
        );
        handle_media(&app, &fragment);
        assert_eq!(app.lock().unwrap().videos.len(), 0);
    }

    #[test]
    fn vid_pushes_a_candidate_and_items_survive() {
        let _guard = TEST_LOCK.blocking_lock();
        let (app, _) = app();
        let msg = ext_msg(
            r#"{"url": "https://cdn.test/v/a.mp4", "responseHeaders": {"Content-Type": ["video/mp4"], "Content-Length": ["1048576"]}, "tabId": "2"}"#,
        );
        handle_media(&app, &msg);
        let vid_msg = ext_msg(r#"{"vid": "1"}"#);
        handle_vid(&app, &vid_msg);
        let cmds = take_commands();
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            MonitorCmd::AddDownload(c) => {
                assert_eq!(c.url, "https://cdn.test/v/a.mp4");
                assert_eq!(c.name, "a.mp4");
                assert_eq!(c.total, Some(1048576));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // XDM semantics: the item stays listed after starting.
        assert_eq!(app.lock().unwrap().videos.len(), 1);
    }

    #[test]
    fn batch_pushes_all_http_links() {
        let _guard = TEST_LOCK.blocking_lock();
        let batch = serde_json::from_str::<BatchMsg>(
            r#"{"userAgent":"UA","referer":"https://p/","groups":[
                {"cookie":"c=1","items":[{"url":"https://x/1.zip","fileSize":5},{"url":"https://x/1.zip"}]},
                {"items":[{"url":"ftp://y/no"}]}
            ]}"#,
        )
        .unwrap();
        handle_batch(&batch);
        let cmds = take_commands();
        assert_eq!(cmds.len(), 1);
    }

    #[test]
    fn download_candidate_from_extension_message() {
        let _guard = TEST_LOCK.blocking_lock();
        let msg = ext_msg(
            r#"{
                "url": "https://download.example.com/setup.exe",
                "cookie": "sid=7",
                "requestHeaders": {"User-Agent": ["ff"], "Referer": ["https://d/"], "Content-Type": ["application/x-www-form-urlencoded"]},
                "responseHeaders": {"Content-Length": ["5242880"], "Content-Type": ["application/octet-stream"]}
            }"#,
        );
        handle_download(&msg);
        let cmds = take_commands();
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            MonitorCmd::AddDownload(c) => {
                assert_eq!(c.url, "https://download.example.com/setup.exe");
                assert_eq!(c.name, "setup.exe");
                assert_eq!(c.total, Some(5242880));
                // Cookie from the message's own field; the captured headers
                // pass (with the engine's blocked list still applying later).
                assert!(c.headers.contains(&("Cookie".to_owned(), "sid=7".to_owned())));
                assert!(c.headers.contains(&("Referer".to_owned(), "https://d/".to_owned())));
                assert!(c.headers.contains(&("User-Agent".to_owned(), "ff".to_owned())));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    /// Live loop check: real sockets, real routing, slot communicates.
    #[tokio::test]
    async fn server_end_to_end() {
        let _guard = TEST_LOCK.lock().await;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        // A 1 s park keeps the parked-poll check fast (production parks for
        // PARK; the extension's re-poll floor is 1 s either way).
        tokio::spawn(async move {
            let (version_tx, version_rx) = watch::channel(0u64);
            let app = Arc::new(Mutex::new(App::new(version_tx)));
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let app = Arc::clone(&app);
                        let rx = version_rx.clone();
                        tokio::spawn(handle_conn_park(app, rx, stream, std::time::Duration::from_secs(1)));
                    }
                    Err(_) => return,
                }
            }
        });

        // GET /sync without Origin: answered.
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET /sync HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("\"version\""));

        // GET /sync with a page Origin: 403.
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET /sync HTTP/1.1\r\nHost: x\r\nOrigin: https://evil.test\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        assert!(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 403"));

        // State-changing path over GET: 405.
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET /download HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        assert!(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 405"));

        // POST /show through the real extension origin: command arrives.
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"POST /show HTTP/1.1\r\nHost: x\r\nOrigin: moz-extension://abef\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("\"instanceId\""));
        assert!(take_commands().into_iter().any(|c| matches!(c, MonitorCmd::ShowWindow)));

        // POST /media through the real protocol: the badge count goes up.
        let media_body = br#"{"url":"https://c/v/x.mp4","tabId":"5","tabUrl":"https://p/","tabTitle":"Page","requestHeaders":{"Referer":["https://p/"]},"responseHeaders":{"Content-Type":["video/mp4"],"Content-Length":["2097152"]}}"#;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(format!("POST /media HTTP/1.1\r\nHost: x\r\nOrigin: moz-extension://abef\r\nContent-Length: {}\r\n\r\n", media_body.len()).as_bytes()).await.unwrap();
        sock.write_all(media_body).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("\"videoList\":[{"), "{text}");
        assert!(text.contains("\"id\":\"1\"") && text.contains("Page.mp4"));

        // POST /poll with the current state version: parked until 204.
        let poll = br#"{"clientId":"t1","version":1}"#;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(format!("POST /poll HTTP/1.1\r\nHost: x\r\nOrigin: moz-extension://abef\r\nContent-Length: {}\r\n\r\n", poll.len()).as_bytes()).await.unwrap();
        sock.write_all(poll).await.unwrap();
        let mut buf = Vec::new();
        let started = std::time::Instant::now();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 204"), "{text}");
        // The park must have actually parked (not short-circuited to 204).
        assert!(started.elapsed() >= std::time::Duration::from_millis(500), "{:?}", started.elapsed());

        // A stale version poll: immediate fresh snapshot.
        let poll = br#"{"clientId":"t2","version":0}"#;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(format!("POST /poll HTTP/1.1\r\nHost: x\r\nOrigin: moz-extension://abef\r\nContent-Length: {}\r\n\r\n", poll.len()).as_bytes()).await.unwrap();
        sock.write_all(poll).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("\"id\":\"1\""));
    }
}
