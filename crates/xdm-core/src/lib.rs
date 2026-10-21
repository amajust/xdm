//! xdm-core: the XDM download engine, rewritten in Rust.
//!
//! Port of the Kotlin `xdm-core` module. Goals: same feature set, none of the
//! JVM weight — native binaries, async concurrency via tokio, crash-safe
//! resume state.
//!
//! Modules:
//! - [`http`]: segmented, resumable HTTP downloader (port of `HttpDownloaderTask`)
//! - [`models`]: shared download types (port of `downloaders/Models.kt`)

pub mod http;
pub mod models;
