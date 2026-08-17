use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bootwitness::model::{CurrentStatus, EventKind, SystemSample};
use bootwitness::monitor::is_healthy;
use bootwitness::storage::{StartupDisposition, Storage};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test system clock")
            .as_nanos();
        let sequence = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "bootwitness-test-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("state.sqlite3");
        Self { directory, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn sample(boot_id: &str, observed_at_ms: i64, boottime_ms: i64) -> SystemSample {
    SystemSample::new(boot_id, observed_at_ms, boottime_ms)
}

#[test]
fn first_start_creates_a_baseline_without_an_event() {
    let database = TestDatabase::new();
    let mut storage = Storage::open_or_create(database.path()).unwrap();

    let outcome = storage
        .begin_monitoring(&sample("boot-a", 100_000, 10_000))
        .unwrap();

    assert_eq!(outcome.disposition, StartupDisposition::Baseline);
    let status = storage.status().unwrap().unwrap();
    assert_eq!(status.boot_id, "boot-a");
    assert!(status.monitor_active);
    assert_eq!(status.monitor_generation, 1);
    assert_eq!(status.total_event_count, 0);
    assert!(storage.history(50, None).unwrap().is_empty());
}

#[test]
fn changed_boot_id_records_one_ungraceful_continuity_event() {
    let database = TestDatabase::new();
    let mut storage = Storage::open_or_create(database.path()).unwrap();
    storage
        .begin_monitoring(&sample("boot-a", 100_000, 10_000))
        .unwrap();
    storage
        .heartbeat(&sample("boot-a", 120_000, 30_000), 10_000, 3)
        .unwrap();

    let outcome = storage
        .begin_monitoring(&sample("boot-b", 200_000, 10_000))
        .unwrap();

    assert_eq!(
        outcome.disposition,
        StartupDisposition::BootChanged { graceful: false }
    );
    let events = storage.history(50, Some("boot_changed")).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::BootChanged.as_str());
    assert_eq!(events[0].previous_boot_id.as_deref(), Some("boot-a"));
    assert_eq!(events[0].current_boot_id, "boot-b");
    assert_eq!(events[0].window_start_ms, Some(120_000));
    assert_eq!(events[0].window_end_ms, Some(190_000));
    assert_eq!(events[0].graceful, Some(false));

    storage
        .begin_monitoring(&sample("boot-b", 201_000, 11_000))
        .unwrap();
    assert_eq!(storage.history(50, Some("boot_changed")).unwrap().len(), 1);
}

#[test]
fn signal_stop_marks_the_next_boot_as_graceful() {
    let database = TestDatabase::new();
    let mut storage = Storage::open_or_create(database.path()).unwrap();
    storage
        .begin_monitoring(&sample("boot-a", 100_000, 10_000))
        .unwrap();
    storage
        .mark_stopped(&sample("boot-a", 125_000, 35_000), "signal")
        .unwrap();

    let outcome = storage
        .begin_monitoring(&sample("boot-b", 200_000, 10_000))
        .unwrap();

    assert_eq!(
        outcome.disposition,
        StartupDisposition::BootChanged { graceful: true }
    );
    let event = storage
        .history(1, Some("boot_changed"))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(event.graceful, Some(true));
}

#[test]
fn same_boot_restart_preserves_linux_continuity_and_records_monitor_state() {
    let database = TestDatabase::new();
    let mut storage = Storage::open_or_create(database.path()).unwrap();
    storage
        .begin_monitoring(&sample("boot-a", 100_000, 10_000))
        .unwrap();

    let crashed = storage
        .begin_monitoring(&sample("boot-a", 110_000, 20_000))
        .unwrap();
    assert_eq!(
        crashed.disposition,
        StartupDisposition::MonitorRestarted { graceful: false }
    );

    storage
        .mark_stopped(&sample("boot-a", 120_000, 30_000), "signal")
        .unwrap();
    let restarted = storage
        .begin_monitoring(&sample("boot-a", 130_000, 40_000))
        .unwrap();
    assert_eq!(
        restarted.disposition,
        StartupDisposition::MonitorRestarted { graceful: true }
    );

    let events = storage.history(50, Some("monitor_restarted")).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].graceful, Some(true));
    assert_eq!(events[1].graceful, Some(false));
    assert!(storage
        .history(50, Some("boot_changed"))
        .unwrap()
        .is_empty());
}

#[test]
fn heartbeat_detects_long_gaps_and_backwards_clocks() {
    let database = TestDatabase::new();
    let mut storage = Storage::open_or_create(database.path()).unwrap();
    storage
        .begin_monitoring(&sample("boot-a", 1_000_000, 100_000))
        .unwrap();

    let gap_events = storage
        .heartbeat(&sample("boot-a", 1_005_000, 105_000), 1_000, 3)
        .unwrap();
    assert_eq!(gap_events, vec![EventKind::HeartbeatGap]);

    let clock_events = storage
        .heartbeat(&sample("boot-a", 900_000, 104_000), 1_000, 3)
        .unwrap();
    assert_eq!(clock_events, vec![EventKind::ClockAnomaly]);

    assert_eq!(storage.history(50, Some("heartbeat_gap")).unwrap().len(), 1);
    assert_eq!(storage.history(50, Some("clock_anomaly")).unwrap().len(), 1);
}

#[test]
fn read_only_client_can_inspect_a_live_wal_database() {
    let database = TestDatabase::new();
    let mut writer = Storage::open_or_create(database.path()).unwrap();
    writer
        .begin_monitoring(&sample("boot-a", 100_000, 10_000))
        .unwrap();

    let reader = Storage::open_read_only(database.path()).unwrap();
    let status = reader.status().unwrap().unwrap();
    assert_eq!(status.boot_id, "boot-a");
}

#[test]
fn health_check_requires_matching_boot_active_state_and_fresh_heartbeat() {
    let mut status = CurrentStatus {
        boot_id: "boot-a".to_owned(),
        boot_started_at_ms: 90_000,
        first_seen_at_ms: 100_000,
        last_heartbeat_at_ms: 120_000,
        last_boottime_ms: 30_000,
        monitor_active: true,
        monitor_generation: 1,
        ended_at_ms: None,
        end_reason: None,
        boot_change_count: 0,
        total_event_count: 0,
    };
    let current = sample("boot-a", 125_000, 35_000);
    assert!(is_healthy(&status, &current, Duration::from_secs(10)).is_ok());

    status.monitor_active = false;
    assert!(is_healthy(&status, &current, Duration::from_secs(10))
        .unwrap_err()
        .contains("inactive"));
    status.monitor_active = true;
    status.boot_id = "boot-b".to_owned();
    assert!(is_healthy(&status, &current, Duration::from_secs(10))
        .unwrap_err()
        .contains("does not match"));
    status.boot_id = "boot-a".to_owned();
    assert!(is_healthy(&status, &current, Duration::from_secs(1))
        .unwrap_err()
        .contains("old"));
}
