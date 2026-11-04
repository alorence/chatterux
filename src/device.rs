//! Device detection, uinput cloning and filtering loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    BusType, Device, EventSummary, EventType, InputEvent, InputId, KeyCode, SynchronizationCode,
    UinputAbsSetup,
};
use log::{debug, info, warn};

use crate::config::{Config, DeviceSelection};
use crate::filter::{ChatterFilter, Verdict};

/// Prefix of the virtual devices we create, so we never grab them ourselves.
pub const VIRTUAL_PREFIX: &str = "chatterux";

/// Kernel UINPUT_MAX_NAME_SIZE (80) minus the terminator and the margin required by evdev.
const MAX_NAME_LEN: usize = 78;

/// Maximum time to wait for all keys to be released before grabbing.
const RELEASE_WAIT: Duration = Duration::from_secs(5);

pub fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|keys| {
        [KeyCode::KEY_A, KeyCode::KEY_Z, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER]
            .iter()
            .all(|k| keys.contains(*k))
    })
}

pub fn is_mouse(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|keys| keys.contains(KeyCode::BTN_LEFT))
        && dev.supported_relative_axes().is_some()
}

fn is_mouse_button(key: KeyCode) -> bool {
    (KeyCode::BTN_LEFT.code()..=KeyCode::BTN_TASK.code()).contains(&key.code())
}

pub fn is_own_virtual_device(dev: &Device) -> bool {
    dev.name().is_some_and(|n| n.starts_with(VIRTUAL_PREFIX))
}

/// A pattern matches either the device path (by-id/by-path links are resolved)
/// or a substring of its name (case-insensitive).
fn pattern_matches(pattern: &str, path: &Path, dev: &Device) -> bool {
    if pattern.starts_with('/') {
        return std::fs::canonicalize(pattern).is_ok_and(|p| p == path);
    }
    dev.name()
        .is_some_and(|n| n.to_lowercase().contains(&pattern.to_lowercase()))
}

pub fn is_selected(path: &Path, dev: &Device, sel: &DeviceSelection) -> bool {
    if is_own_virtual_device(dev) || sel.exclude.iter().any(|p| pattern_matches(p, path, dev)) {
        return false;
    }
    if !sel.include.is_empty() {
        return sel.include.iter().any(|p| pattern_matches(p, path, dev));
    }
    is_keyboard(dev) || (sel.mice && is_mouse(dev))
}

/// Lists input devices sorted by path.
pub fn enumerate() -> Vec<(PathBuf, Device)> {
    let mut devices: Vec<_> = evdev::enumerate().collect();
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    devices
}

pub fn display_name(dev: &Device) -> &str {
    dev.name().unwrap_or("<unnamed>")
}

fn virtual_name(dev: &Device) -> String {
    let mut name = format!("{VIRTUAL_PREFIX}: {}", display_name(dev));
    if name.len() > MAX_NAME_LEN {
        let mut end = MAX_NAME_LEN;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    name
}

/// Creates a uinput device mirroring the capabilities of the physical device.
fn clone_device(dev: &Device) -> Result<VirtualDevice> {
    let name = virtual_name(dev);
    let id = dev.input_id();
    let mut builder = VirtualDevice::builder()
        .context("opening /dev/uinput")?
        .name(&name)
        .input_id(InputId::new(BusType::BUS_VIRTUAL, id.vendor(), id.product(), id.version()))
        .with_properties(dev.properties())?;
    if let Some(keys) = dev.supported_keys() {
        builder = builder.with_keys(keys)?;
    }
    if let Some(axes) = dev.supported_relative_axes() {
        builder = builder.with_relative_axes(axes)?;
    }
    if dev.supported_events().contains(EventType::ABSOLUTE) {
        for (axis, info) in dev.get_absinfo()? {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
        }
    }
    if let Some(msc) = dev.misc_properties() {
        builder = builder.with_msc(msc)?;
    }
    Ok(builder.build()?)
}

/// Waits until all keys are released: grabbing a device while a key is held down
/// (e.g. Enter after launching the command) would leave that key stuck.
fn wait_for_release(dev: &Device) -> Result<()> {
    let deadline = Instant::now() + RELEASE_WAIT;
    while dev.get_key_state()?.iter().next().is_some() {
        if Instant::now() > deadline {
            warn!("{}: keys are still held down, grabbing anyway", display_name(dev));
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Filters a device until it is disconnected.
pub fn run(path: PathBuf, mut dev: Device, config: Arc<RwLock<Config>>, dry_run: bool) -> Result<()> {
    let name = display_name(&dev).to_owned();
    let mut output = if dry_run {
        None
    } else {
        let virt = clone_device(&dev).with_context(|| format!("creating virtual clone of {name}"))?;
        wait_for_release(&dev)?;
        dev.grab().with_context(|| format!("grabbing {}", path.display()))?;
        Some(virt)
    };
    info!(
        "{}: filtering {:?}{}",
        path.display(),
        name,
        if dry_run { " (dry run)" } else { "" }
    );

    let mut filter = ChatterFilter::new();
    // Events of the current report, emitted when SYN_REPORT is received.
    let mut frame: Vec<InputEvent> = Vec::new();
    let mut frame_filtered = false;

    loop {
        let events = dev
            .fetch_events()
            .with_context(|| format!("reading {}", path.display()))?;
        for event in events {
            match event.destructure() {
                EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                    // A report whose keys were removed only holds orphan MSC_SCAN
                    // events: no need to forward it.
                    let keep = !frame_filtered
                        || frame.iter().any(|e| e.event_type() != EventType::MISC);
                    if keep && let Some(out) = output.as_mut() {
                        out.emit(&frame)?;
                    }
                    frame.clear();
                    frame_filtered = false;
                }
                EventSummary::Key(_, key, value) => {
                    let (threshold, mice) = {
                        let cfg = config.read().unwrap();
                        (cfg.threshold_for(key), cfg.devices.mice)
                    };
                    if is_mouse_button(key) && !mice {
                        frame.push(event);
                        continue;
                    }
                    match filter.process(key.code(), value, event.timestamp(), threshold) {
                        Verdict::Pass => frame.push(event),
                        Verdict::Chatter(elapsed) => {
                            info!(
                                "{name}: blocked chatter on {key:?} ({} ms after release, threshold {} ms)",
                                elapsed.as_millis(),
                                threshold.as_millis()
                            );
                            frame_filtered = true;
                        }
                        Verdict::Swallow => {
                            debug!("{name}: {key:?} value {value} dropped (blocked press)");
                            frame_filtered = true;
                        }
                    }
                }
                _ => frame.push(event),
            }
        }
    }
}
