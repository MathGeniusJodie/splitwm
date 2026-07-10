//! Screen backlight control, writing `/sys/class/backlight` directly —
//! no brightnessctl or D-Bus helper in the loop. Needs write access to
//! the sysfs `brightness` file (the `video` group on a stock udev setup).

use std::fs;
use std::io;
use std::path::Path;

/// Step every backlight device by `delta_percent` of its maximum.
/// Devices that fail (missing files, no write permission) are logged and
/// skipped; a machine with no backlight at all is a silent no-op.
pub fn step(delta_percent: i32) {
    let Ok(entries) = fs::read_dir("/sys/class/backlight") else {
        return;
    };
    for entry in entries.flatten() {
        if let Err(err) = step_device(&entry.path(), delta_percent) {
            tracing::warn!("backlight {:?}: {err}", entry.file_name());
        }
    }
}

fn step_device(dev: &Path, delta_percent: i32) -> io::Result<()> {
    let read = |name: &str| -> io::Result<i64> {
        fs::read_to_string(dev.join(name))?
            .trim()
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    };
    let max = read("max_brightness")?;
    let cur = read("brightness")?;
    let step = max * i64::from(delta_percent) / 100;
    // Never step down to a fully dark panel: floor at 1% of max (at least
    // one raw unit), so the screen stays readable enough to step back up.
    let floor = (max / 100).max(1);
    let new = (cur + step).clamp(floor, max);
    fs::write(dev.join("brightness"), new.to_string())
}
