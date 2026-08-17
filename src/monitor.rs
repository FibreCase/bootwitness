use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::model::{format_timestamp, SystemSample};
use crate::platform::{RealSystemProbe, SystemProbe};
use crate::storage::{StartupDisposition, Storage};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub database: PathBuf,
    pub interval: Duration,
    pub gap_factor: u64,
}

pub fn run_daemon(config: &DaemonConfig) -> Result<()> {
    validate_config(config)?;
    let stopping = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&stopping);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::SeqCst))
        .context("failed to install SIGINT/SIGTERM handler")?;

    let probe = RealSystemProbe;
    let first_sample = probe.sample()?;
    ensure_parent_directory(&config.database)?;
    let _lock = DaemonLock::acquire(&config.database)?;

    let mut storage = Storage::open_or_create(&config.database)?;
    let startup = storage.begin_monitoring(&first_sample)?;
    match startup.disposition {
        StartupDisposition::Baseline => log_message(
            "info",
            &format!(
                "monitoring baseline created boot_id={} generation={}",
                first_sample.boot_id, startup.generation
            ),
        ),
        StartupDisposition::BootChanged { graceful } => log_message(
            "warn",
            &format!(
                "boot continuity change detected boot_id={} graceful={} generation={}",
                first_sample.boot_id, graceful, startup.generation
            ),
        ),
        StartupDisposition::MonitorRestarted { graceful } => log_message(
            "warn",
            &format!(
                "monitor process restarted boot_id={} graceful={} generation={}",
                first_sample.boot_id, graceful, startup.generation
            ),
        ),
    }

    let mut next_heartbeat = Instant::now() + config.interval;
    while !stopping.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next_heartbeat {
            thread::sleep(POLL_INTERVAL.min(next_heartbeat.duration_since(now)));
            continue;
        }

        let sample = probe.sample()?;
        let emitted = storage.heartbeat(
            &sample,
            duration_millis_i64(config.interval)?,
            i64::try_from(config.gap_factor).unwrap_or(i64::MAX),
        )?;
        for kind in emitted {
            log_message(
                "warn",
                &format!("{} detected boot_id={}", kind.as_str(), sample.boot_id),
            );
        }
        next_heartbeat = Instant::now() + config.interval;
    }

    let final_sample = probe
        .sample()
        .context("failed to sample the system during graceful shutdown")?;
    storage.mark_stopped(&final_sample, "signal")?;
    log_message(
        "info",
        &format!(
            "monitor stopped gracefully boot_id={}",
            final_sample.boot_id
        ),
    );
    Ok(())
}

fn validate_config(config: &DaemonConfig) -> Result<()> {
    if config.interval < Duration::from_secs(1) {
        bail!("heartbeat interval must be at least 1 second");
    }
    if config.interval > Duration::from_secs(86_400) {
        bail!("heartbeat interval must not exceed 24 hours");
    }
    if config.gap_factor < 2 {
        bail!("heartbeat gap factor must be at least 2");
    }
    Ok(())
}

fn ensure_parent_directory(database: &Path) -> Result<()> {
    if let Some(parent) = database.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
    }
    Ok(())
}

fn duration_millis_i64(duration: Duration) -> Result<i64> {
    duration
        .as_millis()
        .try_into()
        .context("duration does not fit in i64 milliseconds")
}

fn log_message(level: &str, message: &str) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    let timestamp = now_ms
        .map(format_timestamp)
        .unwrap_or_else(|| "unknown-time".to_owned());
    eprintln!("{timestamp} level={level} {message}");
}

struct DaemonLock {
    #[allow(dead_code)]
    file: File,
}

impl DaemonLock {
    #[cfg(unix)]
    fn acquire(database: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;

        let lock_path = lock_path(database);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open daemon lock {}", lock_path.display()))?;
        // SAFETY: flock only inspects the valid file descriptor for the duration
        // of the call. The File remains alive for the lifetime of DaemonLock.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            bail!(
                "another bootwitness daemon may already be active (lock {}): {error}",
                lock_path.display()
            );
        }
        Ok(Self { file })
    }

    #[cfg(not(unix))]
    fn acquire(_database: &Path) -> Result<Self> {
        bail!("the bootwitness daemon is supported only on Unix systems")
    }
}

fn lock_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

pub fn is_healthy(
    status: &crate::model::CurrentStatus,
    sample: &SystemSample,
    max_heartbeat_age: Duration,
) -> Result<(), String> {
    if status.boot_id != sample.boot_id {
        return Err(format!(
            "database boot ID {} does not match running kernel {}",
            status.boot_id, sample.boot_id
        ));
    }
    if !status.monitor_active {
        return Err("monitor is marked as inactive".to_owned());
    }

    let max_age_ms = i64::try_from(max_heartbeat_age.as_millis()).unwrap_or(i64::MAX);
    let age_ms = sample
        .observed_at_ms
        .saturating_sub(status.last_heartbeat_at_ms);
    if age_ms < -1_000 {
        return Err(format!(
            "last heartbeat is {} ms in the future",
            age_ms.saturating_neg()
        ));
    }
    if age_ms > max_age_ms {
        return Err(format!(
            "last heartbeat is {age_ms} ms old (limit {max_age_ms} ms)"
        ));
    }
    Ok(())
}
