use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};

use crate::model::{CurrentStatus, EventKind, EventRecord, SystemSample};

const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4257_4954; // "BWIT"

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupDisposition {
    Baseline,
    BootChanged { graceful: bool },
    MonitorRestarted { graceful: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupOutcome {
    pub disposition: StartupDisposition,
    pub generation: i64,
}

#[derive(Debug)]
struct StateRow {
    boot_id: String,
    boot_started_at_ms: i64,
    first_seen_at_ms: i64,
    last_seen_at_ms: i64,
    last_boottime_ms: i64,
    active: bool,
    generation: i64,
    ended_at_ms: Option<i64>,
    end_reason: Option<String>,
}

pub struct Storage {
    connection: Connection,
}

impl Storage {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create database directory {}", parent.display())
                })?;
            }
        }

        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .with_context(|| format!("failed to open database {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
        )?;

        let mut storage = Self { connection };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .with_context(|| format!("failed to open database {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;

        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            bail!("unsupported database schema version {version}; expected {SCHEMA_VERSION}");
        }
        Ok(Self { connection })
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                let transaction = self.connection.transaction()?;
                transaction.execute_batch(&format!(
                    "CREATE TABLE boot_sessions (
                        boot_id TEXT PRIMARY KEY NOT NULL,
                        boot_started_at_ms INTEGER NOT NULL,
                        first_seen_at_ms INTEGER NOT NULL,
                        last_seen_at_ms INTEGER NOT NULL,
                        last_boottime_ms INTEGER NOT NULL,
                        monitor_generations INTEGER NOT NULL,
                        ended_at_ms INTEGER,
                        end_reason TEXT
                    );

                    CREATE TABLE current_state (
                        singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
                        boot_id TEXT NOT NULL,
                        boot_started_at_ms INTEGER NOT NULL,
                        first_seen_at_ms INTEGER NOT NULL,
                        last_seen_at_ms INTEGER NOT NULL,
                        last_boottime_ms INTEGER NOT NULL,
                        active INTEGER NOT NULL CHECK (active IN (0, 1)),
                        generation INTEGER NOT NULL,
                        ended_at_ms INTEGER,
                        end_reason TEXT,
                        FOREIGN KEY (boot_id) REFERENCES boot_sessions(boot_id)
                    );

                    CREATE TABLE events (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        fingerprint TEXT NOT NULL UNIQUE,
                        kind TEXT NOT NULL,
                        detected_at_ms INTEGER NOT NULL,
                        previous_boot_id TEXT,
                        current_boot_id TEXT NOT NULL,
                        window_start_ms INTEGER,
                        window_end_ms INTEGER,
                        graceful INTEGER CHECK (graceful IS NULL OR graceful IN (0, 1)),
                        details TEXT
                    );

                    CREATE INDEX events_detected_at_idx
                        ON events(detected_at_ms DESC);
                    CREATE INDEX events_kind_idx
                        ON events(kind, detected_at_ms DESC);

                    PRAGMA application_id = {APPLICATION_ID};
                    PRAGMA user_version = {SCHEMA_VERSION};"
                ))?;
                transaction.commit()?;
            }
            SCHEMA_VERSION => {}
            other => bail!(
                "database schema version {other} is newer than supported version {SCHEMA_VERSION}"
            ),
        }
        Ok(())
    }

    pub fn begin_monitoring(&mut self, sample: &SystemSample) -> Result<StartupOutcome> {
        let transaction = self.connection.transaction()?;
        let previous = query_state(&transaction)?;

        let outcome = match previous {
            None => {
                insert_boot_session(&transaction, sample, 1)?;
                replace_current_state(&transaction, sample, sample.observed_at_ms, 1)?;
                StartupOutcome {
                    disposition: StartupDisposition::Baseline,
                    generation: 1,
                }
            }
            Some(previous) if previous.boot_id != sample.boot_id => {
                let graceful = !previous.active && previous.ended_at_ms.is_some();
                let details = if sample.boot_started_at_ms < previous.last_seen_at_ms {
                    Some("wall clock overlap: the estimated new boot time is earlier than the previous heartbeat")
                } else {
                    None
                };
                insert_event(
                    &transaction,
                    &format!("boot:{}:{}", previous.boot_id, sample.boot_id),
                    EventKind::BootChanged,
                    sample.observed_at_ms,
                    Some(&previous.boot_id),
                    &sample.boot_id,
                    Some(previous.last_seen_at_ms),
                    Some(sample.boot_started_at_ms),
                    Some(graceful),
                    details,
                )?;

                if sample.boot_started_at_ms < previous.last_seen_at_ms {
                    insert_event(
                        &transaction,
                        &format!("clock:boot:{}:{}", previous.boot_id, sample.boot_id),
                        EventKind::ClockAnomaly,
                        sample.observed_at_ms,
                        Some(&previous.boot_id),
                        &sample.boot_id,
                        Some(previous.last_seen_at_ms),
                        Some(sample.boot_started_at_ms),
                        None,
                        Some("wall clock moved backwards across boots"),
                    )?;
                }

                insert_boot_session(&transaction, sample, 1)?;
                replace_current_state(&transaction, sample, sample.observed_at_ms, 1)?;
                StartupOutcome {
                    disposition: StartupDisposition::BootChanged { graceful },
                    generation: 1,
                }
            }
            Some(previous) => {
                let graceful = !previous.active && previous.ended_at_ms.is_some();
                let generation = previous.generation.saturating_add(1);
                insert_event(
                    &transaction,
                    &format!("monitor:{}:{generation}", sample.boot_id),
                    EventKind::MonitorRestarted,
                    sample.observed_at_ms,
                    Some(&previous.boot_id),
                    &sample.boot_id,
                    Some(previous.last_seen_at_ms),
                    Some(sample.observed_at_ms),
                    Some(graceful),
                    Some(if graceful {
                        "monitor restarted after an observed graceful stop"
                    } else {
                        "monitor restarted without an observed graceful stop"
                    }),
                )?;
                transaction.execute(
                    "UPDATE boot_sessions
                     SET last_seen_at_ms = ?2,
                         last_boottime_ms = ?3,
                         monitor_generations = ?4,
                         ended_at_ms = NULL,
                         end_reason = NULL
                     WHERE boot_id = ?1",
                    params![
                        sample.boot_id,
                        sample.observed_at_ms,
                        sample.boottime_ms,
                        generation
                    ],
                )?;
                replace_current_state(&transaction, sample, previous.first_seen_at_ms, generation)?;
                StartupOutcome {
                    disposition: StartupDisposition::MonitorRestarted { graceful },
                    generation,
                }
            }
        };

        transaction.commit()?;
        Ok(outcome)
    }

    pub fn heartbeat(
        &mut self,
        sample: &SystemSample,
        interval_ms: i64,
        gap_factor: i64,
    ) -> Result<Vec<EventKind>> {
        let transaction = self.connection.transaction()?;
        let previous = query_state(&transaction)?
            .context("database has no monitoring baseline; start the daemon first")?;
        if previous.boot_id != sample.boot_id {
            bail!(
                "boot ID changed while the daemon was running ({} -> {}); systemd should restart the daemon",
                previous.boot_id,
                sample.boot_id
            );
        }

        let mut emitted = Vec::new();
        let threshold = interval_ms.saturating_mul(gap_factor);
        let boottime_delta = sample.boottime_ms.saturating_sub(previous.last_boottime_ms);
        if sample.boottime_ms >= previous.last_boottime_ms && boottime_delta > threshold {
            insert_event(
                &transaction,
                &format!(
                    "gap:{}:{}:{}",
                    sample.boot_id, previous.generation, previous.last_boottime_ms
                ),
                EventKind::HeartbeatGap,
                sample.observed_at_ms,
                Some(&sample.boot_id),
                &sample.boot_id,
                Some(previous.last_seen_at_ms),
                Some(sample.observed_at_ms),
                None,
                Some(&format!(
                    "CLOCK_BOOTTIME advanced by {boottime_delta} ms; threshold was {threshold} ms"
                )),
            )?;
            emitted.push(EventKind::HeartbeatGap);
        }

        let clock_went_backwards = sample.observed_at_ms.saturating_add(1_000)
            < previous.last_seen_at_ms
            || sample.boottime_ms < previous.last_boottime_ms;
        if clock_went_backwards {
            insert_event(
                &transaction,
                &format!(
                    "clock:{}:{}:{}",
                    sample.boot_id, previous.generation, previous.last_boottime_ms
                ),
                EventKind::ClockAnomaly,
                sample.observed_at_ms,
                Some(&sample.boot_id),
                &sample.boot_id,
                Some(previous.last_seen_at_ms),
                Some(sample.observed_at_ms),
                None,
                Some("wall clock or CLOCK_BOOTTIME moved backwards"),
            )?;
            emitted.push(EventKind::ClockAnomaly);
        }

        transaction.execute(
            "UPDATE boot_sessions
             SET last_seen_at_ms = ?2, last_boottime_ms = ?3
             WHERE boot_id = ?1",
            params![sample.boot_id, sample.observed_at_ms, sample.boottime_ms],
        )?;
        transaction.execute(
            "UPDATE current_state
             SET last_seen_at_ms = ?2,
                 last_boottime_ms = ?3,
                 active = 1,
                 ended_at_ms = NULL,
                 end_reason = NULL
             WHERE singleton_id = 1 AND boot_id = ?1",
            params![sample.boot_id, sample.observed_at_ms, sample.boottime_ms],
        )?;
        transaction.commit()?;
        Ok(emitted)
    }

    pub fn mark_stopped(&mut self, sample: &SystemSample, reason: &str) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let previous = query_state(&transaction)?
            .context("database has no monitoring baseline; start the daemon first")?;
        if previous.boot_id != sample.boot_id {
            bail!("refusing to mark a different boot as gracefully stopped");
        }
        transaction.execute(
            "UPDATE boot_sessions
             SET last_seen_at_ms = ?2,
                 last_boottime_ms = ?3,
                 ended_at_ms = ?2,
                 end_reason = ?4
             WHERE boot_id = ?1",
            params![
                sample.boot_id,
                sample.observed_at_ms,
                sample.boottime_ms,
                reason
            ],
        )?;
        transaction.execute(
            "UPDATE current_state
             SET last_seen_at_ms = ?2,
                 last_boottime_ms = ?3,
                 active = 0,
                 ended_at_ms = ?2,
                 end_reason = ?4
             WHERE singleton_id = 1 AND boot_id = ?1",
            params![
                sample.boot_id,
                sample.observed_at_ms,
                sample.boottime_ms,
                reason
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn status(&self) -> Result<Option<CurrentStatus>> {
        let Some(state) = query_state(&self.connection)? else {
            return Ok(None);
        };
        let boot_change_count = self.connection.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'boot_changed'",
            [],
            |row| row.get(0),
        )?;
        let total_event_count =
            self.connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(Some(CurrentStatus {
            boot_id: state.boot_id,
            boot_started_at_ms: state.boot_started_at_ms,
            first_seen_at_ms: state.first_seen_at_ms,
            last_heartbeat_at_ms: state.last_seen_at_ms,
            last_boottime_ms: state.last_boottime_ms,
            monitor_active: state.active,
            monitor_generation: state.generation,
            ended_at_ms: state.ended_at_ms,
            end_reason: state.end_reason,
            boot_change_count,
            total_event_count,
        }))
    }

    pub fn history(&self, limit: usize, kind: Option<&str>) -> Result<Vec<EventRecord>> {
        let sql = if kind.is_some() {
            "SELECT id, kind, detected_at_ms, previous_boot_id, current_boot_id,
                    window_start_ms, window_end_ms, graceful, details
             FROM events WHERE kind = ?1 ORDER BY detected_at_ms DESC, id DESC LIMIT ?2"
        } else {
            "SELECT id, kind, detected_at_ms, previous_boot_id, current_boot_id,
                    window_start_ms, window_end_ms, graceful, details
             FROM events ORDER BY detected_at_ms DESC, id DESC LIMIT ?2"
        };
        let mut statement = self.connection.prepare(sql)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(EventRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                detected_at_ms: row.get(2)?,
                previous_boot_id: row.get(3)?,
                current_boot_id: row.get(4)?,
                window_start_ms: row.get(5)?,
                window_end_ms: row.get(6)?,
                graceful: row.get::<_, Option<i64>>(7)?.map(|value| value != 0),
                details: row.get(8)?,
            })
        };

        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match kind {
            Some(kind) => statement.query_map(params![kind, limit], map_row)?,
            None => statement.query_map(params![rusqlite::types::Null, limit], map_row)?,
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

fn query_state(connection: &Connection) -> Result<Option<StateRow>> {
    connection
        .query_row(
            "SELECT boot_id, boot_started_at_ms, first_seen_at_ms,
                    last_seen_at_ms, last_boottime_ms, active, generation,
                    ended_at_ms, end_reason
             FROM current_state WHERE singleton_id = 1",
            [],
            |row| {
                Ok(StateRow {
                    boot_id: row.get(0)?,
                    boot_started_at_ms: row.get(1)?,
                    first_seen_at_ms: row.get(2)?,
                    last_seen_at_ms: row.get(3)?,
                    last_boottime_ms: row.get(4)?,
                    active: row.get::<_, i64>(5)? != 0,
                    generation: row.get(6)?,
                    ended_at_ms: row.get(7)?,
                    end_reason: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn insert_boot_session(
    transaction: &Transaction<'_>,
    sample: &SystemSample,
    generation: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO boot_sessions (
             boot_id, boot_started_at_ms, first_seen_at_ms, last_seen_at_ms,
             last_boottime_ms, monitor_generations, ended_at_ms, end_reason
         ) VALUES (?1, ?2, ?3, ?3, ?4, ?5, NULL, NULL)
         ON CONFLICT(boot_id) DO UPDATE SET
             boot_started_at_ms = excluded.boot_started_at_ms,
             last_seen_at_ms = excluded.last_seen_at_ms,
             last_boottime_ms = excluded.last_boottime_ms,
             monitor_generations = excluded.monitor_generations,
             ended_at_ms = NULL,
             end_reason = NULL",
        params![
            sample.boot_id,
            sample.boot_started_at_ms,
            sample.observed_at_ms,
            sample.boottime_ms,
            generation
        ],
    )?;
    Ok(())
}

fn replace_current_state(
    transaction: &Transaction<'_>,
    sample: &SystemSample,
    first_seen_at_ms: i64,
    generation: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO current_state (
             singleton_id, boot_id, boot_started_at_ms, first_seen_at_ms,
             last_seen_at_ms, last_boottime_ms, active, generation,
             ended_at_ms, end_reason
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, 1, ?6, NULL, NULL)
         ON CONFLICT(singleton_id) DO UPDATE SET
             boot_id = excluded.boot_id,
             boot_started_at_ms = excluded.boot_started_at_ms,
             first_seen_at_ms = excluded.first_seen_at_ms,
             last_seen_at_ms = excluded.last_seen_at_ms,
             last_boottime_ms = excluded.last_boottime_ms,
             active = 1,
             generation = excluded.generation,
             ended_at_ms = NULL,
             end_reason = NULL",
        params![
            sample.boot_id,
            sample.boot_started_at_ms,
            first_seen_at_ms,
            sample.observed_at_ms,
            sample.boottime_ms,
            generation
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_event(
    transaction: &Transaction<'_>,
    fingerprint: &str,
    kind: EventKind,
    detected_at_ms: i64,
    previous_boot_id: Option<&str>,
    current_boot_id: &str,
    window_start_ms: Option<i64>,
    window_end_ms: Option<i64>,
    graceful: Option<bool>,
    details: Option<&str>,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO events (
             fingerprint, kind, detected_at_ms, previous_boot_id, current_boot_id,
             window_start_ms, window_end_ms, graceful, details
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(fingerprint) DO NOTHING",
        params![
            fingerprint,
            kind.as_str(),
            detected_at_ms,
            previous_boot_id,
            current_boot_id,
            window_start_ms,
            window_end_ms,
            graceful.map(i64::from),
            details
        ],
    )?;
    Ok(())
}
