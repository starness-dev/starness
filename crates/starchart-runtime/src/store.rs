//! SQLite persistence store (`:memory:` or WAL file on disk) for multi-instance `starchart serve`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::TraceRecord;

/// Lifecycle state of a hosted statechart instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    /// Non-final state with an active `invoke`, `after` timer, or delayed send in flight.
    Active,
    /// Non-final quiescent state waiting purely on external `on = [...]` events.
    Waiting,
    /// Reached a top-level `final_state` (`done == true`).
    Completed,
    /// Terminated due to a runtime execution error.
    Failed,
    /// Explicitly cancelled/stopped via `DELETE /machines/:machine/instances/:id`.
    Cancelled,
}

impl InstanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "active" | "running" => Some(Self::Active),
            "waiting" | "idle" => Some(Self::Waiting),
            "completed" | "done" => Some(Self::Completed),
            "failed" | "error" => Some(Self::Failed),
            "cancelled" | "canceled" | "stopped" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Returns `true` when the instance will never execute further transitions (`completed`, `failed`, or `cancelled`).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Durable snapshot of one statechart instance stored in SQLite and returned over HTTP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceSnapshot {
    pub machine: String,
    pub id: String,
    pub status: InstanceStatus,
    pub done: bool,
    pub active_states: Vec<String>,
    #[serde(default)]
    pub history: HashMap<String, String>,
    pub context: Value,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Thread-safe SQLite store backing `starchart serve` (`:memory:` by default, or WAL file on disk).
pub struct SqliteStore {
    target: String,
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens an in-memory SQLite database (`target == ":memory:"` or `""`) or a file-backed WAL database.
    pub fn open(target: &str) -> anyhow::Result<Self> {
        let trimmed = target.trim();
        let is_memory = trimmed.is_empty() || trimmed == ":memory:";
        let normalized_target = if is_memory {
            ":memory:".to_owned()
        } else {
            trimmed.to_owned()
        };

        let conn = if is_memory {
            Connection::open_in_memory().context("failed to open in-memory SQLite database")?
        } else {
            let p = Path::new(&normalized_target);
            if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create parent directory `{}` for SQLite database",
                        parent.display()
                    )
                })?;
            }
            Connection::open(p)
                .with_context(|| format!("failed to open SQLite database `{normalized_target}`"))?
        };

        if !is_memory {
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
                .context("failed configuring WAL pragmas on SQLite database")?;
        }
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;

             CREATE TABLE IF NOT EXISTS instances (
                 machine_id     TEXT NOT NULL,
                 instance_id    TEXT NOT NULL,
                 status         TEXT NOT NULL,
                 done           INTEGER NOT NULL,
                 active_states  TEXT NOT NULL,
                 history        TEXT NOT NULL,
                 context        TEXT NOT NULL,
                 output         TEXT NOT NULL,
                 error          TEXT,
                 created_at_ms  INTEGER NOT NULL,
                 updated_at_ms  INTEGER NOT NULL,
                 PRIMARY KEY (machine_id, instance_id)
             );

             CREATE INDEX IF NOT EXISTS idx_instances_machine_status
                 ON instances (machine_id, status, updated_at_ms DESC);

             CREATE TABLE IF NOT EXISTS trace_events (
                 machine_id     TEXT NOT NULL,
                 instance_id    TEXT NOT NULL,
                 seq            INTEGER NOT NULL,
                 t_ms           INTEGER NOT NULL,
                 kind           TEXT NOT NULL,
                 record_json    TEXT NOT NULL,
                 PRIMARY KEY (machine_id, instance_id, seq),
                 FOREIGN KEY (machine_id, instance_id)
                     REFERENCES instances (machine_id, instance_id)
                     ON DELETE CASCADE
             );",
        )
        .context("failed initializing SQLite schema")?;

        Ok(Self {
            target: normalized_target,
            conn: Mutex::new(conn),
        })
    }

    /// Returns `:memory:` or the configured database file path.
    pub fn target(&self) -> &str {
        &self.target
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Inserts a new instance snapshot, returning `Ok(false)` if `(machine, id)` already exists.
    pub fn insert_new_instance(&self, snap: &InstanceSnapshot) -> anyhow::Result<bool> {
        let active_states = serde_json::to_string(&snap.active_states)?;
        let history = serde_json::to_string(&snap.history)?;
        let context = serde_json::to_string(&snap.context)?;
        let output = serde_json::to_string(&snap.output)?;
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO instances (
                machine_id, instance_id, status, done, active_states, history,
                context, output, error, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                snap.machine,
                snap.id,
                snap.status.as_str(),
                i32::from(snap.done),
                active_states,
                history,
                context,
                output,
                snap.error,
                i64::try_from(snap.created_at_ms).unwrap_or(i64::MAX),
                i64::try_from(snap.updated_at_ms).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(inserted > 0)
    }

    /// Inserts or updates an instance snapshot while preserving its original `created_at_ms`.
    pub fn upsert_instance(&self, snap: &InstanceSnapshot) -> anyhow::Result<()> {
        let active_states = serde_json::to_string(&snap.active_states)?;
        let history = serde_json::to_string(&snap.history)?;
        let context = serde_json::to_string(&snap.context)?;
        let output = serde_json::to_string(&snap.output)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO instances (
                machine_id, instance_id, status, done, active_states, history,
                context, output, error, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(machine_id, instance_id) DO UPDATE SET
                status = excluded.status,
                done = excluded.done,
                active_states = excluded.active_states,
                history = excluded.history,
                context = excluded.context,
                output = excluded.output,
                error = excluded.error,
                updated_at_ms = excluded.updated_at_ms",
            params![
                snap.machine,
                snap.id,
                snap.status.as_str(),
                i32::from(snap.done),
                active_states,
                history,
                context,
                output,
                snap.error,
                i64::try_from(snap.created_at_ms).unwrap_or(i64::MAX),
                i64::try_from(snap.updated_at_ms).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    /// Fetches a single instance snapshot by `(machine_id, instance_id)`.
    pub fn get_instance(
        &self,
        machine_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Option<InstanceSnapshot>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT machine_id, instance_id, status, done, active_states, history,
                    context, output, error, created_at_ms, updated_at_ms
               FROM instances
              WHERE machine_id = ?1 AND instance_id = ?2",
        )?;
        stmt.query_row(params![machine_id, instance_id], row_to_snapshot)
            .optional()
            .map_err(Into::into)
    }

    /// Lists instances for `machine_id`, optionally filtered by `status`, newest updates first.
    pub fn list_instances(
        &self,
        machine_id: &str,
        status_filter: Option<InstanceStatus>,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<InstanceSnapshot>> {
        let max_rows = i64::try_from(limit.unwrap_or(1000)).unwrap_or(1000);
        let conn = self.lock();
        let mut out = Vec::new();
        if let Some(st) = status_filter {
            let mut stmt = conn.prepare_cached(
                "SELECT machine_id, instance_id, status, done, active_states, history,
                        context, output, error, created_at_ms, updated_at_ms
                   FROM instances
                  WHERE machine_id = ?1 AND status = ?2
                  ORDER BY updated_at_ms DESC, instance_id ASC
                  LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![machine_id, st.as_str(), max_rows], row_to_snapshot)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let mut stmt = conn.prepare_cached(
                "SELECT machine_id, instance_id, status, done, active_states, history,
                        context, output, error, created_at_ms, updated_at_ms
                   FROM instances
                  WHERE machine_id = ?1
                  ORDER BY updated_at_ms DESC, instance_id ASC
                  LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![machine_id, max_rows], row_to_snapshot)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// Counts instances grouped by `status` for `machine_id`.
    pub fn count_instances_by_status(
        &self,
        machine_id: &str,
    ) -> anyhow::Result<HashMap<String, usize>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT status, COUNT(*) FROM instances WHERE machine_id = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![machine_id], |row| {
            let status: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((status, usize::try_from(count).unwrap_or(0)))
        })?;
        let mut counts = HashMap::new();
        for pair in rows {
            let (k, v) = pair?;
            counts.insert(k, v);
        }
        Ok(counts)
    }

    /// Lists all non-terminal (`active` or `waiting`) instances across all machines for startup rehydration.
    pub fn list_resumable_instances(&self) -> anyhow::Result<Vec<InstanceSnapshot>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT machine_id, instance_id, status, done, active_states, history,
                    context, output, error, created_at_ms, updated_at_ms
               FROM instances
              WHERE status IN ('active', 'waiting')
              ORDER BY created_at_ms ASC, instance_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_snapshot)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Deletes an instance and all of its cascaded `trace_events` rows from SQLite.
    pub fn delete_instance(&self, machine_id: &str, instance_id: &str) -> anyhow::Result<bool> {
        let conn = self.lock();
        let deleted = conn.execute(
            "DELETE FROM instances WHERE machine_id = ?1 AND instance_id = ?2",
            params![machine_id, instance_id],
        )?;
        Ok(deleted > 0)
    }

    /// Appends one or more [`TraceRecord`]s for `(machine_id, instance_id)`.
    pub fn insert_trace_records(
        &self,
        machine_id: &str,
        instance_id: &str,
        records: &[TraceRecord],
    ) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO trace_events (
                    machine_id, instance_id, seq, t_ms, kind, record_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for rec in records {
                let line = rec.to_json_line();
                stmt.execute(params![
                    machine_id,
                    instance_id,
                    i64::try_from(rec.seq).unwrap_or(i64::MAX),
                    i64::try_from(rec.t_ms).unwrap_or(i64::MAX),
                    trace_event_kind(&rec.event),
                    line,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns the highest persisted `seq` for `(machine_id, instance_id)`, or `0` when none exist.
    pub fn max_trace_seq(&self, machine_id: &str, instance_id: &str) -> anyhow::Result<u64> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT COALESCE(MAX(seq), 0) FROM trace_events WHERE machine_id = ?1 AND instance_id = ?2",
        )?;
        let max_seq: i64 = stmt.query_row(params![machine_id, instance_id], |r| r.get(0))?;
        Ok(u64::try_from(max_seq).unwrap_or(0))
    }

    /// Reads ordered `(seq, record_json)` rows for one instance with `seq > after_seq`.
    pub fn read_instance_traces_after(
        &self,
        machine_id: &str,
        instance_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<(u64, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT seq, record_json
               FROM trace_events
              WHERE machine_id = ?1 AND instance_id = ?2 AND seq > ?3
              ORDER BY seq ASC
              LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                machine_id,
                instance_id,
                i64::try_from(after_seq).unwrap_or(i64::MAX),
                i64::try_from(limit).unwrap_or(500),
            ],
            |row| {
                let seq: i64 = row.get(0)?;
                let json_line: String = row.get(1)?;
                Ok((u64::try_from(seq).unwrap_or(0), json_line))
            },
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Reads ordered `(rowid, seq, record_json)` across all machines & instances with `rowid > after_rowid`.
    pub fn read_global_traces_after(
        &self,
        after_rowid: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<(i64, u64, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT rowid, seq, record_json
               FROM trace_events
              WHERE rowid > ?1
              ORDER BY rowid ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![after_rowid, i64::try_from(limit).unwrap_or(500)],
            |row| {
                let rowid: i64 = row.get(0)?;
                let seq: i64 = row.get(1)?;
                let json_line: String = row.get(2)?;
                Ok((rowid, u64::try_from(seq).unwrap_or(0), json_line))
            },
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn trace_event_kind(ev: &crate::TraceEvent) -> &'static str {
    match ev {
        crate::TraceEvent::Started { .. } => "started",
        crate::TraceEvent::Transition { .. } => "transition",
        crate::TraceEvent::EventDropped { .. } => "event_dropped",
        crate::TraceEvent::Error { .. } => "error",
        crate::TraceEvent::Stopped { .. } => "stopped",
        crate::TraceEvent::ServiceStarted { .. } => "service_started",
        crate::TraceEvent::ServiceProgress { .. } => "service_progress",
        crate::TraceEvent::ServiceFinished { .. } => "service_finished",
    }
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceSnapshot> {
    let machine: String = row.get(0)?;
    let id: String = row.get(1)?;
    let status_str: String = row.get(2)?;
    let done_int: i32 = row.get(3)?;
    let active_states_str: String = row.get(4)?;
    let history_str: String = row.get(5)?;
    let context_str: String = row.get(6)?;
    let output_str: String = row.get(7)?;
    let error: Option<String> = row.get(8)?;
    let created_i64: i64 = row.get(9)?;
    let updated_i64: i64 = row.get(10)?;

    let status = InstanceStatus::parse_str(&status_str).unwrap_or(InstanceStatus::Waiting);
    let active_states: Vec<String> = serde_json::from_str(&active_states_str).unwrap_or_default();
    let history: HashMap<String, String> = serde_json::from_str(&history_str).unwrap_or_default();
    let context: Value = serde_json::from_str(&context_str).unwrap_or(Value::Null);
    let output: Value = serde_json::from_str(&output_str).unwrap_or(Value::Null);

    Ok(InstanceSnapshot {
        machine,
        id,
        status,
        done: done_int != 0,
        active_states,
        history,
        context,
        output,
        error,
        created_at_ms: u64::try_from(created_i64).unwrap_or(0),
        updated_at_ms: u64::try_from(updated_i64).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::TraceEvent;

    #[test]
    fn in_memory_sqlite_store_roundtrips_instances_and_cascades_trace_delete() {
        let store = SqliteStore::open(":memory:").expect("open :memory:");
        assert_eq!(store.target(), ":memory:");

        let snap = InstanceSnapshot {
            machine: "deployment_pipeline".into(),
            id: "deploy-42".into(),
            status: InstanceStatus::Waiting,
            done: false,
            active_states: vec!["awaiting_approval".into()],
            history: HashMap::new(),
            context: json!({"approvals": 1}),
            output: Value::Null,
            error: None,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };

        assert!(store.insert_new_instance(&snap).unwrap());
        assert!(!store.insert_new_instance(&snap).unwrap());

        store
            .insert_trace_records(
                "deployment_pipeline",
                "deploy-42",
                &[TraceRecord {
                    seq: 1,
                    t_ms: 0,
                    machine: "deployment_pipeline".into(),
                    instance_id: Some("deploy-42".into()),
                    event: TraceEvent::Started {
                        states: vec!["awaiting_approval".into()],
                        actions: vec![],
                        context: json!({"approvals": 0}),
                        status: "running".into(),
                    },
                }],
            )
            .unwrap();

        assert_eq!(store.max_trace_seq("deployment_pipeline", "deploy-42").unwrap(), 1);
        assert_eq!(store.list_resumable_instances().unwrap().len(), 1);

        let mut completed = snap.clone();
        completed.status = InstanceStatus::Completed;
        completed.done = true;
        completed.active_states = vec!["live".into()];
        completed.output = json!({"status": "live"});
        completed.updated_at_ms = 2500;
        store.upsert_instance(&completed).unwrap();

        let fetched = store
            .get_instance("deployment_pipeline", "deploy-42")
            .unwrap()
            .expect("exists");
        assert_eq!(fetched.status, InstanceStatus::Completed);
        assert!(fetched.done);
        assert_eq!(fetched.created_at_ms, 1000);
        assert_eq!(fetched.updated_at_ms, 2500);
        assert!(store.list_resumable_instances().unwrap().is_empty());

        assert!(store.delete_instance("deployment_pipeline", "deploy-42").unwrap());
        assert_eq!(store.max_trace_seq("deployment_pipeline", "deploy-42").unwrap(), 0);
    }
}
