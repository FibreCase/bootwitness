use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemSample {
    pub boot_id: String,
    pub observed_at_ms: i64,
    pub boottime_ms: i64,
    pub boot_started_at_ms: i64,
}

impl SystemSample {
    pub fn new(boot_id: impl Into<String>, observed_at_ms: i64, boottime_ms: i64) -> Self {
        Self {
            boot_id: boot_id.into(),
            observed_at_ms,
            boottime_ms,
            boot_started_at_ms: observed_at_ms.saturating_sub(boottime_ms),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    BootChanged,
    MonitorRestarted,
    HeartbeatGap,
    ClockAnomaly,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BootChanged => "boot_changed",
            Self::MonitorRestarted => "monitor_restarted",
            Self::HeartbeatGap => "heartbeat_gap",
            Self::ClockAnomaly => "clock_anomaly",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EventRecord {
    pub id: i64,
    pub kind: String,
    pub detected_at_ms: i64,
    pub previous_boot_id: Option<String>,
    pub current_boot_id: String,
    pub window_start_ms: Option<i64>,
    pub window_end_ms: Option<i64>,
    pub graceful: Option<bool>,
    pub details: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CurrentStatus {
    pub boot_id: String,
    pub boot_started_at_ms: i64,
    pub first_seen_at_ms: i64,
    pub last_heartbeat_at_ms: i64,
    pub last_boottime_ms: i64,
    pub monitor_active: bool,
    pub monitor_generation: i64,
    pub ended_at_ms: Option<i64>,
    pub end_reason: Option<String>,
    pub boot_change_count: i64,
    pub total_event_count: i64,
}

pub fn format_timestamp(unix_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(unix_ms)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("invalid({unix_ms})"))
}
