use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bootwitness::model::{format_timestamp, CurrentStatus, EventRecord};
use bootwitness::monitor::{is_healthy, run_daemon, DaemonConfig};
use bootwitness::platform::{RealSystemProbe, SystemProbe};
use bootwitness::storage::Storage;
use clap::{Parser, Subcommand};

const DEFAULT_DATABASE: &str = "/var/lib/bootwitness/bootwitness.sqlite3";

#[derive(Debug, Parser)]
#[command(name = "bootwitness", version, about)]
struct Cli {
    /// SQLite database path.
    #[arg(long, global = true, default_value = DEFAULT_DATABASE)]
    database: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Linux continuity monitoring loop.
    Daemon {
        /// Durable heartbeat interval in seconds.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        interval: u64,

        /// Emit heartbeat_gap when CLOCK_BOOTTIME advances by more than this multiple.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(2..))]
        gap_factor: u64,
    },

    /// Show the latest persisted monitoring state.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// List continuity and monitor events, newest first.
    History {
        /// Maximum number of events to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Filter by boot_changed, monitor_restarted, heartbeat_gap, or clock_anomaly.
        #[arg(long)]
        kind: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Check that the daemon heartbeat matches the running Linux boot.
    Check {
        /// Maximum accepted heartbeat age in seconds.
        #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..))]
        max_heartbeat_age: u64,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon {
            interval,
            gap_factor,
        } => {
            run_daemon(&DaemonConfig {
                database: cli.database,
                interval: Duration::from_secs(interval),
                gap_factor,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { json } => {
            let storage = Storage::open_read_only(&cli.database)?;
            let status = storage
                .status()?
                .context("database has no monitoring baseline; start the daemon first")?;
            print_status(&status, json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::History { limit, kind, json } => {
            if !(1..=10_000).contains(&limit) {
                bail!("history limit must be between 1 and 10000");
            }
            if let Some(kind) = kind.as_deref() {
                validate_kind(kind)?;
            }
            let storage = Storage::open_read_only(&cli.database)?;
            let events = storage.history(limit, kind.as_deref())?;
            print_history(&events, json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Check {
            max_heartbeat_age,
            json,
        } => {
            let sample = RealSystemProbe.sample()?;
            let storage = Storage::open_read_only(&cli.database)?;
            let status = storage
                .status()?
                .context("database has no monitoring baseline; start the daemon first")?;
            match is_healthy(&status, &sample, Duration::from_secs(max_heartbeat_age)) {
                Ok(()) => {
                    if json {
                        println!("{{\"healthy\":true,\"reason\":null}}");
                    } else {
                        println!("healthy");
                    }
                    Ok(ExitCode::SUCCESS)
                }
                Err(reason) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({ "healthy": false, "reason": reason })
                        );
                    } else {
                        println!("unhealthy: {reason}");
                    }
                    Ok(ExitCode::from(1))
                }
            }
        }
    }
}

fn validate_kind(kind: &str) -> Result<()> {
    match kind {
        "boot_changed" | "monitor_restarted" | "heartbeat_gap" | "clock_anomaly" => Ok(()),
        _ => bail!(
            "invalid event kind {kind:?}; expected boot_changed, monitor_restarted, heartbeat_gap, or clock_anomaly"
        ),
    }
}

fn print_status(status: &CurrentStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    println!(
        "monitor:            {}",
        if status.monitor_active {
            "active"
        } else {
            "inactive"
        }
    );
    println!("boot id:            {}", status.boot_id);
    println!(
        "boot started:       {}",
        format_timestamp(status.boot_started_at_ms)
    );
    println!(
        "first observed:     {}",
        format_timestamp(status.first_seen_at_ms)
    );
    println!(
        "last heartbeat:     {}",
        format_timestamp(status.last_heartbeat_at_ms)
    );
    println!(
        "kernel uptime:      {}",
        format_duration(status.last_boottime_ms)
    );
    println!("monitor generation: {}", status.monitor_generation);
    println!("boot changes:       {}", status.boot_change_count);
    println!("all events:         {}", status.total_event_count);
    if let Some(ended_at_ms) = status.ended_at_ms {
        println!("monitor ended:      {}", format_timestamp(ended_at_ms));
    }
    if let Some(reason) = &status.end_reason {
        println!("end reason:         {reason}");
    }
    Ok(())
}

fn print_history(events: &[EventRecord], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(events)?);
        return Ok(());
    }
    if events.is_empty() {
        println!("no events");
        return Ok(());
    }
    for event in events {
        println!(
            "#{} {} detected={} graceful={}",
            event.id,
            event.kind,
            format_timestamp(event.detected_at_ms),
            event
                .graceful
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        );
        if let Some(previous_boot_id) = &event.previous_boot_id {
            println!("  boot: {previous_boot_id} -> {}", event.current_boot_id);
        } else {
            println!("  boot: {}", event.current_boot_id);
        }
        if let (Some(start), Some(end)) = (event.window_start_ms, event.window_end_ms) {
            println!(
                "  evidence window: {} .. {}",
                format_timestamp(start),
                format_timestamp(end)
            );
        }
        if let Some(details) = &event.details {
            println!("  details: {details}");
        }
    }
    Ok(())
}

fn format_duration(milliseconds: i64) -> String {
    let total_seconds = milliseconds.max(0) / 1_000;
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!("{days}d {hours:02}:{minutes:02}:{seconds:02}")
}
