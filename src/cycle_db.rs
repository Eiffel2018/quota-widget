//! Compact per-quota-reset archives. The main token database remains the
//! incremental source index; these files retain only normalized task events
//! from one weekly quota cycle for fast comparison.
use crate::tokens::TokenEvent;
use rusqlite::{params, types::Value, Connection, Transaction};
use std::{
    io,
    path::{Path, PathBuf},
};

fn sql_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn counter(value: u64) -> Value {
    i64::try_from(value)
        .map(Value::Integer)
        .unwrap_or_else(|_| Value::Blob(value.to_be_bytes().to_vec()))
}

pub struct CycleDb {
    conn: Connection,
    path: PathBuf,
    start: i64,
}

impl CycleDb {
    pub fn open(directory: &Path, start: i64) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let dt = chrono::DateTime::from_timestamp(start, 0).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Invalid quota reset timestamp")
        })?;
        let name = format!(
            "quota-cycle-{}.sqlite3",
            dt.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                .format("%Y%m%d-%H%M%S")
        );
        let path = directory.join(name);
        let conn = Connection::open(&path).map_err(sql_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sql_error)?;
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS cycle_info(singleton INTEGER PRIMARY KEY CHECK(singleton=1), reset_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS cycle_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), backfill_complete INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS cycle_events(id TEXT PRIMARY KEY, timestamp INTEGER NOT NULL, model TEXT NOT NULL, session TEXT NOT NULL, model_assumed INTEGER NOT NULL, reasoning_effort TEXT, response_id TEXT, turn_id TEXT, source_kind TEXT, input INTEGER NOT NULL, cached INTEGER NOT NULL, uncached INTEGER NOT NULL, output INTEGER NOT NULL, reasoning INTEGER NOT NULL, cost_usd REAL, partial_flags TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS cycle_events_time ON cycle_events(timestamp,id);
            CREATE INDEX IF NOT EXISTS cycle_events_model ON cycle_events(model,timestamp);")
            .map_err(sql_error)?;
        conn.execute("INSERT OR IGNORE INTO cycle_state VALUES(1, 0)", [])
            .map_err(sql_error)?;
        conn.execute("INSERT OR IGNORE INTO cycle_info VALUES(1, ?)", [start])
            .map_err(sql_error)?;
        let recorded: i64 = conn
            .query_row(
                "SELECT reset_at FROM cycle_info WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if recorded != start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Cycle archive filename collides with another reset",
            ));
        }
        Ok(Self { conn, path, start })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn needs_backfill(&self) -> io::Result<bool> {
        self.conn
            .query_row(
                "SELECT backfill_complete=0 FROM cycle_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(sql_error)
    }
    pub fn apply_changes(
        &mut self,
        changes: &[(Option<TokenEvent>, Option<TokenEvent>)],
    ) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(sql_error)?;
        for (old, new) in changes {
            if let Some(old) = old.as_ref().filter(|e| e.timestamp >= self.start) {
                tx.execute("DELETE FROM cycle_events WHERE id=?", [&old.id])
                    .map_err(sql_error)?;
            }
            if let Some(event) = new.as_ref().filter(|e| e.timestamp >= self.start) {
                insert_event(&tx, event)?;
            }
        }
        tx.commit().map_err(sql_error)
    }
    pub fn backfill(&mut self, events: &[TokenEvent]) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(sql_error)?;
        for event in events.iter().filter(|e| e.timestamp >= self.start) {
            insert_event(&tx, event)?;
        }
        tx.execute(
            "UPDATE cycle_state SET backfill_complete=1 WHERE singleton=1",
            [],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }
    #[cfg(test)]
    pub fn event_count(&self) -> io::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM cycle_events", [], |r| r.get(0))
            .map_err(sql_error)
    }
}

