//! The Reverything service: keeps the index of the NTFS volumes the user picked and answers
//! searches over a named pipe.
//!
//! ```text
//! reverything-service install            register and start the Windows service (admin)
//! reverything-service uninstall          stop and remove it (admin)
//! reverything-service stop               stop it, saving the index (admin)
//! reverything-service --console          run in the foreground until Ctrl+C (admin)
//! reverything-service --console --offline
//!                                        serve the development indices without admin rights,
//!                                        not kept up to date
//! reverything-service --bench            benchmarks, see the README (admin)
//! reverything-service query <text>       search through a running service
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eyre::{bail, Result};
use mimalloc_rust::GlobalMiMalloc;
use windows::core::BOOL;
use windows::Win32::System::Console::SetConsoleCtrlHandler;

use reverything_core::index::persist::dev_db_dir;
use reverything_core::ntfs::volume::ntfs_volumes;
use reverything_core::service::IndexSet;
use reverything_protocol::pipe_name;

mod bench;
mod config;
mod logger;
mod query;
mod security;
mod server;
mod winsvc;

#[global_allocator]
static GLOBAL: GlobalMiMalloc = GlobalMiMalloc;

/// How often changed indices are saved, so a crash or power loss does not cost a full rescan
const SAVE_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// A running index set with its pipe server.
pub struct App {
    pub set: Arc<IndexSet>,
}

impl App {
    /// Loads (or builds) the indices of the enabled volumes in the background and starts
    /// serving clients on `pipe`. `offline` loads the saved indices without volume access and
    /// does not update them.
    pub fn start(db_dir: PathBuf, pipe: String, offline: bool) -> Result<Self> {
        let volumes = ntfs_volumes();
        if volumes.is_empty() {
            bail!("No fixed NTFS volumes found");
        }
        let config = config::Config::load(&db_dir);
        let set = if offline {
            IndexSet::new_offline(volumes, db_dir)
        } else {
            IndexSet::new(volumes, db_dir)
        };
        set.set_enabled(&config.volumes);
        if !offline {
            set.delete_unused();
            let saver = set.clone();
            std::thread::Builder::new()
                .name("saver".into())
                .spawn(move || loop {
                    std::thread::sleep(SAVE_INTERVAL);
                    saver.save_changed();
                })?;
        }

        let server = server::Server::new(set.clone(), pipe);
        std::thread::Builder::new()
            .name("pipe server".into())
            .spawn(move || {
                if let Err(e) = server.serve() {
                    log::error!("Pipe server stopped: {:#}", e);
                }
            })?;

        Ok(Self { set })
    }
}

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    let result = match args.first().map(String::as_str) {
        Some("install") => winsvc::install(),
        Some("uninstall") => winsvc::uninstall(),
        Some("stop") => winsvc::stop(),
        Some("query") => query::run(&args[1..].join(" ")),
        Some("--bench") => bench::run(),
        Some("--console") => run_console(has("--offline")),
        Some("--service") => winsvc::run_dispatcher(),
        _ => {
            eprintln!("{}", USAGE);
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
Usage:
  reverything-service install              register and start the Windows service (admin)
  reverything-service uninstall            stop and remove the service (admin)
  reverything-service stop                 stop the service, saving the index (admin)
  reverything-service --console            run in the foreground until Ctrl+C (admin)
  reverything-service --console --offline  serve the development indices, no admin needed
  reverything-service --bench              run benchmarks (admin)
  reverything-service query <text>         search through a running service";

static CONSOLE_STOP: OnceLock<mpsc::Sender<()>> = OnceLock::new();

unsafe extern "system" fn on_console_ctrl(_ctrl_type: u32) -> BOOL {
    if let Some(stop) = CONSOLE_STOP.get() {
        let _ = stop.send(());
    }
    true.into()
}

fn run_console(offline: bool) -> Result<()> {
    logger::init_stderr();
    let db_dir = if offline {
        dev_db_dir()
    } else {
        security::data_dir()?
    };
    log::info!(
        "Using indices in {:?}{}",
        db_dir,
        if offline { " (offline)" } else { "" }
    );

    let (stop_tx, stop_rx) = mpsc::channel();
    let _ = CONSOLE_STOP.set(stop_tx);
    unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true)? };

    let app = App::start(db_dir, pipe_name(), offline)?;
    let _ = stop_rx.recv();
    log::info!("Stopping");
    if !offline {
        app.set.save_changed();
    }
    Ok(())
}
