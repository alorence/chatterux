//! chatterux: blocks mechanical keyboard chatter (double key presses) on Linux.

mod config;
mod device;
mod filter;

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use evdev::Device;
use log::{error, info, warn};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use config::Config;

const INPUT_DIR: &str = "/dev/input";

static PERMISSION_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(version, about = "Blocks mechanical keyboard chatter (double key presses)")]
struct Args {
    /// Configuration file (defaults are used if it does not exist).
    #[arg(short, long, default_value = "/etc/chatterux/config.toml")]
    config: PathBuf,

    /// Lists input devices and shows which ones would be filtered.
    #[arg(long)]
    list_devices: bool,

    /// Validates the configuration file and exits.
    #[arg(long)]
    check_config: bool,

    /// Logs detected chatter without grabbing or altering input.
    #[arg(long)]
    dry_run: bool,

    /// Interval between scans for new devices, in seconds.
    #[arg(long, default_value_t = 2)]
    scan_interval: u64,
}

fn main() {
    init_logger();
    if let Err(e) = run(Args::parse()) {
        error!("{e:#}");
        std::process::exit(1);
    }
}

fn init_logger() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    // Under systemd, journald already timestamps every line.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()));
    }
    builder.init();
}

fn load_config(path: &Path) -> Result<Config> {
    if path.exists() {
        Config::load(path)
    } else {
        warn!("{} not found, using default configuration", path.display());
        Ok(Config::default())
    }
}

fn run(args: Args) -> Result<()> {
    let config = load_config(&args.config)?;
    if args.check_config {
        println!("{config:#?}");
        return Ok(());
    }
    if args.list_devices {
        list_devices(&config);
        return Ok(());
    }
    info!(
        "global threshold {} ms, {} per-key threshold(s)",
        config.threshold.as_millis(),
        config.key_thresholds.len()
    );
    supervise(&args, Arc::new(RwLock::new(config)))
}

fn list_devices(config: &Config) {
    let aliases = by_id_aliases();
    let devices = device::enumerate();
    if devices.is_empty() {
        println!("No accessible device: run again with sudo (reading {INPUT_DIR}/event*).");
        return;
    }
    for (path, dev) in devices {
        let kind = if device::is_own_virtual_device(&dev) {
            "chatterux virtual"
        } else if device::is_keyboard(&dev) {
            "keyboard"
        } else if device::is_mouse(&dev) {
            "mouse"
        } else {
            "other"
        };
        let mark = if device::is_selected(&path, &dev, &config.devices) { "*" } else { " " };
        println!("{mark} {:<20} {:<18} {}", path.display(), kind, device::display_name(&dev));
        for alias in aliases.get(&path).into_iter().flatten() {
            println!("  {:<20} {:<18} └ {}", "", "", alias.display());
        }
    }
    println!("\n* = device filtered with the current configuration");
}

/// Maps each /dev/input/eventN to its stable links in /dev/input/by-id.
fn by_id_aliases() -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut aliases: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(Path::new(INPUT_DIR).join("by-id")) else {
        return aliases;
    };
    for link in entries.flatten().map(|e| e.path()) {
        if let Ok(target) = std::fs::canonicalize(&link) {
            aliases.entry(target).or_default().push(link);
        }
    }
    aliases
}

enum Event {
    Reload,
    Quit,
}

fn supervise(args: &Args, config: Arc<RwLock<Config>>) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut signals = Signals::new([SIGHUP, SIGINT, SIGTERM]).context("installing signal handlers")?;
    thread::spawn(move || {
        for sig in signals.forever() {
            let event = if sig == SIGHUP { Event::Reload } else { Event::Quit };
            if tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut active: HashMap<PathBuf, JoinHandle<()>> = HashMap::new();
    // Devices whose filtering stopped (error or disconnection): only retried once they
    // disappear from /dev/input or the configuration is reloaded.
    let mut failed: HashSet<PathBuf> = HashSet::new();
    let interval = Duration::from_secs(args.scan_interval.max(1));

    loop {
        active.retain(|path, handle| {
            let running = !handle.is_finished();
            if !running {
                failed.insert(path.clone());
            }
            running
        });
        failed.retain(|path| path.exists());
        scan(args, &config, &mut active, &failed);

        match rx.recv_timeout(interval) {
            Ok(Event::Reload) => {
                match load_config(&args.config) {
                    Ok(new) => {
                        info!("configuration reloaded (global threshold {} ms)", new.threshold.as_millis());
                        *config.write().unwrap() = new;
                    }
                    Err(e) => error!("reload ignored: {e:#}"),
                }
                failed.clear();
            }
            Ok(Event::Quit) => {
                info!("shutting down");
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("signal handler stopped"),
        }
    }
}

/// Starts a filtering thread for each newly selected device.
fn scan(
    args: &Args,
    config: &Arc<RwLock<Config>>,
    active: &mut HashMap<PathBuf, JoinHandle<()>>,
    failed: &HashSet<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(INPUT_DIR) else {
        return;
    };
    let selection = config.read().unwrap().devices.clone();
    for path in entries.flatten().map(|e| e.path()) {
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node || active.contains_key(&path) || failed.contains(&path) {
            continue;
        }
        let dev = match Device::open(&path) {
            Ok(dev) => dev,
            Err(e) => {
                if e.kind() == ErrorKind::PermissionDenied && !PERMISSION_WARNED.swap(true, Ordering::Relaxed) {
                    warn!("permission denied on {}: chatterux must run as root", path.display());
                }
                continue;
            }
        };
        if !device::is_selected(&path, &dev, &selection) {
            continue;
        }
        let config = Arc::clone(config);
        let dry_run = args.dry_run;
        let thread_path = path.clone();
        let handle = thread::Builder::new()
            .name(format!("chatterux {}", path.display()))
            .spawn(move || {
                let shown = thread_path.display().to_string();
                match device::run(thread_path, dev, config, dry_run) {
                    Ok(()) => info!("{shown}: filtering ended"),
                    Err(e) => warn!("{shown}: filtering stopped: {e:#}"),
                }
            });
        match handle {
            Ok(handle) => {
                active.insert(path, handle);
            }
            Err(e) => error!("{}: failed to start thread: {e}", path.display()),
        }
    }
}
