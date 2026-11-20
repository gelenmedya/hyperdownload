//! HyperDownload, the download manager add-on of rustyfiles. For now a command line to try the engine:
//!
//!   hyperdownload get <url> [folder]
//!   hyperdownload continue <file.hdpart.json>
//!
//! (rustyfiles will start it with the JSON-RPC interface; that comes next.)

use std::path::PathBuf;
use std::time::Duration;

use hd_engine::{Download, Phase, Request};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let download = match args.first().map(String::as_str) {
        Some("get") if args.len() >= 2 => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            Download::start(Request { url: args[1].clone(), dir, ..Default::default() })
        }
        Some("continue") if args.len() >= 2 => match Download::continue_from(std::path::Path::new(&args[1]), Request::default()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{}: {e}", args[1]);
                std::process::exit(2);
            }
        },
        _ => {
            eprintln!("hyperdownload get <url> [folder]\nhyperdownload continue <file.hdpart.json>");
            std::process::exit(2);
        }
    };
    loop {
        let s = download.status();
        match &s.phase {
            Phase::Done(path) => {
                println!("\ndone: {}", path.display());
                return;
            }
            Phase::Failed(e) => {
                eprintln!("\nfailed: {e}");
                std::process::exit(1);
            }
            Phase::Paused | Phase::Cancelled => return,
            _ => {
                let total = s.total.map(|t| format!("{:.1} MB", t as f64 / 1e6)).unwrap_or_else(|| "?".into());
                eprint!("\r{} {:.1} / {total} MB · {:.1} MB/s · {} connections   ", s.name.as_deref().unwrap_or("…"), s.done as f64 / 1e6, s.speed as f64 / 1e6, s.connections);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
