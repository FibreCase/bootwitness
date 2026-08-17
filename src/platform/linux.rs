use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::model::SystemSample;

const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

pub(super) fn sample() -> Result<SystemSample> {
    let boot_id = fs::read_to_string(BOOT_ID_PATH)
        .with_context(|| format!("failed to read {BOOT_ID_PATH}"))?;
    let boot_id = boot_id.trim().to_owned();
    validate_boot_id(&boot_id)?;

    let observed_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is earlier than the Unix epoch")?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow!("system time does not fit in an i64 millisecond timestamp"))?;

    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` points to valid writable memory and CLOCK_BOOTTIME does
    // not retain the pointer after the call.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut timestamp) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("CLOCK_BOOTTIME failed");
    }
    if timestamp.tv_sec < 0 || timestamp.tv_nsec < 0 {
        bail!("CLOCK_BOOTTIME returned a negative duration");
    }
    let boottime_ms = timestamp
        .tv_sec
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(timestamp.tv_nsec / 1_000_000))
        .ok_or_else(|| anyhow!("CLOCK_BOOTTIME overflow"))?;

    Ok(SystemSample::new(boot_id, observed_at_ms, boottime_ms))
}

fn validate_boot_id(boot_id: &str) -> Result<()> {
    if boot_id.len() != 36
        || !boot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        bail!("kernel returned an invalid boot ID: {boot_id:?}");
    }
    Ok(())
}