fn insert_event(tx: &Transaction<'_>, e: &TokenEvent) -> io::Result<()> {
    let flags = serde_json::to_string(&e.partial_flags).map_err(sql_error)?;
    tx.execute("INSERT INTO cycle_events VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET timestamp=excluded.timestamp,model=excluded.model,session=excluded.session,model_assumed=excluded.model_assumed,reasoning_effort=excluded.reasoning_effort,response_id=excluded.response_id,turn_id=excluded.turn_id,source_kind=excluded.source_kind,input=excluded.input,cached=excluded.cached,uncached=excluded.uncached,output=excluded.output,reasoning=excluded.reasoning,cost_usd=excluded.cost_usd,partial_flags=excluded.partial_flags", params![e.id,e.timestamp,e.model,e.session,e.model_assumed,e.reasoning_effort,e.response_id,e.turn_id,e.source_kind,counter(e.input),counter(e.cached),counter(e.uncached),counter(e.output),counter(e.reasoning),e.cost_usd,flags]).map_err(sql_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn event(id: &str, timestamp: i64) -> TokenEvent {
        TokenEvent {
            id: id.into(),
            timestamp,
            model: "gpt-6-luna".into(),
            model_assumed: false,
            reasoning_effort: None,
            session: "thread".into(),
            response_id: Some(id.into()),
            turn_id: None,
            source_kind: None,
            input: 10,
            cached: 0,
            uncached: 10,
            output: 2,
            reasoning: 1,
            cost_usd: Some(0.01),
            coverage_start_timestamp_ms: None,
            partial_flags: vec![],
        }
    }

    #[test]
    fn history_reads_archived_ranges_without_opening_other_cycles_or_creating_files() {
        let dir = TempDir::new().unwrap();
        let start = 1_800_000_000;
        let mut first = CycleDb::open(dir.path(), start).unwrap();
        first
            .backfill(&[event("first", start + 10), event("edge", start + 20)])
            .unwrap();
        let mut second = CycleDb::open(dir.path(), start + 100).unwrap();
        second.backfill(&[event("next", start + 110)]).unwrap();
        let rows = query_archived_events(dir.path(), start + 10, start + 20).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "first");
        assert_eq!(rows[0].cost_usd, Some(0.01));
        assert_eq!(rows[0].input, 10);
        assert_eq!(
            query_archived_events(dir.path(), start, start + 120)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // A rebuilt index identifies real reset files, ignoring a leftover
        // duplicate file whose timestamp would otherwise split the range.
        let conn = Connection::open(dir.path().join("tokens.sqlite3")).unwrap();
        conn.execute_batch("CREATE TABLE quota_cycle_files(cycle_start INTEGER PRIMARY KEY,cycle_file TEXT NOT NULL);").unwrap();
        for db in [&first, &second] {
            conn.execute(
                "INSERT INTO quota_cycle_files VALUES(?,?)",
                params![db.start, db.path().file_name().unwrap().to_str().unwrap()],
            )
            .unwrap();
        }
        let mut stale = CycleDb::open(dir.path(), start + 50).unwrap();
        stale.backfill(&[event("wrong-cycle", start + 60)]).unwrap();
        assert_eq!(
            query_archived_events(dir.path(), start, start + 120)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn cycle_archive_keeps_only_events_at_or_after_reset_and_upserts_by_id() {
        let dir = TempDir::new().unwrap();
        let mut db = CycleDb::open(dir.path(), 1_800_000_000).unwrap();
        db.backfill(&[
            event("before", 1_799_999_999),
            event("inside", 1_800_000_001),
        ])
        .unwrap();
        assert_eq!(db.event_count().unwrap(), 1);
        let changed = event("inside", 1_800_000_002);
        db.apply_changes(&[(Some(event("inside", 1_800_000_001)), Some(changed))])
            .unwrap();
        assert_eq!(db.event_count().unwrap(), 1);
        assert!(!db.needs_backfill().unwrap());
        assert!(db
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("quota-cycle-"));
        let path = db.path().to_owned();
        drop(db);
        let reopened = Connection::open(path).unwrap();
        assert_eq!(
            reopened
                .query_row("SELECT backfill_complete FROM cycle_state", [], |r| r
                    .get::<_, bool>(0))
                .unwrap(),
            true
        );
    }
}

/// Read just the archived reset cycles intersecting [start, end), without creating files.
pub fn query_archived_events(
    directory: &Path,
    start: i64,
    end: i64,
) -> io::Result<Vec<TokenEvent>> {
    let indexed = crate::quota_index::cycle_paths(directory)?;
    let mut cycles = indexed.clone().unwrap_or_default();
    let entries = if indexed.is_none() {
        Some(std::fs::read_dir(directory)?)
    } else {
        None
    };
    for entry in entries.into_iter().flatten() {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Some(stamp) = name
            .strip_prefix("quota-cycle-")
            .and_then(|n| n.strip_suffix(".sqlite3"))
        else {
            continue;
        };
        let Ok(dt) = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S") else {
            continue;
        };
        cycles.push((dt.and_utc().timestamp() - 8 * 3600, path));
    }
    cycles.sort();
    let mut events = Vec::new();
    for (index, (cycle_start, path)) in cycles.iter().enumerate() {
        if *cycle_start >= end
            || cycles
                .get(index + 1)
                .is_some_and(|(next, _)| *next <= start)
        {
            continue;
        }
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql_error)?;
        conn.busy_timeout(std::time::Duration::from_millis(250))
            .map_err(sql_error)?;
        let mut stmt = conn.prepare("SELECT id,timestamp,model,session,model_assumed,reasoning_effort,response_id,turn_id,source_kind,input,cached,uncached,output,reasoning,cost_usd,partial_flags FROM cycle_events WHERE timestamp>=? AND timestamp<? ORDER BY timestamp,id").map_err(sql_error)?;
        let rows = stmt
            .query_map(params![start, end], |r| {
                fn count(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
                    match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Integer(n) if n >= 0 => Ok(n as u64),
                        rusqlite::types::ValueRef::Blob(b) if b.len() == 8 => {
                            Ok(u64::from_be_bytes(b.try_into().unwrap()))
                        }
                        _ => Err(rusqlite::Error::InvalidQuery),
                    }
                }
                let flags: String = r.get(15)?;
                let partial_flags = serde_json::from_str(&flags).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        15,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(TokenEvent {
                    id: r.get(0)?,
                    timestamp: r.get(1)?,
                    model: r.get(2)?,
                    session: r.get(3)?,
                    model_assumed: r.get(4)?,
                    reasoning_effort: r.get(5)?,
                    response_id: r.get(6)?,
                    turn_id: r.get(7)?,
                    source_kind: r.get(8)?,
                    input: count(r, 9)?,
                    cached: count(r, 10)?,
                    uncached: count(r, 11)?,
                    output: count(r, 12)?,
                    reasoning: count(r, 13)?,
                    cost_usd: r.get(14)?,
                    partial_flags,
                    coverage_start_timestamp_ms: None,
                })
            })
            .map_err(sql_error)?;
        for row in rows {
            let event = row.map_err(sql_error)?;
            if !event.is_excluded_auto_review() {
                events.push(event);
            }
        }
    }
    Ok(events)
}
