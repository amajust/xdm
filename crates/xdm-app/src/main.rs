//! xdm-app: CLI slice of the Rust rewrite. The iced GUI lands here later;
//! for now this exercises the engine end-to-end from the command line.

use std::io::Write as _;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use xdm_core::http::{HttpDownload, Progress};

const MIB: f64 = 1024.0 * 1024.0;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => println!("xdm-app {}", env!("CARGO_PKG_VERSION")),
        Some("download") => download(&args[1..]).await,
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "xdm-app {} — Rust rewrite (in progress)\n\nUsage:\n  xdm-app download <url> <destination>\n  xdm-app --version",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}

async fn download(args: &[String]) {
    let (Some(url), Some(dest)) = (args.first(), args.get(1)) else {
        usage();
    };
    let dest = PathBuf::from(dest);

    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::watch::channel(Progress::default());
    let task = HttpDownload::new(url.clone(), dest);
    let run_cancel = cancel.clone();
    let mut handle = tokio::spawn(async move { task.run(run_cancel, tx).await });

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
                    Ok(Err(e)) => {
                        eprintln!("failed: {e}");
                        std::process::exit(1);
                    }
                    Err(join) => eprintln!("download task panicked: {join}"),
                }
                break;
            }
        }
    }
}

fn print_progress(p: &Progress) {
    match p.total {
        Some(total) if total > 0 => print!(
            "\r{:>9.2} / {:.2} MiB ({:5.1}%)",
            p.downloaded as f64 / MIB,
            total as f64 / MIB,
            (p.downloaded as f64 / total as f64 * 100.0).min(100.0)
        ),
        _ => print!("\r{:>12} bytes", p.downloaded),
    }
    let _ = std::io::stdout().flush();
}
