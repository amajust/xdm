//! xdm-core: the XDM download engine, rewritten in Rust.
//!
//! Port of the Kotlin `xdm-core` module. Goals: same feature set, none of the
//! JVM weight — native binaries, async concurrency via tokio, crash-safe
//! resume state.
//!
//! Modules:
//! - [`engine`]: dispatches URLs to the right downloader (.m3u8/.mpd/HTTP)
//! - [`http`]: segmented, resumable HTTP downloader (port of `HttpDownloaderTask`)
//! - [`streaming`]: HLS/DASH segment streaming + assembly (port of the
//!   streaming downloaders; segments are concatenated into fMP4/TS/ADTS
//!   containers — no external tools, no remux yet)
//! - [`models`]: shared download types (port of `downloaders/Models.kt`)

pub mod engine;
pub mod http;
pub mod media;
pub mod models;
pub mod streaming;
