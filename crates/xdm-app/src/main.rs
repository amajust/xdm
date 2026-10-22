//! xdm-app: the Rust rewrite of Xtreme Download Manager.
//!
//! `xdm-app` (no args) opens the iced GUI; the `download` subcommand is the
//! headless CLI slice exercising the engine directly.

mod gui;
mod monitor;

use std::io::Write as _;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use xdm_core::engine::download;
use xdm_core::http::Progress;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("gui") => {
            if let Err(e) = gui::run() {
                eprintln!("gui failed: {e}");
                std::process::exit(1);
            }
        }
        Some("--version" | "-V") => println!("xdm-app {}", env!("CARGO_PKG_VERSION")),
        Some("download") => cli_download(&args[1..]),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "xdm-app {} — Rust rewrite (in progress)\n\nUsage:\n  xdm-app                      open the GUI\n  xdm-app download <url> <destination>\n  xdm-app --version",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}

fn cli_download(args: &[String]) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = rt.block_on(download_impl(args)) {
        eprintln!("failed: {e}");
        std::process::exit(1);
    }
}

async fn download_impl(args: &[String]) -> Result<(), xdm_core::http::DownloadError> {
    let (Some(url), Some(dest)) = (args.first(), args.get(1)) else {
        usage();
    };
    let dest = PathBuf::from(dest);

    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::watch::channel(Progress::default());
    let url_owned = url.clone();
    let run_cancel = cancel.clone();
    let mut handle = tokio::spawn(async move {
        download(&url_owned, dest, Default::default(), run_cancel, tx).await
    });

    let mut interrupted = false;
    let mut sender_gone = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c(), if !interrupted => {
                interrupted = true;
                println!("\npaused — state saved; rerun the same command to resume");
                cancel.cancel();
            }
            changed = rx.changed(), if !sender_gone => {
                match changed {
                    Ok(()) => print_progress(&rx.borrow_and_update()),
                    Err(_) => sender_gone = true,
                }
            }
            res = &mut handle => {
                println!();
                match res {
                    Ok(Ok(Some(total))) => println!("done — {total} bytes"),
                    Ok(Ok(None)) => println!("done"),
                    Ok(Err(xdm_core::http::DownloadError::Cancelled)) => {
                        eprintln!("download paused");
                        std::process::exit(130);
                    }
                    Ok(Err(e)) => return Err(e),
                    Err(join) => eprintln!("download task panicked: {join}"),
                }
                break;
            }
        }
    }

    Ok(())
}

fn print_progress(p: &Progress) {
    match p.total {
        Some(total) if total > 0 => print!(
            "\r{:>9.2} / {:.2} MiB ({:5.1}%)",
            p.downloaded as f64 / MIB_F64,
            total as f64 / MIB_F64,
            (p.downloaded as f64 / total as f64 * 100.0).min(100.0)
        ),
        _ => print!("\r{:>12} bytes", p.downloaded),
    }
    let _ = std::io::stdout().flush();
}

const MIB_F64: f64 = 1024.0 * 1024.0;
