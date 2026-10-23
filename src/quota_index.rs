//! Searchable percentage observations and reset-file routing, beside token data.
use crate::{cancellation, model, tokens::ImportedQuotaObservation};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::{
    io,
    path::{Path, PathBuf},
};

fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

pub(crate) fn initialize(conn: &Connection) -> io::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS quota_index_chunks(ordinal INTEGER PRIMARY KEY,digest BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS quota_observation_index(chunk INTEGER NOT NULL,ordinal INTEGER NOT NULL,window_ordinal INTEGER NOT NULL,observed_at INTEGER NOT NULL,observed_at_ms INTEGER,source_key TEXT NOT NULL,window_key TEXT NOT NULL,remaining TEXT NOT NULL,duration_mins INTEGER,reset_at INTEGER,account_id TEXT,PRIMARY KEY(chunk,ordinal,window_ordinal)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS quota_observation_time ON quota_observation_index(observed_at);
        CREATE TABLE IF NOT EXISTS quota_cycle_files(cycle_start INTEGER PRIMARY KEY,cycle_file TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS quota_change_observations(ordinal INTEGER PRIMARY KEY,observed_at INTEGER NOT NULL,source_key TEXT NOT NULL,window_key TEXT,before_percent TEXT NOT NULL,after_percent TEXT NOT NULL,previous_observed_at INTEGER);
        CREATE INDEX IF NOT EXISTS quota_change_time ON quota_change_observations(observed_at);
        CREATE VIEW IF NOT EXISTS quota_change_index AS SELECT o.*,COALESCE((SELECT c.cycle_file FROM quota_cycle_files c WHERE c.cycle_start<=o.observed_at ORDER BY c.cycle_start DESC LIMIT 1),'tokens.sqlite3') AS cycle_file FROM quota_change_observations o;
        CREATE VIEW IF NOT EXISTS quota_percentage_index AS SELECT o.*,COALESCE((SELECT c.cycle_file FROM quota_cycle_files c WHERE c.cycle_start<=o.observed_at ORDER BY c.cycle_start DESC LIMIT 1),'tokens.sqlite3') AS cycle_file FROM quota_observation_index o;")
        .map_err(error)?;
    // Existing compressed chunks are decoded once, never on a display query.
    let mut stmt=conn.prepare("SELECT q.ordinal,q.digest,q.data FROM quota_chunks q LEFT JOIN quota_index_chunks i ON i.ordinal=q.ordinal WHERE i.digest IS NULL OR i.digest<>q.digest ORDER BY q.ordinal").map_err(error)?;
    let chunks = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(error)?;
    drop(stmt);
    if !chunks.is_empty() {
        let tx = conn.unchecked_transaction().map_err(error)?;
        for (ordinal, digest, data) in chunks {
            cancellation::check()?;
            let observations: Vec<ImportedQuotaObservation> =
                crate::token_db::decode_index_chunk(&data)?;
            sync_chunk(&tx, ordinal, &digest, &observations)?;
        }
        tx.commit().map_err(error)?;
    }
    Ok(())
}

pub(crate) fn sync_chunk(
    tx: &Transaction<'_>,
    chunk: i64,
    digest: &[u8],
    observations: &[ImportedQuotaObservation],
) -> io::Result<()> {
    tx.execute("DELETE FROM quota_observation_index WHERE chunk=?", [chunk])
        .map_err(error)?;
    let mut insert = tx
        .prepare_cached("INSERT INTO quota_observation_index VALUES(?,?,?,?,?,?,?,?,?,?,?)")
        .map_err(error)?;
    for (ordinal, o) in observations.iter().enumerate() {
        cancellation::check()?;
        // Invalid provider snapshots stay in the original evidence; they do not
        // become invented percentages in this derived lookup.
        for (window_ordinal, w) in model::historical_windows(&o.payload)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            insert
                .execute(params![
                    chunk,
                    ordinal as i64,
                    window_ordinal as i64,
                    o.timestamp,
                    o.timestamp_ms,
                    w.source_key,
                    w.window_key,
                    w.remaining,
                    w.duration,
                    w.reset_at,
                    w.account_id
                ])
                .map_err(error)?;
        }
    }
    drop(insert);
    tx.execute("INSERT INTO quota_index_chunks VALUES(?,?) ON CONFLICT(ordinal) DO UPDATE SET digest=excluded.digest",params![chunk,digest]).map_err(error)?;
    Ok(())
}

pub(crate) fn sync_cycles(
    conn: &Connection,
    starts: &[i64],
    history: &[model::HistoryEvent],
) -> io::Result<()> {
    let tx = conn.unchecked_transaction().map_err(error)?;
    tx.execute("DELETE FROM quota_cycle_files", [])
        .map_err(error)?;
    for &start in starts {
        let dt = chrono::DateTime::from_timestamp(start, 0)
            .ok_or_else(|| error("Invalid reset timestamp"))?;
        let filename = format!(
            "quota-cycle-{}.sqlite3",
            dt.with_timezone(&chrono::FixedOffset::east_opt(28800).unwrap())
                .format("%Y%m%d-%H%M%S")
        );
        tx.execute(
            "INSERT INTO quota_cycle_files VALUES(?,?)",
            params![start, filename],
        )
        .map_err(error)?;
    }
    tx.execute("DELETE FROM quota_change_observations", [])
        .map_err(error)?;
    for (ordinal, event) in history.iter().enumerate() {
        cancellation::check()?;
        if matches!(
            event.kind,
            model::HistoryKind::QuotaDecrease | model::HistoryKind::QuotaReset
        ) {
            tx.execute(
                "INSERT INTO quota_change_observations VALUES(?,?,?,?,?,?,?)",
                params![
                    ordinal as i64,
                    event.observed_at_unix,
                    event.source_key,
                    event.window_key,
                    event.before,
                    event.after,
                    event.before_observed_at_unix
                ],
            )
            .map_err(error)?;
        }
    }
    tx.commit().map_err(error)
}

/// None means an older database without a populated index; use directory discovery.
pub fn cycle_paths(directory: &Path) -> io::Result<Option<Vec<(i64, PathBuf)>>> {
    let path = directory.join("tokens.sqlite3");
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(error)?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(error)?;
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='quota_cycle_files'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(error)?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare("SELECT cycle_start,cycle_file FROM quota_cycle_files ORDER BY cycle_start")
        .map_err(error)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(error)?;
    let mut paths = Vec::new();
    for row in rows {
        let (start, name) = row.map_err(error)?;
        if Path::new(&name).components().count() != 1
            || !name.starts_with("quota-cycle-")
            || !name.ends_with(".sqlite3")
        {
            return Err(error("Invalid indexed cycle filename"));
        }
        paths.push((start, directory.join(name)));
    }
    Ok((!paths.is_empty()).then_some(paths))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unchanged_percentages_are_indexed_and_resets_route_by_timestamp() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE quota_chunks(ordinal INTEGER PRIMARY KEY,digest BLOB,data BLOB);",
        )
        .unwrap();
        initialize(&conn).unwrap();
        let reset = 1_800_000_000;
        let observations:Vec<_>=[(reset-1,2),(reset,0),(reset+30,0),(reset+60,1)].into_iter().map(|(timestamp,used)|ImportedQuotaObservation{timestamp,payload:json!({"limit_id":"codex","secondary":{"used_percent":used,"window_minutes":10080,"resets_at":reset+604800}}),origin_session:None,source_id:None,timestamp_ms:Some(timestamp*1000)}).collect();
        let tx = conn.unchecked_transaction().unwrap();
        sync_chunk(&tx, 0, b"one", &observations).unwrap();
        tx.commit().unwrap();
        sync_cycles(&conn, &[reset], &[]).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM quota_percentage_index", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
        assert_eq!(
            conn.query_row(
                "SELECT cycle_file FROM quota_percentage_index WHERE observed_at=?",
                [reset - 1],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "tokens.sqlite3"
        );
        let file: String = conn
            .query_row(
                "SELECT cycle_file FROM quota_percentage_index WHERE observed_at=?",
                [reset],
                |r| r.get(0),
            )
            .unwrap();
        assert!(file.starts_with("quota-cycle-"));
        let tx = conn.unchecked_transaction().unwrap();
        sync_chunk(&tx, 0, b"two", &observations[..1]).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM quota_percentage_index", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
