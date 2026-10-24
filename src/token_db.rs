//! Local SQLite cache. Raw evidence is retained in compressed per-thread shards.
//!
//! This module never discovers session roots, removes the legacy JSON, or treats
//! SQL as a replacement for the accounting rules in `tokens`. The caller holds
//! the data-directory writer lock for every migration/import. Read-only opens do
//! not create a database. Full-range queries are available for compatibility;
//! paged queries do not hydrate historical audit shards.
use crate::cancellation::{self, CancellationToken};
use crate::model::HistoryEvent;
use crate::tokens::{
    self, DirtyExport, Pricing, StoreHeader, ThreadShard, TokenEvent, TokenStore, TokenSummary,
};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use rusqlite::{
    params,
    types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Value, ValueRef},
    Connection, OpenFlags, OptionalExtension, ToSql, Transaction, TransactionBehavior,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const FORMAT_VERSION: i64 = 2;
const QUOTA_CHUNK: usize = 256;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), format_version INTEGER NOT NULL, revision BLOB NOT NULL, active INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS global_parts(name TEXT PRIMARY KEY, digest BLOB NOT NULL, data BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS source_cursors(source_key TEXT PRIMARY KEY, digest BLOB NOT NULL, data BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS quota_chunks(ordinal INTEGER PRIMARY KEY, row_count INTEGER NOT NULL, first_ts INTEGER, last_ts INTEGER, digest BLOB NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS history_chunks(ordinal INTEGER PRIMARY KEY, digest BLOB NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS threads(thread_key INTEGER PRIMARY KEY, thread_id TEXT NOT NULL UNIQUE, digest BLOB NOT NULL, data BLOB NOT NULL, turn_usage BLOB, observation_count INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS strings(sid INTEGER PRIMARY KEY, value TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS events(event_key INTEGER PRIMARY KEY, thread_key INTEGER NOT NULL REFERENCES threads ON DELETE CASCADE, ordinal INTEGER NOT NULL, id TEXT NOT NULL UNIQUE, timestamp INTEGER NOT NULL, model_sid INTEGER NOT NULL REFERENCES strings, assumed INTEGER NOT NULL, effort_sid INTEGER REFERENCES strings, response_id TEXT, turn_id TEXT, source_kind_sid INTEGER REFERENCES strings, input NOT NULL, cached NOT NULL, uncached NOT NULL, output NOT NULL, reasoning NOT NULL, cost_usd REAL, flags_sid INTEGER NOT NULL REFERENCES strings, excluded INTEGER NOT NULL, anonymous_legacy INTEGER NOT NULL, coverage_start_timestamp_ms INTEGER);
CREATE INDEX IF NOT EXISTS event_time ON events(timestamp,id);
CREATE INDEX IF NOT EXISTS event_thread_time ON events(thread_key,timestamp,id);
CREATE INDEX IF NOT EXISTS event_partition_time ON events(thread_key,excluded,anonymous_legacy,timestamp,id);
CREATE INDEX IF NOT EXISTS event_model_time ON events(model_sid,timestamp,id);
CREATE TABLE IF NOT EXISTS thread_policy(thread_id TEXT PRIMARY KEY,response INTEGER NOT NULL,anonymous INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS event_partitions(thread_key INTEGER NOT NULL,excluded INTEGER NOT NULL,anonymous_legacy INTEGER NOT NULL,event_count INTEGER NOT NULL,latest_event_key INTEGER NOT NULL,first_ts INTEGER NOT NULL,PRIMARY KEY(thread_key,excluded,anonymous_legacy)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS day_counts(thread_key INTEGER NOT NULL,day INTEGER NOT NULL,excluded INTEGER NOT NULL,anonymous_legacy INTEGER NOT NULL,event_count INTEGER NOT NULL,PRIMARY KEY(thread_key,day,excluded,anonymous_legacy)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS day_count_day ON day_counts(day,excluded);
CREATE TABLE IF NOT EXISTS model_counts(thread_key INTEGER NOT NULL,model_sid INTEGER NOT NULL,excluded INTEGER NOT NULL,anonymous_legacy INTEGER NOT NULL,event_count INTEGER NOT NULL,PRIMARY KEY(thread_key,model_sid,excluded,anonymous_legacy)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS event_filter_time ON events(excluded,timestamp,id);
CREATE TABLE IF NOT EXISTS operations(operation_id TEXT PRIMARY KEY, digest BLOB NOT NULL, revision BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS migration(singleton INTEGER PRIMARY KEY CHECK(singleton=1), source_json TEXT NOT NULL, fingerprint TEXT NOT NULL, source_bytes BLOB NOT NULL, source_modified TEXT, expected_threads INTEGER NOT NULL, complete INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS migrated_threads(thread_id TEXT PRIMARY KEY, digest BLOB NOT NULL) WITHOUT ROWID;
"#;
const EVENT_SELECT: &str = "SELECT e.id,e.timestamp,m.value,e.assumed,r.value,t.thread_id,e.response_id,e.turn_id,k.value,e.input,e.cached,e.uncached,e.output,e.reasoning,e.cost_usd,f.value,e.coverage_start_timestamp_ms FROM events e JOIN threads t ON t.thread_key=e.thread_key JOIN strings m ON m.sid=e.model_sid LEFT JOIN strings r ON r.sid=e.effort_sid LEFT JOIN strings k ON k.sid=e.source_kind_sid JOIN strings f ON f.sid=e.flags_sid";

#[derive(Debug, Clone, Copy)]
pub struct DbProgress {
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct CommitStats {
    pub threads_written: usize,
    pub events_written: usize,
    pub source_cursors_written: usize,
    pub quota_chunks_written: usize,
    pub compressed_bytes_written: usize,
    pub duplicate: bool,
    pub revision: u64,
    pub event_changes: Vec<(Option<TokenEvent>, Option<TokenEvent>)>,
    pub global_policy_changed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub sha256: String,
    pub bytes: u64,
    pub modified: Option<String>,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct DbCounts {
    pub stored_events: usize,
    pub included_events: usize,
    pub excluded_events: usize,
    pub threads: usize,
    pub observations: usize,
}

/// SQLite's INTEGER is signed. Large counters use an exact eight-byte big-endian
/// blob, never REAL, a negative cast, or an SQL SUM that can overflow.
#[derive(Debug, Clone, Copy)]
struct Counter(u64);
impl ToSql for Counter {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(if let Ok(n) = i64::try_from(self.0) {
            Value::Integer(n)
        } else {
            Value::Blob(self.0.to_be_bytes().to_vec())
        }))
    }
}
impl FromSql for Counter {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Integer(n) if n >= 0 => Ok(Self(n as u64)),
            ValueRef::Blob(bytes) if bytes.len() == 8 => Ok(Self(u64::from_be_bytes(
                bytes.try_into().expect("length checked"),
            ))),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}
fn error(error: impl std::fmt::Display) -> io::Error {
    // Serde and SQLite erase I/O error kinds. Preserve our public cancellation
    // result, without feeding Interrupted into retrying reader/writer helpers.
    cancellation::check()
        .err()
        .unwrap_or_else(|| io::Error::other(error.to_string()))
}

const IO_CHUNK: usize = 64 * 1024;

struct CheckedReader<R>(R);
impl<R: Read> Read for CheckedReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        cancellation::check().map_err(|e| io::Error::other(e.to_string()))?;
        let limit = output.len().min(IO_CHUNK);
        self.0.read(&mut output[..limit])
    }
}
struct CheckedWriter<W> {
    inner: W,
    since_check: usize,
}
impl<W: Write> CheckedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            since_check: IO_CHUNK,
        }
    }
}
impl<W: Write> Write for CheckedWriter<W> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.since_check >= IO_CHUNK {
            cancellation::check().map_err(|e| io::Error::other(e.to_string()))?;
            self.since_check = 0;
        }
        let count = self.inner.write(&input[..input.len().min(IO_CHUNK)])?;
        self.since_check = self.since_check.saturating_add(count);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        cancellation::check().map_err(|e| io::Error::other(e.to_string()))?;
        self.inner.flush()
    }
}
fn checked_row<T>(row: rusqlite::Result<T>) -> io::Result<T> {
    cancellation::check()?;
    row.map_err(error)
}
fn sqlite_busy(attempt: i32) -> bool {
    if attempt >= 500 || cancellation::check().is_err() {
        return false;
    }
    std::thread::sleep(Duration::from_millis(10));
    cancellation::check().is_ok()
}

// Rollback is mandatory cleanup. Suppress the cancelled scope while dropping a
// transaction so the SQLite progress handler cannot interrupt its rollback.
pub(crate) struct DbTransaction<'a>(Option<Transaction<'a>>);
impl<'a> std::ops::Deref for DbTransaction<'a> {
    type Target = Transaction<'a>;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("live transaction")
    }
}
impl DbTransaction<'_> {
    fn commit(self) -> io::Result<()> {
        cancellation::check()?;
        // Keep ownership until COMMIT succeeds so any error uses our protected
        // rollback, including cancellation reported by SQLite during COMMIT.
        self.execute_batch("COMMIT").map_err(error)
    }
}
impl Drop for DbTransaction<'_> {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            CancellationToken::new().scope(|| drop(tx));
        }
    }
}
fn conflict(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, message)
}
fn encode<T: Serialize + ?Sized>(value: &T) -> io::Result<Vec<u8>> {
    cancellation::check()?;
    let mut writer = CheckedWriter::new(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(error)?;
    cancellation::check()?;
    Ok(writer.inner)
}
fn hash(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(IO_CHUNK) {
        cancellation::check()?;
        digest.update(chunk);
    }
    cancellation::check()?;
    Ok(digest.finalize().to_vec())
}
// Preserve the exact transaction receipt digest without allocating a full JSON copy.
fn serialized_hash<T: Serialize + ?Sized>(value: &T) -> io::Result<Vec<u8>> {
    cancellation::check()?;
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            cancellation::check().map_err(|e| io::Error::other(e.to_string()))?;
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = std::io::BufWriter::with_capacity(IO_CHUNK, HashWriter(Sha256::new()));
    serde_json::to_writer(&mut writer, value).map_err(error)?;
    writer.flush().map_err(error)?;
    cancellation::check()?;
    Ok(writer.into_inner().map_err(error)?.0.finalize().to_vec())
}
fn compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    cancellation::check()?;
    let mut out = ZlibEncoder::new(Vec::new(), Compression::new(6));
    for chunk in bytes.chunks(IO_CHUNK) {
        cancellation::check()?;
        out.write_all(chunk)?;
    }
    cancellation::check()?;
    let result = out.finish()?;
    cancellation::check()?;
    Ok(result)
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    cancellation::check()?;
    let value = serde_json::from_reader(BufReader::with_capacity(
        IO_CHUNK,
        CheckedReader(ZlibDecoder::new(bytes)),
    ))
    .map_err(error)?;
    cancellation::check()?;
    Ok(value)
}
fn json_decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    cancellation::check()?;
    let value = if bytes.len() <= IO_CHUNK {
        serde_json::from_slice(bytes).map_err(error)?
    } else {
        serde_json::from_reader(BufReader::with_capacity(IO_CHUNK, CheckedReader(bytes)))
            .map_err(error)?
    };
    cancellation::check()?;
    Ok(value)
}
fn to_value<T: Serialize + ?Sized>(value: &T) -> io::Result<serde_json::Value> {
    // serde_json's in-memory Value serializer has no cooperative I/O hooks.
    // The checked stream bounds cancellation latency for large evidence shards.
    json_decode(&encode(value)?)
}
fn revision_bytes(revision: u64) -> Vec<u8> {
    revision.to_be_bytes().to_vec()
}
fn read_revision(conn: &Connection) -> io::Result<Option<u64>> {
    cancellation::check()?;
    let value: Option<Vec<u8>> = conn
        .query_row("SELECT revision FROM state WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(error)?;
    value
        .map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| error("Invalid database revision"))
        })
        .transpose()
}
fn intern(tx: &Transaction<'_>, value: &str) -> io::Result<i64> {
    cancellation::check()?;
    tx.execute(
        "INSERT INTO strings(value) VALUES(?) ON CONFLICT(value) DO NOTHING",
        [value],
    )
    .map_err(error)?;
    tx.query_row("SELECT sid FROM strings WHERE value=?", [value], |row| {
        row.get(0)
    })
    .map_err(error)
}
fn optional_intern(tx: &Transaction<'_>, value: Option<&str>) -> io::Result<Option<i64>> {
    value.map(|v| intern(tx, v)).transpose()
}
// Effective API/UI/export projection. Raw stored cost remains available only via
// load_thread for exact audit round trips; rates-only commits need no shard rewrite.
fn project_event(
    header: &StoreHeader,
    policy: &tokens::EventPolicy<'_>,
    event: &mut TokenEvent,
) -> bool {
    if !policy.apply(event) {
        return false;
    }
    event.cost_usd = header
        .pricing
        .as_ref()
        .and_then(|pricing| tokens::event_cost(event, pricing));
    true
}
fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenEvent> {
    let flags: String = row.get(15)?;
    let partial_flags = json_decode(flags.as_bytes()).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(15, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(TokenEvent {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        model: row.get(2)?,
        model_assumed: row.get(3)?,
        reasoning_effort: row.get(4)?,
        session: row.get(5)?,
        response_id: row.get(6)?,
        turn_id: row.get(7)?,
        source_kind: row.get(8)?,
        input: row.get::<_, Counter>(9)?.0,
        cached: row.get::<_, Counter>(10)?.0,
        uncached: row.get::<_, Counter>(11)?.0,
        output: row.get::<_, Counter>(12)?.0,
        reasoning: row.get::<_, Counter>(13)?.0,
        cost_usd: row.get(14)?,
        partial_flags,
        coverage_start_timestamp_ms: row.get(16)?,
    })
}

pub struct TokenDb {
    conn: Connection,
    path: PathBuf,
    writable: bool,
    header_cache: RefCell<Option<Arc<StoreHeader>>>,
    projection_header_cache: RefCell<Option<Arc<StoreHeader>>>,
    _shared_read_lock: Option<File>,
}
impl TokenDb {
    pub(crate) fn read_snapshot(&self) -> io::Result<Option<DbTransaction<'_>>> {
        cancellation::check()?;
        if self.conn.is_autocommit() {
            self.conn
                .unchecked_transaction()
                .map(|tx| Some(DbTransaction(Some(tx))))
                .map_err(error)
        } else {
            Ok(None)
        }
    }
    pub fn database_path(legacy: &Path) -> PathBuf {
        legacy.with_extension("sqlite3")
    }
    pub fn open_readonly(path: &Path) -> io::Result<Self> {
        Self::open(path, false)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Explicit low-level open. Only a writable open may create schema.
    pub fn open(path: &Path, writable: bool) -> io::Result<Self> {
        cancellation::check()?;
        let existed = path.exists();
        let flags = if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        // All application writers use writer.lock. Holding it shared makes an
        // immutable open safe only when no uncheckpointed WAL data exists. This
        // prevents a clean read-only CLI inspection from creating WAL/SHM files.
        let read_lock = if !writable {
            path.parent()
                .and_then(|parent| File::open(parent.join("writer.lock")).ok())
                .filter(|file| fs2::FileExt::try_lock_shared(file).is_ok())
        } else {
            None
        };
        let mut wal_name = path.as_os_str().to_os_string();
        wal_name.push("-wal");
        let wal_path = PathBuf::from(wal_name);
        let no_wal = match fs::metadata(&wal_path) {
            Ok(meta) => meta.len() == 0,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        let immutable = read_lock.is_some() && no_wal;
        let conn = if immutable {
            Connection::open_with_flags(immutable_uri(path)?, flags | OpenFlags::SQLITE_OPEN_URI)
        } else {
            Connection::open_with_flags(path, flags)
        }
        .map_err(error)?;
        conn.busy_handler(Some(sqlite_busy)).map_err(error)?;
        conn.progress_handler(1000, Some(|| cancellation::check().is_err()))
            .map_err(error)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(error)?;
        conn.pragma_update(None, "cache_size", -16384_i64)
            .map_err(error)?;
        conn.pragma_update(None, "mmap_size", 0_i64)
            .map_err(error)?;
        if writable {
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(error)?;
            conn.pragma_update(None, "synchronous", "FULL")
                .map_err(error)?;
            conn.pragma_update(None, "wal_autocheckpoint", 1000_i64)
                .map_err(error)?;
            let empty_schema: bool = conn.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type IN ('table','index','view','trigger'))",
                [], |row| row.get(0),
            ).map_err(error)?;
            if !existed || empty_schema {
                // A cancelled first open may leave an empty SQLite file. Schema
                // creation is atomic and can safely restart only when empty.
                let tx = DbTransaction(Some(conn.unchecked_transaction().map_err(error)?));
                tx.execute_batch(SCHEMA).map_err(error)?;
                tx.commit()?;
            }
            crate::quota_index::initialize(&conn)?;
            // Additive migration for existing databases: quota history now lives
            // beside token observations instead of being mirrored to JSON.
            conn.execute_batch("CREATE TABLE IF NOT EXISTS history_chunks(ordinal INTEGER PRIMARY KEY, digest BLOB NOT NULL, data BLOB NOT NULL);")
                .map_err(error)?;
        }
        if existed {
            let version: Option<i64> = conn
                .query_row(
                    "SELECT format_version FROM state WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(error)?;
            if version.is_some_and(|v| v != FORMAT_VERSION) {
                return Err(error(
                    "Unsupported token database format; original files preserved",
                ));
            }
        }
        cancellation::check()?;
        Ok(Self {
            conn,
            path: path.to_owned(),
            writable,
            header_cache: RefCell::new(None),
            projection_header_cache: RefCell::new(None),
            _shared_read_lock: read_lock,
        })
    }
    pub fn is_active(&self) -> io::Result<bool> {
        cancellation::check()?;
        self.conn
            .query_row("SELECT active FROM state WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .optional()
            .map(|value: Option<bool>| value.unwrap_or(false))
            .map_err(error)
    }
    pub fn revision(&self) -> io::Result<Option<u64>> {
        read_revision(&self.conn)
    }
    pub fn header(&self) -> io::Result<StoreHeader> {
        let shared = self.header_shared()?;
        // Move the one full importer header instead of retaining a second copy
        // of every quota payload and cursor in the connection's read cache.
        self.header_cache.borrow_mut().take();
        let header = Arc::try_unwrap(shared).unwrap_or_else(|shared| (*shared).clone());
        cancellation::check()?;
        Ok(header)
    }
    pub fn index_quota_cycles(&self, starts: &[i64], history: &[HistoryEvent]) -> io::Result<()> {
        if !self.writable {
            return Err(error("Quota index is read-only"));
        }
        crate::quota_index::sync_cycles(&self.conn, starts, history)
    }
    pub fn history_events(&self) -> io::Result<Vec<HistoryEvent>> {
        cancellation::check()?;
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='history_chunks')",
            [], |row| row.get(0),
        ).map_err(error)?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        let mut statement = self
            .conn
            .prepare("SELECT data FROM history_chunks ORDER BY ordinal")
            .map_err(error)?;
        for row in statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(error)?
        {
            cancellation::check()?;
            events.extend(decode::<Vec<HistoryEvent>>(&checked_row(row)?)?);
        }
        Ok(events)
    }
    pub fn save_history_events(&self, events: &[HistoryEvent]) -> io::Result<()> {
        cancellation::check()?;
        if !self.writable {
            return Err(error("History database is read-only"));
        }
        let tx = self.conn.unchecked_transaction().map_err(error)?;
        for (ordinal, chunk) in events.chunks(QUOTA_CHUNK).enumerate() {
            cancellation::check()?;
            let body = encode(chunk)?;
            let digest = hash(&body)?;
            let old: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT digest FROM history_chunks WHERE ordinal=?",
                    [ordinal as i64],
                    |row| row.get(0),
                )
                .optional()
                .map_err(error)?;
            if old.as_ref() != Some(&digest) {
                let data = compress(&body)?;
                tx.execute("INSERT INTO history_chunks VALUES(?,?,?) ON CONFLICT(ordinal) DO UPDATE SET digest=excluded.digest,data=excluded.data", params![ordinal as i64,digest,data]).map_err(error)?;
            }
        }
        tx.execute(
            "DELETE FROM history_chunks WHERE ordinal>=?",
            [events.len().div_ceil(QUOTA_CHUNK) as i64],
        )
        .map_err(error)?;
        tx.commit().map_err(error)
    }
    /// Event projection needs attribution policy, never import cursors or quota history.
    pub(crate) fn projection_header_shared(&self) -> io::Result<Arc<StoreHeader>> {
        let revision = self.revision()?;
        if let Some(header) = self.projection_header_cache.borrow().as_ref() {
            if revision == Some(header.revision) {
                return Ok(Arc::clone(header));
            }
        }
        let bytes: Vec<u8> = self
            .conn
            .query_row(
                "SELECT data FROM global_parts WHERE name='header'",
                [],
                |r| r.get(0),
            )
            .map_err(error)?;
        let mut value: serde_json::Value = decode(&bytes)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| error("Invalid stored header"))?;
        object.insert("sources".into(), serde_json::json!({}));
        object.insert("quota".into(), serde_json::json!([]));
        let header: StoreHeader = json_decode(&encode(&value)?)?;
        if revision != Some(header.revision) {
            return Err(error("Header revision does not match transaction revision"));
        }
        let header = Arc::new(header);
        *self.projection_header_cache.borrow_mut() = Some(Arc::clone(&header));
        Ok(header)
    }
    pub(crate) fn header_shared(&self) -> io::Result<Arc<StoreHeader>> {
        let _snapshot = self.read_snapshot()?;
        let revision = self.revision()?;
        if let Some(header) = self.header_cache.borrow().as_ref() {
            if revision == Some(header.revision) {
                return Ok(Arc::clone(header));
            }
        }
        let bytes: Vec<u8> = self
            .conn
            .query_row(
                "SELECT data FROM global_parts WHERE name='header'",
                [],
                |r| r.get(0),
            )
            .map_err(error)?;
        let mut header: serde_json::Value = decode(&bytes)?;
        let object = header
            .as_object_mut()
            .ok_or_else(|| error("Invalid stored header"))?;
        object.insert("sources".into(), serde_json::json!({}));
        object.insert("quota".into(), serde_json::json!([]));
        let mut header: StoreHeader = json_decode(&encode(&header)?)?;
        let mut statement = self
            .conn
            .prepare("SELECT source_key,data FROM source_cursors ORDER BY source_key")
            .map_err(error)?;
        for row in statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(error)?
        {
            let (key, bytes) = checked_row(row)?;
            header.sources.insert(key, decode(&bytes)?);
        }

        let mut statement = self
            .conn
            .prepare("SELECT data FROM quota_chunks ORDER BY ordinal")
            .map_err(error)?;
        for row in statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(error)?
        {
            header
                .quota
                .extend(decode::<Vec<crate::tokens::ImportedQuotaObservation>>(
                    &checked_row(row)?,
                )?);
        }

        if revision != Some(header.revision) {
            return Err(error("Header revision does not match transaction revision"));
        }
        let header = Arc::new(header);
        *self.header_cache.borrow_mut() = Some(Arc::clone(&header));
        Ok(header)
    }
    pub fn thread_ids(&self) -> io::Result<Vec<String>> {
        cancellation::check()?;
        self.conn
            .prepare("SELECT thread_id FROM threads ORDER BY thread_id")
            .map_err(error)?
            .query_map([], |row| row.get(0))
            .map_err(error)?
            .map(checked_row)
            .collect()
    }
    pub fn load_thread(&self, thread_id: &str) -> io::Result<Option<ThreadShard>> {
        let _snapshot = self.read_snapshot()?;
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM threads WHERE thread_id=?",
                [thread_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(error)?;
        let Some(bytes) = bytes else { return Ok(None) };
        let mut shard: ThreadShard = decode(&bytes)?;
        let sql = format!("{EVENT_SELECT} WHERE t.thread_id=? ORDER BY e.ordinal");
        shard.events = self
            .conn
            .prepare(&sql)
            .map_err(error)?
            .query_map([thread_id], event_from_row)
            .map_err(error)?
            .map(checked_row)
            .collect::<io::Result<_>>()?;
        Ok(Some(shard))
    }
    fn scan_events(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        descending: bool,
        mut visit: impl FnMut(TokenEvent) -> io::Result<bool>,
    ) -> io::Result<()> {
        let _snapshot = self.read_snapshot()?;
        let header = self.projection_header_shared()?;
        let policy = header.event_policy();
        let order = if descending { "DESC" } else { "ASC" };
        let mut conditions = Vec::new();
        let mut args = Vec::new();
        if let Some(start) = from {
            conditions.push("e.timestamp>=?");
            args.push(start);
        }
        if let Some(end) = to {
            conditions.push("e.timestamp<?");
            args.push(end);
        }
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sql = format!("{EVENT_SELECT}{filter} ORDER BY e.timestamp {order},e.id {order}");
        let mut statement = self.conn.prepare(&sql).map_err(error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(args), event_from_row)
            .map_err(error)?;
        for row in rows {
            let mut event = checked_row(row)?;
            if project_event(&header, &policy, &mut event) && !visit(event)? {
                break;
            }
        }
        cancellation::check()?;
        Ok(())
    }
    /// Inclusive start and exclusive end, identical to TokenStore::select_events.
    pub fn query_events(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        include_excluded: bool,
    ) -> io::Result<Vec<TokenEvent>> {
        let mut result = Vec::new();
        self.scan_events(from, to, false, |event| {
            if include_excluded || !event.is_excluded_auto_review() {
                result.push(event);
            }
            Ok(true)
        })?;
        Ok(result)
    }
    pub fn has_events_before(&self, timestamp: i64) -> io::Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE timestamp < ?1)",
                [timestamp],
                |row| row.get(0),
            )
            .map_err(error)
    }
    pub fn query_page(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        offset: usize,
        limit: usize,
        include_excluded: bool,
    ) -> io::Result<Vec<TokenEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut seen = 0;
        let mut result = Vec::new();
        self.scan_events(from, to, true, |event| {
            if !include_excluded && event.is_excluded_auto_review() {
                return Ok(true);
            }
            if seen >= offset {
                result.push(event)
            }
            seen += 1;
            Ok(result.len() < limit)
        })?;
        Ok(result)
    }
    pub fn query_summary(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        pricing: &Pricing,
    ) -> io::Result<TokenSummary> {
        let mut result = TokenSummary::default();
        let mut gap = false;
        self.scan_events(from, to, false, |event| {
            if event.is_excluded_auto_review() {
                return Ok(true);
            }
            result.event_count += 1;
            result.input = result.input.saturating_add(event.input);
            result.cached = result.cached.saturating_add(event.cached);
            result.uncached = result.uncached.saturating_add(event.uncached);
            result.output = result.output.saturating_add(event.output);
            result.reasoning = result.reasoning.saturating_add(event.reasoning);
            result.partial_events += usize::from(event.is_partial());
            result.assumed_model_events += usize::from(event.model_assumed);
            match tokens::event_cost(&event, pricing) {
                Some(cost) => result.priced_cost_usd += cost,
                None => result.unpriced_events += 1,
            }
            gap |= event.partial_flags.iter().any(|f| {
                matches!(
                    f.as_str(),
                    "response_coverage_gap" | "mixed_schema_legacy_overlap_suppressed"
                )
            });
            Ok(true)
        })?;
        result.total_tokens = result.input.saturating_add(result.output);
        result.cost_usd = (result.unpriced_events == 0 && !gap).then_some(result.priced_cost_usd);
        Ok(result)
    }
    pub fn days(&self) -> io::Result<Vec<i64>> {
        let _snapshot = self.read_snapshot()?;
        let enabled = !self.projection_header_shared()?.response_threads.is_empty();
        self.conn.prepare("SELECT DISTINCT p.day FROM day_counts p JOIN threads t ON t.thread_key=p.thread_key LEFT JOIN thread_policy q ON q.thread_id=t.thread_id WHERE p.excluded=0 AND (?=0 OR p.anonymous_legacy=0 OR COALESCE(q.anonymous,0)=0) ORDER BY p.day").map_err(error)?.query_map([enabled],|row|row.get(0)).map_err(error)?.map(checked_row).collect()
    }
    pub fn models(&self) -> io::Result<Vec<String>> {
        let _snapshot = self.read_snapshot()?;
        let enabled = !self.projection_header_shared()?.response_threads.is_empty();
        self.conn.prepare("SELECT DISTINCT m.value FROM model_counts p JOIN strings m ON m.sid=p.model_sid JOIN threads t ON t.thread_key=p.thread_key LEFT JOIN thread_policy q ON q.thread_id=t.thread_id WHERE p.excluded=0 AND (?=0 OR p.anonymous_legacy=0 OR COALESCE(q.anonymous,0)=0) ORDER BY m.value").map_err(error)?.query_map([enabled],|row|row.get(0)).map_err(error)?.map(checked_row).collect()
    }
    pub fn latest_per_thread(
        &self,
        include_excluded: bool,
    ) -> io::Result<BTreeMap<String, TokenEvent>> {
        let _snapshot = self.read_snapshot()?;
        let header = self.projection_header_shared()?;
        let policy = header.event_policy();
        let enabled = !header.response_threads.is_empty();
        let sql = format!(
            "{EVENT_SELECT} WHERE e.event_key IN (SELECT event_key FROM (SELECT p.latest_event_key AS event_key,ROW_NUMBER() OVER(PARTITION BY p.thread_key ORDER BY x.timestamp DESC,x.id DESC) AS position FROM event_partitions p JOIN events x ON x.event_key=p.latest_event_key JOIN threads t ON t.thread_key=p.thread_key LEFT JOIN thread_policy q ON q.thread_id=t.thread_id WHERE (?=1 OR p.excluded=0) AND (?=0 OR p.anonymous_legacy=0 OR COALESCE(q.anonymous,0)=0)) WHERE position=1)"
        );
        let mut result = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).map_err(error)?;
        for row in stmt
            .query_map(params![include_excluded, enabled], event_from_row)
            .map_err(error)?
        {
            let mut event = checked_row(row)?;
            if project_event(&header, &policy, &mut event) {
                result.insert(event.session.clone(), event);
            }
        }
        Ok(result)
    }
    /// Inclusive cutoff: a future-dated row must not hide the latest valid row.
    pub fn latest_per_thread_before(
        &self,
        cutoff: i64,
        include_excluded: bool,
    ) -> io::Result<BTreeMap<String, TokenEvent>> {
        let _snapshot = self.read_snapshot()?;
        let header = self.projection_header_shared()?;
        let policy = header.event_policy();
        let enabled = !header.response_threads.is_empty();
        let sql = format!(
            "WITH candidates AS (SELECT p.thread_key,CASE WHEN x.timestamp<=? THEN p.latest_event_key ELSE(SELECT y.event_key FROM events y WHERE y.thread_key=p.thread_key AND y.excluded=p.excluded AND y.anonymous_legacy=p.anonymous_legacy AND y.timestamp<=? ORDER BY y.timestamp DESC,y.id DESC LIMIT 1) END AS event_key FROM event_partitions p JOIN events x ON x.event_key=p.latest_event_key JOIN threads t ON t.thread_key=p.thread_key LEFT JOIN thread_policy q ON q.thread_id=t.thread_id WHERE (?=1 OR p.excluded=0) AND (?=0 OR p.anonymous_legacy=0 OR COALESCE(q.anonymous,0)=0)),ranked AS(SELECT c.event_key,ROW_NUMBER() OVER(PARTITION BY c.thread_key ORDER BY x.timestamp DESC,x.id DESC) AS position FROM candidates c JOIN events x ON x.event_key=c.event_key) {EVENT_SELECT} WHERE e.event_key IN(SELECT event_key FROM ranked WHERE position=1)"
        );
        let mut result = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).map_err(error)?;
        for row in stmt
            .query_map(
                params![cutoff, cutoff, include_excluded, enabled],
                event_from_row,
            )
            .map_err(error)?
        {
            let mut event = checked_row(row)?;
            if project_event(&header, &policy, &mut event) {
                result.insert(event.session.clone(), event);
            }
        }
        Ok(result)
    }
    pub fn turn_usage(&self) -> io::Result<BTreeMap<String, crate::tokens::TaskTurnUsage>> {
        cancellation::check()?;
        let mut result = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT thread_id,turn_usage FROM threads WHERE turn_usage IS NOT NULL")
            .map_err(error)?;
        for row in stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(error)?
        {
            let (id, bytes) = checked_row(row)?;
            result.insert(id, decode(&bytes)?);
        }
        Ok(result)
    }
    pub fn commit(
        &mut self,
        export: &DirtyExport,
        expected_revision: u64,
    ) -> io::Result<CommitStats> {
        let digest = serialized_hash(export)?;
        let operation_id = format!(
            "{expected_revision}:{}:{}",
            export.header.revision,
            hex(&digest)
        );
        self.commit_inner(export, expected_revision, &operation_id, digest)
    }
    pub fn commit_with_operation(
        &mut self,
        export: &DirtyExport,
        expected_revision: u64,
        operation_id: &str,
    ) -> io::Result<CommitStats> {
        let digest = serialized_hash(export)?;
        self.commit_inner(export, expected_revision, operation_id, digest)
    }
    fn commit_inner(
        &mut self,
        export: &DirtyExport,
        expected_revision: u64,
        operation_id: &str,
        payload_digest: Vec<u8>,
    ) -> io::Result<CommitStats> {
        cancellation::check()?;
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Read-only token database",
            ));
        }
        let old_header = self.projection_header_shared().ok();
        cancellation::check()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map(|tx| DbTransaction(Some(tx)))
            .map_err(error)?;
        if let Some((digest, revision)) = tx
            .query_row(
                "SELECT digest,revision FROM operations WHERE operation_id=?",
                [operation_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(error)?
        {
            if digest != payload_digest {
                return Err(conflict("Operation ID reused with different token data"));
            }
            let revision = u64::from_be_bytes(
                revision
                    .as_slice()
                    .try_into()
                    .map_err(|_| error("Invalid receipt revision"))?,
            );
            return Ok(CommitStats {
                duplicate: true,
                revision,
                ..CommitStats::default()
            });
        }
        let current = read_revision(&tx)?.unwrap_or(0);
        if current != expected_revision {
            return Err(conflict(
                "Token database revision changed; reload before retrying",
            ));
        }
        if export.header.revision < current {
            return Err(conflict("Token revision cannot move backwards"));
        }
        let mut stats = CommitStats {
            revision: export.header.revision,
            global_policy_changed: old_header
                .as_ref()
                .is_some_and(|old| !old.same_event_policy(&export.header)),
            ..CommitStats::default()
        };
        persist_header(&tx, &export.header, &mut stats)?;
        for shard in &export.threads {
            cancellation::check()?;
            persist_thread(&tx, shard, &mut stats)?;
        }
        tx.execute("INSERT INTO state VALUES(1,?,?,1) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,active=1",params![FORMAT_VERSION,revision_bytes(export.header.revision)]).map_err(error)?;
        tx.execute(
            "INSERT INTO operations VALUES(?,?,?)",
            params![
                operation_id,
                payload_digest,
                revision_bytes(export.header.revision)
            ],
        )
        .map_err(error)?;
        tx.commit()?;
        *self.header_cache.borrow_mut() = None;
        Ok(stats)
    }
    pub fn export_store(&self) -> io::Result<TokenStore> {
        let _snapshot = self.read_snapshot()?;
        let mut store = TokenStore::from_header(self.header()?);
        for id in self.thread_ids()? {
            cancellation::check()?;
            store.hydrate_thread(
                self.load_thread(&id)?
                    .ok_or_else(|| error("Missing thread during export"))?,
            )?;
        }
        store.finish_hydration_for_export_checked()?;
        cancellation::check()?;
        Ok(store)
    }
    pub fn export_json(&self, destination: &Path) -> io::Result<()> {
        if destination.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Export destination exists; original preserved",
            ));
        }
        self.export_store()?.save_new(destination)
    }
    pub fn integrity_check(&self) -> io::Result<()> {
        cancellation::check()?;
        let check: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(error)?;
        if check != "ok" {
            return Err(error(format!("SQLite integrity check: {check}")));
        }
        let mut stmt = self
            .conn
            .prepare("PRAGMA foreign_key_check")
            .map_err(error)?;
        if stmt
            .query([])
            .map_err(error)?
            .next()
            .map_err(error)?
            .is_some()
        {
            return Err(error("SQLite foreign key validation failed"));
        }
        Ok(())
    }
    pub fn checkpoint(&self) -> io::Result<()> {
        cancellation::check()?;
        if self.writable {
            self.conn
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(error)?;
        }
        Ok(())
    }
    /// Create a consistent SQLite backup, including committed WAL content.
    pub fn backup_to(&self, destination: &Path) -> io::Result<()> {
        if !self.writable {
            return Err(error("SQLite backup requires the writer connection"));
        }
        if destination.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "SQLite backup destination already exists",
            ));
        }
        let path = destination.to_string_lossy();
        self.conn
            .execute("VACUUM INTO ?1", [path.as_ref()])
            .map_err(error)?;
        Ok(())
    }
    pub fn vacuum(&self) -> io::Result<()> {
        if !self.writable {
            return Err(error("SQLite compaction requires the writer connection"));
        }
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .map_err(error)
    }
}

fn persist_header(
    tx: &Transaction<'_>,
    header: &StoreHeader,
    stats: &mut CommitStats,
) -> io::Result<()> {
    cancellation::check()?;
    let mut policy_ids = BTreeSet::new();
    for id in header.response_threads.union(&header.anonymous_threads) {
        cancellation::check()?;
        policy_ids.insert(id.clone());
    }
    for id in &policy_ids {
        cancellation::check()?;
        tx.execute("INSERT INTO thread_policy VALUES(?,?,?) ON CONFLICT(thread_id) DO UPDATE SET response=excluded.response,anonymous=excluded.anonymous WHERE response!=excluded.response OR anonymous!=excluded.anonymous",params![id,header.response_threads.contains(id),header.anonymous_threads.contains(id)]).map_err(error)?;
    }
    let old_ids: Vec<String> = tx
        .prepare("SELECT thread_id FROM thread_policy")
        .map_err(error)?
        .query_map([], |row| row.get(0))
        .map_err(error)?
        .map(checked_row)
        .collect::<io::Result<_>>()?;
    for id in old_ids {
        cancellation::check()?;
        if !policy_ids.contains(&id) {
            tx.execute("DELETE FROM thread_policy WHERE thread_id=?", [id])
                .map_err(error)?;
        }
    }
    let value = header.database_metadata();
    for (key, cursor) in &header.sources {
        cancellation::check()?;
        let body = encode(&to_value(cursor)?)?;
        let digest = hash(&body)?;
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT digest FROM source_cursors WHERE source_key=?",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?;
        if old.as_ref() != Some(&digest) {
            let data = compress(&body)?;
            stats.compressed_bytes_written += data.len();
            tx.execute("INSERT INTO source_cursors VALUES(?,?,?) ON CONFLICT(source_key) DO UPDATE SET digest=excluded.digest,data=excluded.data",params![key,digest,data]).map_err(error)?;
            stats.source_cursors_written += 1;
        }
    }
    // Cursor reset/migration may intentionally remove a cursor; retain exact map.
    let old_keys: Vec<String> = tx
        .prepare("SELECT source_key FROM source_cursors")
        .map_err(error)?
        .query_map([], |r| r.get(0))
        .map_err(error)?
        .map(checked_row)
        .collect::<io::Result<_>>()?;
    for key in old_keys {
        cancellation::check()?;
        if !header.sources.contains_key(&key) {
            tx.execute("DELETE FROM source_cursors WHERE source_key=?", [key])
                .map_err(error)?;
        }
    }
    for (ordinal, chunk) in header.quota.chunks(QUOTA_CHUNK).enumerate() {
        cancellation::check()?;
        let body = encode(chunk)?;
        let digest = hash(&body)?;
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT digest FROM quota_chunks WHERE ordinal=?",
                [ordinal as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?;
        if old.as_ref() != Some(&digest) {
            let data = compress(&body)?;
            stats.compressed_bytes_written += data.len();
            tx.execute("INSERT INTO quota_chunks VALUES(?,?,?,?,?,?) ON CONFLICT(ordinal) DO UPDATE SET row_count=excluded.row_count,first_ts=excluded.first_ts,last_ts=excluded.last_ts,digest=excluded.digest,data=excluded.data",params![ordinal as i64,chunk.len() as i64,chunk.iter().map(|q|q.timestamp).min(),chunk.iter().map(|q|q.timestamp).max(),digest,data]).map_err(error)?;
            crate::quota_index::sync_chunk(tx, ordinal as i64, &digest, chunk)?;
            stats.quota_chunks_written += 1;
        }
    }
    tx.execute(
        "DELETE FROM quota_chunks WHERE ordinal>=?",
        [header.quota.len().div_ceil(QUOTA_CHUNK) as i64],
    )
    .map_err(error)?;
    tx.execute(
        "DELETE FROM quota_observation_index WHERE chunk>=?",
        [header.quota.len().div_ceil(QUOTA_CHUNK) as i64],
    )
    .map_err(error)?;
    tx.execute(
        "DELETE FROM quota_index_chunks WHERE ordinal>=?",
        [header.quota.len().div_ceil(QUOTA_CHUNK) as i64],
    )
    .map_err(error)?;
    let body = encode(&value)?;
    let digest = hash(&body)?;
    let old: Option<Vec<u8>> = tx
        .query_row(
            "SELECT digest FROM global_parts WHERE name='header'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(error)?;
    if old.as_ref() != Some(&digest) {
        let data = compress(&body)?;
        stats.compressed_bytes_written += data.len();
        tx.execute("INSERT INTO global_parts VALUES('header',?,?) ON CONFLICT(name) DO UPDATE SET digest=excluded.digest,data=excluded.data",params![digest,data]).map_err(error)?;
    }
    Ok(())
}
fn persist_thread(
    tx: &Transaction<'_>,
    shard: &ThreadShard,
    stats: &mut CommitStats,
) -> io::Result<()> {
    let digest = hash(&encode(shard)?)?;
    let old: Option<Vec<u8>> = tx
        .query_row(
            "SELECT digest FROM threads WHERE thread_id=?",
            [&shard.thread_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    if old.as_ref() == Some(&digest) {
        return Ok(());
    }
    let mut raw = to_value(shard)?;
    raw.as_object_mut()
        .ok_or_else(|| error("Invalid thread shard"))?
        .insert("events".into(), serde_json::json!([]));
    let data = compress(&encode(&raw)?)?;
    stats.compressed_bytes_written += data.len();
    let turn_usage = shard
        .turn_usage
        .as_ref()
        .map(|usage| encode(usage).and_then(|body| compress(&body)))
        .transpose()?;
    tx.execute("INSERT INTO threads(thread_id,digest,data,turn_usage,observation_count) VALUES(?,?,?,?,?) ON CONFLICT(thread_id) DO UPDATE SET digest=excluded.digest,data=excluded.data,turn_usage=excluded.turn_usage,observation_count=excluded.observation_count",params![shard.thread_id,digest,data,turn_usage,shard.observation_count() as i64]).map_err(error)?;
    let key: i64 = tx
        .query_row(
            "SELECT thread_key FROM threads WHERE thread_id=?",
            [&shard.thread_id],
            |row| row.get(0),
        )
        .map_err(error)?;
    let old_sql = format!("{EVENT_SELECT} WHERE e.thread_key=?");
    let mut old_events: BTreeMap<String, TokenEvent> = tx
        .prepare(&old_sql)
        .map_err(error)?
        .query_map([key], event_from_row)
        .map_err(error)?
        .map(|row| checked_row(row).map(|e| (e.id.clone(), e)))
        .collect::<io::Result<_>>()?;
    for table in ["event_partitions", "day_counts", "model_counts"] {
        cancellation::check()?;
        tx.execute(&format!("DELETE FROM {table} WHERE thread_key=?"), [key])
            .map_err(error)?;
    }
    type PartitionValue = (i64, i64, i64, String, i64);
    let mut partitions: BTreeMap<(bool, bool), PartitionValue> = BTreeMap::new();
    let mut days: BTreeMap<(i64, bool, bool), i64> = BTreeMap::new();
    let mut models: BTreeMap<(i64, bool, bool), i64> = BTreeMap::new();
    tx.execute("DELETE FROM events WHERE thread_key=?", [key])
        .map_err(error)?;
    for (ordinal, event) in shard.events.iter().enumerate() {
        cancellation::check()?;
        let old = old_events.remove(&event.id);
        if old.as_ref() != Some(event) {
            stats.event_changes.push((old, Some(event.clone())));
        }
        if event.session != shard.thread_id {
            return Err(error("Event session does not match its thread shard"));
        }
        if event.cost_usd.is_some_and(|cost| !cost.is_finite()) {
            return Err(error("Non-finite cached cost"));
        }
        let model = intern(tx, &event.model)?;
        let effort = optional_intern(tx, event.reasoning_effort.as_deref())?;
        let kind = optional_intern(tx, event.source_kind.as_deref())?;
        let flags_json = String::from_utf8(encode(&event.partial_flags)?).map_err(error)?;
        let flags = intern(tx, &flags_json)?;
        let excluded = event.is_excluded_auto_review();
        let anonymous = event.source_kind.as_deref() != Some("token_usage_record")
            && event
                .partial_flags
                .iter()
                .any(|flag| flag == "session_identity_missing");
        tx.execute("INSERT INTO events(thread_key,ordinal,id,timestamp,model_sid,assumed,effort_sid,response_id,turn_id,source_kind_sid,input,cached,uncached,output,reasoning,cost_usd,flags_sid,excluded,anonymous_legacy,coverage_start_timestamp_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![key,ordinal as i64,event.id,event.timestamp,model,event.model_assumed,effort,event.response_id,event.turn_id,kind,Counter(event.input),Counter(event.cached),Counter(event.uncached),Counter(event.output),Counter(event.reasoning),event.cost_usd,flags,excluded,anonymous,event.coverage_start_timestamp_ms]).map_err(error)?;
        let event_key = tx.last_insert_rowid();
        let partition = partitions.entry((excluded, anonymous)).or_insert_with(|| {
            (
                0,
                event_key,
                event.timestamp,
                event.id.clone(),
                event.timestamp,
            )
        });
        partition.0 += 1;
        partition.4 = partition.4.min(event.timestamp);
        if (event.timestamp, &event.id) > (partition.2, &partition.3) {
            partition.1 = event_key;
            partition.2 = event.timestamp;
            partition.3 = event.id.clone();
        }
        *days
            .entry((hk_day(event.timestamp), excluded, anonymous))
            .or_default() += 1;
        *models.entry((model, excluded, anonymous)).or_default() += 1;
        stats.events_written += 1;
    }
    for ((excluded, anonymous), (count, latest, _, _, first)) in partitions {
        cancellation::check()?;
        tx.execute(
            "INSERT INTO event_partitions VALUES(?,?,?,?,?,?)",
            params![key, excluded, anonymous, count, latest, first],
        )
        .map_err(error)?;
    }
    for ((day, excluded, anonymous), count) in days {
        cancellation::check()?;
        tx.execute(
            "INSERT INTO day_counts VALUES(?,?,?,?,?)",
            params![key, day, excluded, anonymous, count],
        )
        .map_err(error)?;
    }
    for ((model, excluded, anonymous), count) in models {
        cancellation::check()?;
        tx.execute(
            "INSERT INTO model_counts VALUES(?,?,?,?,?)",
            params![key, model, excluded, anonymous, count],
        )
        .map_err(error)?;
    }
    for old in old_events.into_values() {
        cancellation::check()?;
        stats.event_changes.push((Some(old), None));
    }
    stats.threads_written += 1;
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn fingerprint(path: &Path) -> io::Result<SourceFingerprint> {
    cancellation::check()?;
    let before = fs::metadata(path)?;
    let mut file = BufReader::new(File::open(path)?);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 256 * 1024];
    loop {
        cancellation::check()?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    cancellation::check()?;
    let after = fs::metadata(path)?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(conflict("Legacy token JSON changed while fingerprinting"));
    }
    Ok(SourceFingerprint {
        sha256: hex(&hash.finalize()),
        bytes: after.len(),
        modified: after.modified().ok().map(|t| format!("{t:?}")),
    })
}

impl TokenDb {
    pub fn query_events_all(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        include_excluded: bool,
    ) -> io::Result<Vec<TokenEvent>> {
        self.query_events(from, to, include_excluded)
    }
    /// One task's effective events, ordered by (timestamp, id). Start is
    /// inclusive, end exclusive; `excluded_only` selects the exact review partition.
    pub fn query_task_events(
        &self,
        thread_id: &str,
        from: Option<i64>,
        to: Option<i64>,
        excluded_only: bool,
    ) -> io::Result<Vec<TokenEvent>> {
        self.query_thread_events(thread_id, None, from, to, excluded_only)
    }
    /// Exact recorded turn within one task. `None` selects absent turn IDs only;
    /// callers must keep separate unknown-turn responses distinct by event ID.
    pub fn query_turn_events(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        from: Option<i64>,
        to: Option<i64>,
        excluded_only: bool,
    ) -> io::Result<Vec<TokenEvent>> {
        self.query_thread_events(thread_id, Some(turn_id), from, to, excluded_only)
    }
    fn query_thread_events(
        &self,
        thread_id: &str,
        turn_filter: Option<Option<&str>>,
        from: Option<i64>,
        to: Option<i64>,
        excluded_only: bool,
    ) -> io::Result<Vec<TokenEvent>> {
        let _snapshot = self.read_snapshot()?;
        let header = self.projection_header_shared()?;
        let policy = header.event_policy();
        // Force the existing thread/time index: an excluded/time index could
        // otherwise scan a whole busy day just to find one selected task.
        let select = EVENT_SELECT.replacen(
            "FROM events e",
            "FROM events e INDEXED BY event_thread_time",
            1,
        );
        let mut filter = String::from(
            " WHERE e.thread_key=(SELECT thread_key FROM threads WHERE thread_id=?) AND e.excluded=?",
        );
        let mut args = vec![
            Value::Text(thread_id.to_owned()),
            Value::Integer(i64::from(excluded_only)),
        ];
        if let Some(turn_id) = turn_filter {
            filter.push_str(" AND e.turn_id IS ?");
            args.push(turn_id.map_or(Value::Null, |id| Value::Text(id.to_owned())));
        }
        if let Some(start) = from {
            filter.push_str(" AND e.timestamp>=?");
            args.push(Value::Integer(start));
        }
        if let Some(end) = to {
            filter.push_str(" AND e.timestamp<?");
            args.push(Value::Integer(end));
        }
        let sql = format!("{select}{filter} ORDER BY e.timestamp,e.id");
        let mut statement = self.conn.prepare(&sql).map_err(error)?;
        let mut result = Vec::new();
        for row in statement
            .query_map(rusqlite::params_from_iter(args), event_from_row)
            .map_err(error)?
        {
            let mut event = checked_row(row)?;
            if project_event(&header, &policy, &mut event) {
                result.push(event);
            }
        }
        Ok(result)
    }
    /// UI's exclusion toggle is an exact partition. Filtering precedes offset.
    pub fn query_events_filtered(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        excluded_only: bool,
        offset: usize,
        limit: usize,
    ) -> io::Result<Vec<TokenEvent>> {
        let _snapshot = self.read_snapshot()?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let header = self.projection_header_shared()?;
        let policy = header.event_policy();
        let enabled = !header.response_threads.is_empty();
        let mut filter = String::from(
            " WHERE e.excluded=? AND (?=0 OR e.anonymous_legacy=0 OR NOT EXISTS(SELECT 1 FROM thread_policy q WHERE q.thread_id=t.thread_id AND q.anonymous=1))",
        );
        let mut args = vec![
            Value::Integer(i64::from(excluded_only)),
            Value::Integer(i64::from(enabled)),
        ];
        if let Some(start) = from {
            filter.push_str(" AND e.timestamp>=?");
            args.push(Value::Integer(start));
        }
        if let Some(end) = to {
            filter.push_str(" AND e.timestamp<?");
            args.push(Value::Integer(end));
        }
        args.push(Value::Integer(i64::try_from(limit).map_err(error)?));
        args.push(Value::Integer(i64::try_from(offset).map_err(error)?));
        let sql =
            format!("{EVENT_SELECT}{filter} ORDER BY e.timestamp DESC,e.id DESC LIMIT ? OFFSET ?");
        let mut stmt = self.conn.prepare(&sql).map_err(error)?;
        let mut result = Vec::new();
        for row in stmt
            .query_map(rusqlite::params_from_iter(args), event_from_row)
            .map_err(error)?
        {
            let mut event = checked_row(row)?;
            if project_event(&header, &policy, &mut event) {
                result.push(event);
            }
        }
        Ok(result)
    }
    pub fn count_filtered(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        excluded_only: bool,
    ) -> io::Result<usize> {
        let _snapshot = self.read_snapshot()?;
        let enabled = !self.projection_header_shared()?.response_threads.is_empty();
        let whole_days = from.is_none_or(|value| hk_day(value) == value)
            && to.is_none_or(|value| hk_day(value) == value);
        let (table, time_column, aggregate) = if from.is_none() && to.is_none() {
            ("event_partitions", "", "COALESCE(SUM(p.event_count),0)")
        } else if whole_days {
            ("day_counts", "p.day", "COALESCE(SUM(p.event_count),0)")
        } else {
            ("events", "p.timestamp", "COUNT(*)")
        };
        let mut sql = format!(
            "SELECT {aggregate} FROM {table} p JOIN threads t ON t.thread_key=p.thread_key LEFT JOIN thread_policy q ON q.thread_id=t.thread_id WHERE p.excluded=? AND (?=0 OR p.anonymous_legacy=0 OR COALESCE(q.anonymous,0)=0)"
        );
        let mut args = vec![
            Value::Integer(i64::from(excluded_only)),
            Value::Integer(i64::from(enabled)),
        ];
        if let Some(start) = from {
            sql.push_str(&format!(" AND {time_column}>=?"));
            args.push(Value::Integer(start));
        }
        if let Some(end) = to {
            sql.push_str(&format!(" AND {time_column}<?"));
            args.push(Value::Integer(end));
        }
        let count: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args), |row| row.get(0))
            .map_err(error)?;
        usize::try_from(count).map_err(error)
    }
    pub fn counts(&self) -> io::Result<DbCounts> {
        let _snapshot = self.read_snapshot()?;
        let included = self.count_filtered(None, None, false)?;
        let excluded = self.count_filtered(None, None, true)?;
        let (threads, observations): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(observation_count),0) FROM threads",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(error)?;
        Ok(DbCounts {
            stored_events: included + excluded,
            included_events: included,
            excluded_events: excluded,
            threads: usize::try_from(threads).map_err(error)?,
            observations: usize::try_from(observations).map_err(error)?,
        })
    }
    /// Streaming typed events for main-window/interval reductions. No raw shard
    /// hydration. Returning false stops iteration (e.g. earliest timestamp).
    pub fn for_each_event(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        visit: impl FnMut(TokenEvent) -> io::Result<bool>,
    ) -> io::Result<()> {
        self.scan_events(from, to, false, visit)
    }
    pub fn earliest_included_timestamp(&self) -> io::Result<Option<i64>> {
        let _snapshot = self.read_snapshot()?;
        let enabled = !self.projection_header_shared()?.response_threads.is_empty();
        self.conn.query_row("SELECT MIN(p.first_ts) FROM event_partitions p JOIN threads t ON t.thread_key=p.thread_key LEFT JOIN thread_policy q ON q.thread_id=t.thread_id WHERE p.excluded=0 AND (?=0 OR p.anonymous_legacy=0 OR COALESCE(q.anonymous,0)=0)",[enabled],|row|row.get(0)).map_err(error)
    }
    fn verify_legacy_stamp(&self, legacy: &Path) -> io::Result<()> {
        cancellation::check()?;
        let metadata = match fs::metadata(legacy) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let stamp: Option<(Vec<u8>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT source_bytes,source_modified FROM migration WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(error)?;
        let Some((bytes, modified)) = stamp else {
            return Err(conflict(
                "A legacy tokens.json appeared beside an active SQLite cache. Both are preserved; reconcile the two versions before importing.",
            ));
        };
        let current_modified = metadata.modified().ok().map(|time| format!("{time:?}"));
        if bytes != revision_bytes(metadata.len()) || modified != current_modified {
            return Err(conflict(
                "The preserved tokens.json changed after SQLite migration, possibly from an older application. Both files are preserved; reconcile the legacy changes before continuing.",
            ));
        }
        Ok(())
    }
    /// Runtime SQLite entry point. Never inspects or imports legacy JSON files.
    pub fn open_current(path: &Path, allow_write: bool) -> io::Result<Option<Self>> {
        if !path.exists() && !allow_write {
            return Ok(None);
        }
        let mut db = Self::open(path, allow_write)?;
        if !db.is_active()? {
            if !allow_write {
                return Err(error("Token database is not active"));
            }
            let nonempty: bool = db.conn.query_row("SELECT EXISTS(SELECT 1 FROM state UNION ALL SELECT 1 FROM events UNION ALL SELECT 1 FROM global_parts)", [], |row| row.get(0)).map_err(error)?;
            if nonempty {
                return Err(error(
                    "Token database is not active; existing data preserved",
                ));
            }
            db.commit(
                &DirtyExport {
                    header: TokenStore::default().header_checked()?,
                    threads: Vec::new(),
                },
                0,
            )?;
        }
        Ok(Some(db))
    }
    /// Open an already verified SQLite cache, migrate an explicit legacy JSON,
    /// or create an empty authorized cache. Read-only mode never migrates/creates.
    /// Original JSON bytes are never renamed, deleted, rewritten, or truncated.
    pub fn open_or_migrate(
        legacy: &Path,
        allow_write: bool,
        progress: &mut dyn FnMut(DbProgress),
    ) -> io::Result<Option<Self>> {
        cancellation::check()?;
        let destination = Self::database_path(legacy);
        if destination.exists() {
            let mut db = Self::open(&destination, allow_write)?;
            if !db.is_active()? {
                let empty: bool = db.conn.query_row(
                    "SELECT NOT EXISTS(SELECT 1 FROM state UNION ALL SELECT 1 FROM global_parts UNION ALL SELECT 1 FROM source_cursors UNION ALL SELECT 1 FROM quota_chunks UNION ALL SELECT 1 FROM threads UNION ALL SELECT 1 FROM strings UNION ALL SELECT 1 FROM events UNION ALL SELECT 1 FROM thread_policy UNION ALL SELECT 1 FROM event_partitions UNION ALL SELECT 1 FROM day_counts UNION ALL SELECT 1 FROM model_counts UNION ALL SELECT 1 FROM operations UNION ALL SELECT 1 FROM migration UNION ALL SELECT 1 FROM migrated_threads)",
                    [], |row| row.get(0),
                ).map_err(error)?;
                if allow_write && !legacy.exists() && empty {
                    // Only retry a completely empty first-run cache. Never
                    // reinterpret a partly migrated/nonempty cache as empty.
                    db.commit(
                        &DirtyExport {
                            header: TokenStore::default().header_checked()?,
                            threads: Vec::new(),
                        },
                        0,
                    )?;
                } else {
                    return Err(error(
                        "Token database is not active; original JSON preserved",
                    ));
                }
            }
            db.verify_legacy_stamp(legacy)?;
            return Ok(Some(db));
        }
        if !allow_write {
            return Ok(None);
        }
        if !legacy.exists() {
            let mut db = Self::open(&destination, true)?;
            let store = TokenStore::default();
            db.commit(
                &DirtyExport {
                    header: store.header_checked()?,
                    threads: Vec::new(),
                },
                0,
            )?;
            return Ok(Some(db));
        }
        progress(DbProgress {
            phase: "fingerprinting",
            done: 0,
            total: 0,
        });
        cancellation::check()?;
        let parent = legacy.parent().unwrap_or_else(|| Path::new("."));
        let needed = fs::metadata(legacy)?
            .len()
            .saturating_mul(2)
            .saturating_add(64 * 1024 * 1024);
        let available = fs2::available_space(parent)?;
        if available < needed {
            return Err(error(format!(
                "Not enough free disk for safe token migration: need a conservative {needed} bytes, have {available}. Original JSON preserved."
            )));
        }
        let source = fingerprint(legacy)?;
        progress(DbProgress {
            phase: "loading_legacy",
            done: 0,
            total: 0,
        });
        cancellation::check()?;
        let store = TokenStore::load(legacy)?;
        let header = store.header_checked()?;
        let shards = store.into_thread_shards_checked()?;
        let expected = shards.len();
        let staging = destination.with_extension("migrating.sqlite3");
        let mut db = Self::open(&staging, true)?;
        let previous: Option<(String, Vec<u8>)> = db
            .conn
            .query_row(
                "SELECT fingerprint,source_bytes FROM migration WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(error)?;
        if let Some((digest, bytes)) = previous {
            if digest != source.sha256 || bytes != revision_bytes(source.bytes) {
                return Err(conflict(
                    "Legacy JSON changed since staged migration; original and staged database preserved for review",
                ));
            }
        } else {
            let tx = db
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map(|tx| DbTransaction(Some(tx)))
                .map_err(error)?;
            tx.execute(
                "INSERT INTO migration VALUES(1,?,?,?,?,?,0)",
                params![
                    legacy.to_string_lossy(),
                    source.sha256,
                    revision_bytes(source.bytes),
                    source.modified,
                    expected as i64
                ],
            )
            .map_err(error)?;
            tx.execute(
                "INSERT INTO state VALUES(1,?,?,0)",
                params![FORMAT_VERSION, revision_bytes(header.revision)],
            )
            .map_err(error)?;
            let mut stats = CommitStats::default();
            persist_header(&tx, &header, &mut stats)?;
            tx.commit()?;
        }
        for (index, shard) in shards.iter().enumerate() {
            cancellation::check()?;
            let digest = hash(&encode(shard)?)?;
            let old: Option<Vec<u8>> = db
                .conn
                .query_row(
                    "SELECT digest FROM migrated_threads WHERE thread_id=?",
                    [&shard.thread_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(error)?;
            if old.as_ref() != Some(&digest) {
                let tx = db
                    .conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map(|tx| DbTransaction(Some(tx)))
                    .map_err(error)?;
                let mut stats = CommitStats::default();
                persist_thread(&tx, shard, &mut stats)?;
                tx.execute("INSERT INTO migrated_threads VALUES(?,?) ON CONFLICT(thread_id) DO UPDATE SET digest=excluded.digest",params![shard.thread_id,digest]).map_err(error)?;
                tx.commit()?;
            }
            progress(DbProgress {
                phase: "migrating_threads",
                done: index + 1,
                total: expected,
            });
        }
        progress(DbProgress {
            phase: "validating",
            done: 0,
            total: expected,
        });
        if db.thread_ids()?.len() != expected {
            return Err(error(
                "Migration thread count mismatch; original JSON preserved",
            ));
        }
        if encode(&db.header()?)? != encode(&header)? {
            return Err(error("Migration header mismatch; original JSON preserved"));
        }
        for (index, shard) in shards.iter().enumerate() {
            cancellation::check()?;
            let recovered = db
                .load_thread(&shard.thread_id)?
                .ok_or_else(|| error("Migration shard missing"))?;
            if encode(&recovered)? != encode(shard)? {
                return Err(error(
                    "Migration shard fidelity mismatch; original JSON preserved",
                ));
            }
            progress(DbProgress {
                phase: "validating",
                done: index + 1,
                total: expected,
            });
        }
        db.integrity_check()?;
        if fingerprint(legacy)? != source {
            return Err(conflict(
                "Legacy JSON changed during migration; original and stage preserved",
            ));
        }
        let tx = db
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map(|tx| DbTransaction(Some(tx)))
            .map_err(error)?;
        tx.execute("UPDATE migration SET complete=1 WHERE singleton=1", [])
            .map_err(error)?;
        tx.execute("UPDATE state SET active=1 WHERE singleton=1", [])
            .map_err(error)?;
        tx.commit()?;
        db.checkpoint()?;
        drop(db);
        cancellation::check()?;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staging)?
            .sync_all()?;
        // The data-directory writer lock belongs to the caller. Refuse to replace
        // a final database that appeared unexpectedly rather than discarding it.
        if destination.exists() {
            return Err(conflict(
                "A final database appeared during migration; staged copy preserved",
            ));
        }
        cancellation::check()?;
        fs::rename(&staging, &destination)?;
        #[cfg(unix)]
        {
            if let Some(parent) = destination.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        progress(DbProgress {
            phase: "ready",
            done: expected,
            total: expected,
        });
        Ok(Some(Self::open(&destination, true)?))
    }
}

fn hk_day(timestamp: i64) -> i64 {
    let day = (i128::from(timestamp) + 28800).div_euclid(86400) * 86400 - 28800;
    day.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn immutable_uri(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = path
        .to_str()
        .ok_or_else(|| error("Database path cannot be represented as a SQLite URI"))?
        .as_bytes();
    let mut uri = String::from("file:");
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~')
        {
            uri.push(char::from(*byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    #[test]
    fn streamed_receipt_hash_matches_existing_serialization() {
        let value = serde_json::json!({"quota":[{"primary":{"used_percent":1.25}}],"unicode":"額度", "nested":[null,true,123,"\n"]});
        assert_eq!(
            serialized_hash(&value).unwrap(),
            hash(&encode(&value).unwrap()).unwrap()
        );
    }
    fn fixture_store(dir: &Path) -> TokenStore {
        fs::write(
            dir.join("source.jsonl"),
            include_str!("../fixtures/new_schema_responses.jsonl"),
        )
        .unwrap();
        let mut store = TokenStore::default();
        store.import_dir(dir, &Pricing::demo()).unwrap();
        store
    }
    #[test]
    fn sqlite_cycle_backup_is_consistent_and_never_overwrites() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("tokens.sqlite3");
        let backup = dir.path().join("tokens-backup.sqlite3");
        let db = TokenDb::open(&source, true).unwrap();
        db.backup_to(&backup).unwrap();
        TokenDb::open_readonly(&backup)
            .unwrap()
            .integrity_check()
            .unwrap();
        assert_eq!(
            db.backup_to(&backup).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }
    #[test]
    fn quota_history_round_trips_in_sqlite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history.sqlite3");
        let db = TokenDb::open(&path, true).unwrap();
        let event: HistoryEvent = serde_json::from_value(serde_json::json!({
            "session":"test-session",
            "observed_at_unix":1234,
            "source_key":"codex",
            "epoch":1,
            "kind":"QuotaDecrease",
            "window_label":"primary",
            "before":"50",
            "after":"49",
            "delta":"-1",
            "partial":false
        }))
        .unwrap();
        db.save_history_events(std::slice::from_ref(&event))
            .unwrap();
        assert_eq!(db.history_events().unwrap(), vec![event.clone()]);
        drop(db);
        let db = TokenDb::open_readonly(&path).unwrap();
        assert_eq!(db.history_events().unwrap(), vec![event]);
    }
    fn event(id: &str, thread: &str, n: u64, excluded: bool) -> TokenEvent {
        TokenEvent {
            id: id.into(),
            timestamp: 100,
            model: "gpt-6.1-sol".into(),
            model_assumed: false,
            reasoning_effort: None,
            session: thread.into(),
            response_id: Some(id.into()),
            turn_id: None,
            source_kind: Some("token_usage_record".into()),
            input: n,
            cached: 0,
            uncached: n,
            output: 1,
            reasoning: 0,
            cost_usd: None,
            coverage_start_timestamp_ms: None,
            partial_flags: if excluded {
                vec!["excluded_recorded_codex_auto_review".into()]
            } else {
                vec![]
            },
        }
    }
    fn export(events: Vec<TokenEvent>, revision: u64) -> DirtyExport {
        let mut header = TokenStore::default().header();
        header.revision = revision;
        header.pricing = Some(Pricing::demo());
        let mut grouped: BTreeMap<String, ThreadShard> = BTreeMap::new();
        for event in events {
            grouped
                .entry(event.session.clone())
                .or_insert_with(|| {
                    let mut shard = ThreadShard::default();
                    shard.thread_id = event.session.clone();
                    shard
                })
                .events
                .push(event);
        }
        DirtyExport {
            header,
            threads: grouped.into_values().collect(),
        }
    }
    #[test]
    fn compact_metadata_preserves_all_fields_and_full_header_is_not_retained() {
        let temp = TempDir::new().unwrap();
        let mut db = TokenDb::open(&temp.path().join("tokens.sqlite3"), true).unwrap();
        let data = export(vec![event("response", "thread", 10, false)], 1);
        let mut expected = serde_json::to_value(&data.header).unwrap();
        expected.as_object_mut().unwrap().remove("sources");
        expected.as_object_mut().unwrap().remove("quota");
        assert_eq!(data.header.database_metadata(), expected);
        db.commit(&data, 0).unwrap();
        let restored = db.header().unwrap();
        assert_eq!(restored.database_metadata(), expected);
        assert!(
            db.header_cache.borrow().is_none(),
            "full import evidence must be moved to its owner"
        );
        assert_eq!(db.query_events(None, None, false).unwrap().len(), 1);
    }

    #[test]
    fn bounded_usage_query_does_not_decode_quota_history_or_import_cursors() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        db.commit(&export(vec![event("kept", "thread", 10, false)], 1), 0)
            .unwrap();
        db.conn
            .execute("INSERT INTO quota_chunks VALUES(0,1,0,1,x'00',x'00')", [])
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO source_cursors VALUES('unused',x'00',x'00')",
                [],
            )
            .unwrap();
        assert_eq!(db.query_events(None, None, false).unwrap().len(), 1);
        assert!(
            db.header().is_err(),
            "Full importer header still validates corrupt metadata"
        );
    }

    #[test]
    fn cancelled_empty_cache_initialization_is_safe_to_restart() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join("tokens.json");
        let path = TokenDb::database_path(&legacy);
        // Simulate cancellation after schema creation, before the first atomic commit.
        let mut db = TokenDb::open(&path, true).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            token
                .scope(|| db.commit(&export(Vec::new(), 0), 0))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        drop(db);
        let db = TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
            .unwrap()
            .unwrap();
        assert!(db.is_active().unwrap());
        assert_eq!(db.counts().unwrap().stored_events, 0);
        assert!(!legacy.exists());
        db.integrity_check().unwrap();
        // A rolled-back schema transaction leaves a genuinely empty DB file.
        let empty_path = dir.path().join("empty.sqlite3");
        drop(Connection::open(&empty_path).unwrap());
        TokenDb::open(&empty_path, true)
            .unwrap()
            .integrity_check()
            .unwrap();
        // Unknown/nonempty inactive data is still rejected and preserved.
        let bad_legacy = dir.path().join("nonempty.json");
        let bad_path = TokenDb::database_path(&bad_legacy);
        let bad = TokenDb::open(&bad_path, true).unwrap();
        bad.conn
            .execute("INSERT INTO strings(value) VALUES('preserved')", [])
            .unwrap();
        drop(bad);
        assert!(TokenDb::open_or_migrate(&bad_legacy, true, &mut |_| {}).is_err());
        let bad = TokenDb::open(&bad_path, false).unwrap();
        assert_eq!(
            bad.conn
                .query_row("SELECT value FROM strings", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }

    #[test]
    fn cancelled_busy_wait_stops_before_the_normal_lock_timeout() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tokens.sqlite3");
        let mut db = TokenDb::open(&path, true).unwrap();
        db.commit(&export(Vec::new(), 1), 0).unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let token = CancellationToken::new();
        let other = token.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            other.cancel();
        });
        let started = std::time::Instant::now();
        let result =
            token.scope(|| db.commit(&export(vec![event("new", "thread", 1, false)], 2), 1));
        cancel.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "should not wait the normal five-second busy budget"
        );
        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(db.conn.is_autocommit());
        assert_eq!(db.revision().unwrap(), Some(1));
        db.commit(&export(vec![event("new", "thread", 1, false)], 2), 1)
            .unwrap();
    }

    #[test]
    fn cancelled_migration_preserves_source_and_resumes_at_every_phase() {
        for phase in [
            "fingerprinting",
            "loading_legacy",
            "migrating_threads",
            "validating",
        ] {
            let dir = TempDir::new().unwrap();
            let sources = dir.path().join("sources");
            fs::create_dir(&sources).unwrap();
            let store = fixture_store(&sources);
            let legacy = dir.path().join("tokens.json");
            store.save(&legacy).unwrap();
            let original = fs::read(&legacy).unwrap();
            let token = CancellationToken::new();
            let result = token.scope(|| {
                TokenDb::open_or_migrate(&legacy, true, &mut |status| {
                    if status.phase == phase {
                        token.cancel();
                    }
                })
            });
            assert_eq!(
                result.err().unwrap().kind(),
                io::ErrorKind::Interrupted,
                "{phase}"
            );
            assert_eq!(fs::read(&legacy).unwrap(), original, "{phase}");
            assert!(!TokenDb::database_path(&legacy).exists(), "{phase}");
            if phase == "migrating_threads" || phase == "validating" {
                let stage = TokenDb::database_path(&legacy).with_extension("migrating.sqlite3");
                let staged = TokenDb::open(&stage, false).unwrap();
                staged.integrity_check().unwrap();
                let migrated: i64 = staged
                    .conn
                    .query_row("SELECT COUNT(*) FROM migrated_threads", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert!(migrated >= 1, "a completed shard remains resumable");
            }
            let db = TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
                .unwrap()
                .unwrap();
            assert!(db.is_active().unwrap());
            assert_eq!(db.counts().unwrap().stored_events, store.events().len());
            assert_eq!(fs::read(&legacy).unwrap(), original);
            db.integrity_check().unwrap();
        }
    }

    #[test]
    fn cancelled_sqlite_write_rolls_back_all_rows_and_operation_receipt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tokens.sqlite3");
        let mut db = TokenDb::open(&path, true).unwrap();
        db.commit(&export(vec![event("old", "thread", 10, false)], 1), 0)
            .unwrap();
        let next = export(
            (0..200)
                .map(|i| event(&format!("new-{i}"), "thread", i, false))
                .collect(),
            2,
        );
        let token = CancellationToken::new();
        let hook_token = token.clone();
        let mut inserted = 0;
        db.conn
            .update_hook(Some(move |action, _: &str, table: &str, _: i64| {
                if action == rusqlite::hooks::Action::SQLITE_INSERT && table == "events" {
                    inserted += 1;
                    if inserted == 2 {
                        hook_token.cancel();
                    }
                }
            }))
            .unwrap();
        let error = token
            .scope(|| db.commit_with_operation(&next, 1, "cancel-retry"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            db.conn.is_autocommit(),
            "rollback must complete in the cancelled scope"
        );
        assert_eq!(db.revision().unwrap(), Some(1));
        assert_eq!(
            db.load_thread("thread").unwrap().unwrap().events[0].id,
            "old"
        );
        let receipts: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE operation_id='cancel-retry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipts, 0);
        drop(db);
        let mut db = TokenDb::open(&path, true).unwrap();
        assert_eq!(db.revision().unwrap(), Some(1));
        db.commit_with_operation(&next, 1, "cancel-retry").unwrap();
        assert_eq!(db.revision().unwrap(), Some(2));
        assert_eq!(db.load_thread("thread").unwrap().unwrap().events.len(), 200);
        assert!(
            db.commit_with_operation(&next, 1, "cancel-retry")
                .unwrap()
                .duplicate
        );
        db.integrity_check().unwrap();
    }

    #[test]
    fn cancelled_row_scan_releases_snapshot_and_replays_cleanly() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        db.commit(
            &export(
                (0..100)
                    .map(|i| event(&format!("row-{i}"), "thread", i, false))
                    .collect(),
                1,
            ),
            0,
        )
        .unwrap();
        let token = CancellationToken::new();
        let mut seen = 0;
        let error = token
            .scope(|| {
                db.for_each_event(None, None, |_| {
                    seen += 1;
                    token.cancel();
                    Ok(true)
                })
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(seen, 1);
        assert_eq!(
            token.scope(|| db.load_thread("thread")).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            token
                .scope(|| db.query_events_all(None, None, true))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(db.conn.is_autocommit());
        assert_eq!(db.query_events_all(None, None, true).unwrap().len(), 100);
        assert_eq!(db.load_thread("thread").unwrap().unwrap().events.len(), 100);
    }

    #[test]
    fn cancelled_sqlite_vm_interrupts_single_long_statement() {
        let dir = TempDir::new().unwrap();
        let db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let token = CancellationToken::new();
        let hook_token = token.clone();
        let mut ticks = 0;
        db.conn
            .progress_handler(
                1000,
                Some(move || {
                    ticks += 1;
                    if ticks == 2 {
                        hook_token.cancel();
                    }
                    cancellation::is_cancelled()
                }),
            )
            .unwrap();
        let result = token.scope(|| db.conn.query_row(
            "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<10000000) SELECT SUM(x) FROM n", [], |row| row.get::<_, i64>(0),
        ).map_err(error));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(db.conn.is_autocommit());
        db.integrity_check().unwrap();
    }

    #[test]
    fn cancelled_serialization_uses_non_retrying_io_error() {
        struct CancelDuringSerialize(CancellationToken);
        impl Serialize for CancelDuringSerialize {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element(&"x".repeat(IO_CHUNK * 2))?;
                self.0.cancel();
                seq.serialize_element(&"second".repeat(IO_CHUNK))?;
                seq.end()
            }
        }
        let token = CancellationToken::new();
        let value = CancelDuringSerialize(token.clone());
        assert_eq!(
            token.scope(|| encode(&value)).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        let mut reader = CheckedReader(&b"unchanged"[..]);
        let mut buffer = [0; 8];
        assert_eq!(
            token.scope(|| reader.read(&mut buffer)).unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert_eq!(
            token.scope(|| compress(b"unchanged")).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        let compressed = compress(b"[1,2,3]").unwrap();
        assert_eq!(
            token
                .scope(|| decode::<Vec<u8>>(&compressed))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(decode::<Vec<u8>>(&compressed).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn cancelled_export_never_publishes_partial_destination_and_retries() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        db.commit(
            &export(
                (0..200)
                    .map(|i| event(&format!("row-{i}"), "thread", i, false))
                    .collect(),
                1,
            ),
            0,
        )
        .unwrap();
        let destination = dir.path().join("export.json");
        let token = CancellationToken::new();
        let hook_token = token.clone();
        let mut ticks = 0;
        db.conn
            .progress_handler(
                100,
                Some(move || {
                    ticks += 1;
                    if ticks == 5 {
                        hook_token.cancel();
                    }
                    cancellation::is_cancelled()
                }),
            )
            .unwrap();
        let result = token.scope(|| db.export_json(&destination));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(!destination.exists());
        assert!(db.conn.is_autocommit());
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("tmp-export")));
        db.export_json(&destination).unwrap();
        assert_eq!(TokenStore::load(&destination).unwrap().events().len(), 200);
        let original = fs::read(&destination).unwrap();
        assert_eq!(
            db.export_json(&destination).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&destination).unwrap(), original);
    }

    #[test]
    fn sqlite_preserves_max_u64_optional_fields_and_saturating_summary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tokens.sqlite3");
        let mut db = TokenDb::open(&path, true).unwrap();
        let mut certified = event("a", "thread", u64::MAX, false);
        certified.coverage_start_timestamp_ms = Some(i64::MIN);
        let events = vec![certified, event("b", "thread", 1, false)];
        let delta = export(events.clone(), 1);
        db.commit(&delta, 0).unwrap();
        let saved = db.query_events(None, None, true).unwrap();
        assert_eq!(db.load_thread("thread").unwrap().unwrap().events, events);
        let mut effective = events.clone();
        for event in &mut effective {
            event.cost_usd = tokens::event_cost(event, &Pricing::demo());
        }
        assert_eq!(saved, effective);
        let summary = db.query_summary(None, None, &Pricing::demo()).unwrap();
        assert_eq!(summary.input, u64::MAX);
        assert_eq!(summary.total_tokens, u64::MAX);
        assert_eq!(summary.output, 2);
        assert_eq!(saved[0].reasoning_effort, None);
        assert_eq!(saved[0].turn_id, None);
        assert!(saved[0].cost_usd.is_some());
        db.integrity_check().unwrap();
        drop(db);
        let db = TokenDb::open(&path, false).unwrap();
        assert_eq!(db.query_events(None, None, true).unwrap(), effective);
    }
    #[test]
    fn sqlite_filters_before_pagination_and_keeps_different_threads() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let events = vec![
            event("a", "one", 100, false),
            event("b", "two", 100, false),
            event("c", "review", 100, true),
            event("d", "three", 100, false),
        ];
        db.commit(&export(events, 1), 0).unwrap();
        assert_eq!(db.count_filtered(None, None, false).unwrap(), 3);
        assert_eq!(db.count_filtered(None, None, true).unwrap(), 1);
        let page = db.query_events_filtered(None, None, false, 1, 1).unwrap();
        assert_eq!(page[0].id, "b");
        assert_eq!(
            db.query_events_filtered(None, None, true, 0, 1).unwrap()[0].id,
            "c"
        );
        assert_eq!(
            db.query_summary(None, None, &Pricing::demo())
                .unwrap()
                .input,
            300
        );
    }
    #[test]
    fn sqlite_commit_cas_retry_and_failure_are_atomic() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let delta = export(vec![event("a", "one", 100, false)], 1);
        let first = db.commit_with_operation(&delta, 0, "operation-a").unwrap();
        assert_eq!(first.event_changes.len(), 1);
        assert!(
            db.commit_with_operation(&delta, 0, "operation-a")
                .unwrap()
                .duplicate
        );
        let different = export(vec![event("a", "one", 101, false)], 2);
        assert!(db
            .commit_with_operation(&different, 0, "operation-a")
            .is_err());
        assert!(db
            .commit_with_operation(&different, 0, "operation-b")
            .is_err());
        let invalid = DirtyExport {
            header: different.header.clone(),
            threads: vec![{
                let mut shard = ThreadShard::default();
                shard.thread_id = "wrong".into();
                shard.events = vec![event("bad", "other", 1, false)];
                shard
            }],
        };
        assert!(db.commit_with_operation(&invalid, 1, "invalid").is_err());
        assert_eq!(db.revision().unwrap(), Some(1));
        assert_eq!(db.header().unwrap().revision, 1);
        assert_eq!(db.query_events(None, None, true).unwrap()[0].input, 100);
        assert_eq!(db.thread_ids().unwrap(), vec!["one"]);
        let stats = db
            .commit_with_operation(&different, 1, "operation-b")
            .unwrap();
        assert_eq!(stats.event_changes.len(), 1);
        assert_eq!(stats.event_changes[0].0.as_ref().unwrap().input, 100);
        assert_eq!(stats.event_changes[0].1.as_ref().unwrap().input, 101);
    }
    #[test]
    fn sqlite_one_append_reports_only_actual_event_delta() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let original = event("a", "one", 100, false);
        db.commit(&export(vec![original.clone()], 1), 0).unwrap();
        let stats = db
            .commit(&export(vec![original, event("b", "one", 10, false)], 2), 1)
            .unwrap();
        assert_eq!(stats.events_written, 2);
        assert_eq!(stats.event_changes.len(), 1);
        assert!(stats.event_changes[0].0.is_none());
        assert_eq!(stats.event_changes[0].1.as_ref().unwrap().id, "b");
    }
    #[test]
    fn sqlite_migration_preserves_json_and_roundtrips_every_shard() {
        let dir = TempDir::new().unwrap();
        let sources = dir.path().join("sources");
        fs::create_dir(&sources).unwrap();
        let store = fixture_store(&sources);
        let legacy = dir.path().join("tokens.json");
        store.save(&legacy).unwrap();
        let original = fs::read(&legacy).unwrap();
        assert!(TokenDb::open_or_migrate(&legacy, false, &mut |_| {})
            .unwrap()
            .is_none());
        assert!(!TokenDb::database_path(&legacy).exists());
        let db = TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
            .unwrap()
            .unwrap();
        assert!(db.is_active().unwrap());
        assert_eq!(fs::read(&legacy).unwrap(), original);
        let loaded = TokenStore::load(&legacy).unwrap();
        let mut wanted = loaded.events().to_vec();
        wanted.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        assert_eq!(db.query_events(None, None, true).unwrap(), wanted);
        for shard in loaded.thread_shards() {
            assert_eq!(
                encode(&db.load_thread(&shard.thread_id).unwrap().unwrap()).unwrap(),
                encode(&shard).unwrap()
            );
        }
        assert_eq!(
            serde_json::to_value(db.query_summary(None, None, &Pricing::demo()).unwrap()).unwrap(),
            serde_json::to_value(loaded.summary(None, None, &Pricing::demo())).unwrap()
        );
        db.integrity_check().unwrap();
    }
    #[test]
    fn sqlite_migration_resumes_after_interrupted_checkpoint() {
        let dir = TempDir::new().unwrap();
        let sources = dir.path().join("sources");
        fs::create_dir(&sources).unwrap();
        let store = fixture_store(&sources);
        let legacy = dir.path().join("tokens.json");
        store.save(&legacy).unwrap();
        let original = fs::read(&legacy).unwrap();
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            TokenDb::open_or_migrate(&legacy, true, &mut |progress| {
                if progress.phase == "migrating_threads" && progress.done == 1 {
                    panic!("simulated process interruption after committed shard")
                }
            })
            .unwrap();
        }));
        assert!(interrupted.is_err());
        assert!(!TokenDb::database_path(&legacy).exists());
        let stage = TokenDb::database_path(&legacy).with_extension("migrating.sqlite3");
        assert!(stage.exists());
        let staged = TokenDb::open(&stage, false).unwrap();
        assert!(!staged.is_active().unwrap());
        drop(staged);
        let db = TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
            .unwrap()
            .unwrap();
        assert!(db.is_active().unwrap());
        assert_eq!(db.counts().unwrap().stored_events, store.events().len());
        assert_eq!(fs::read(legacy).unwrap(), original);
    }
    // Invoked in a fresh test process by the two crash tests below. A kill must
    // skip every Rust destructor: catch_unwind alone does not exercise WAL recovery.
    #[test]
    fn sqlite_crash_subprocess_helper() {
        let Some(mode) = std::env::var_os("QUOTA_DB_CRASH_TEST_MODE") else {
            return;
        };
        let path = PathBuf::from(std::env::var_os("QUOTA_DB_CRASH_TEST_PATH").unwrap());
        let marker = PathBuf::from(std::env::var_os("QUOTA_DB_CRASH_TEST_MARKER").unwrap());
        let block_after_marker = || {
            fs::write(&marker, b"checkpoint reached").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        };
        match mode.to_str().unwrap() {
            "migration" => {
                TokenDb::open_or_migrate(&path, true, &mut |progress| {
                    if progress.phase == "migrating_threads" && progress.done == 1 {
                        block_after_marker();
                    }
                })
                .unwrap();
                panic!("migration child did not reach its durable checkpoint");
            }
            "transaction" => {
                let db = TokenDb::open(&path, true).unwrap();
                db.conn
                    .execute_batch("PRAGMA cache_size=4; PRAGMA cache_spill=ON; BEGIN IMMEDIATE; UPDATE events SET input=777,uncached=777; UPDATE threads SET data=X'00';")
                    .unwrap();
                db.conn
                    .execute("UPDATE state SET revision=?", [revision_bytes(99)])
                    .unwrap();
                let mut wal = path.as_os_str().to_os_string();
                wal.push("-wal");
                assert!(fs::metadata(PathBuf::from(wal)).unwrap().len() > 32);
                block_after_marker();
            }
            _ => panic!("unknown crash helper mode"),
        }
    }
    fn kill_crash_helper(mode: &str, path: &Path, marker: &Path) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "token_db::tests::sqlite_crash_subprocess_helper",
                "--nocapture",
            ])
            .env("QUOTA_DB_CRASH_TEST_MODE", mode)
            .env("QUOTA_DB_CRASH_TEST_PATH", path)
            .env("QUOTA_DB_CRASH_TEST_MARKER", marker)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !marker.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("crash helper exited before checkpoint: {status}");
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("crash helper did not reach checkpoint within 20 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
    }
    #[test]
    fn sqlite_migration_resumes_after_subprocess_kill() {
        let dir = TempDir::new().unwrap();
        let sources = dir.path().join("sources");
        fs::create_dir(&sources).unwrap();
        let store = fixture_store(&sources);
        let legacy = dir.path().join("tokens.json");
        store.save(&legacy).unwrap();
        let original = fs::read(&legacy).unwrap();
        kill_crash_helper("migration", &legacy, &dir.path().join("ready"));
        let destination = TokenDb::database_path(&legacy);
        assert!(!destination.exists());
        let staging = destination.with_extension("migrating.sqlite3");
        assert!(staging.exists());
        let staged = TokenDb::open(&staging, false).unwrap();
        assert!(!staged.is_active().unwrap());
        assert_eq!(
            staged
                .conn
                .query_row("SELECT COUNT(*) FROM migrated_threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(staged);
        let db = TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
            .unwrap()
            .unwrap();
        assert!(db.is_active().unwrap());
        db.integrity_check().unwrap();
        assert_eq!(fs::read(&legacy).unwrap(), original);
        let expected = TokenStore::load(&legacy).unwrap();
        assert_eq!(
            encode(&db.header().unwrap()).unwrap(),
            encode(&expected.header()).unwrap()
        );
        for shard in expected.into_thread_shards() {
            assert_eq!(
                encode(&db.load_thread(&shard.thread_id).unwrap().unwrap()).unwrap(),
                encode(&shard).unwrap()
            );
        }
    }
    #[test]
    fn sqlite_uncommitted_wal_rolls_back_after_subprocess_kill() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tokens.sqlite3");
        let events = (0..5000)
            .map(|index| event(&format!("event-{index:05}"), "one", index, false))
            .collect();
        let delta = export(events, 1);
        let mut db = TokenDb::open(&path, true).unwrap();
        db.commit(&delta, 0).unwrap();
        let before = encode(&db.load_thread("one").unwrap().unwrap()).unwrap();
        let header = encode(&db.header().unwrap()).unwrap();
        db.checkpoint().unwrap();
        drop(db);
        kill_crash_helper("transaction", &path, &dir.path().join("ready"));
        let db = TokenDb::open(&path, true).unwrap();
        db.integrity_check().unwrap();
        assert_eq!(db.revision().unwrap(), Some(1));
        assert_eq!(encode(&db.header().unwrap()).unwrap(), header);
        assert_eq!(
            encode(&db.load_thread("one").unwrap().unwrap()).unwrap(),
            before
        );
        assert_eq!(db.counts().unwrap().included_events, 5000);
    }
    #[test]
    fn sqlite_readonly_does_not_create_or_commit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.sqlite3");
        assert!(TokenDb::open(&path, false).is_err());
        assert!(!path.exists());
        let mut writable = TokenDb::open(&path, true).unwrap();
        writable.commit(&export(Vec::new(), 0), 0).unwrap();
        drop(writable);
        let mut db = TokenDb::open(&path, false).unwrap();
        assert!(db.commit(&export(Vec::new(), 1), 0).is_err());
        assert_eq!(db.revision().unwrap(), Some(0));
    }
    #[test]
    fn sqlite_metadata_projections_obey_hk_days_policy_and_future_cutoff() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let mut first = event("a", "one", 10, false);
        first.timestamp = -28801;
        let mut second = event("b", "one", 10, false);
        second.timestamp = -28800;
        let mut future = event("c", "one", 10, false);
        future.timestamp = 57600;
        let mut anonymous = event("legacy", "anonymous", 100, false);
        anonymous.source_kind = Some("token_count".into());
        anonymous.partial_flags = vec!["session_identity_missing".into()];
        let mut delta = export(vec![first, second, future, anonymous], 1);
        delta.header.response_threads.insert("one".into());
        delta.header.anonymous_threads.insert("anonymous".into());
        db.commit(&delta, 0).unwrap();
        assert_eq!(db.days().unwrap(), vec![-115200, -28800, 57600]);
        assert_eq!(db.counts().unwrap().included_events, 3);
        assert_eq!(
            db.count_filtered(Some(-28800), Some(57600), false).unwrap(),
            1
        );
        assert_eq!(db.models().unwrap(), vec!["gpt-6.1-sol"]);
        let latest = db.latest_per_thread_before(0, false).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest["one"].id, "b");
        assert_eq!(db.earliest_included_timestamp().unwrap(), Some(-28801));
        assert!(db
            .query_summary(None, None, &Pricing::demo())
            .unwrap()
            .cost_usd
            .is_none());
        delta.header.response_threads.clear();
        delta.header.anonymous_threads.clear();
        delta.header.revision = 2;
        delta.threads.clear();
        let stats = db.commit(&delta, 1).unwrap();
        assert!(stats.global_policy_changed);
        assert_eq!(db.counts().unwrap().included_events, 4);
        assert_eq!(db.latest_per_thread(false).unwrap().len(), 2);
    }
    #[test]
    fn sqlite_metadata_work_is_bounded_by_projections_not_event_decoding() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let mut events = Vec::new();
        for n in 0..10000 {
            let mut e = event(&format!("{n:05}"), "one", 1, false);
            e.timestamp = n;
            e.partial_flags = if n == 0 {
                vec!["old-only-flag".into()]
            } else {
                Vec::new()
            };
            events.push(e);
        }
        db.commit(&export(events, 1), 0).unwrap();
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM event_partitions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        db.conn.execute("UPDATE strings SET value='invalid JSON solely in old row' WHERE sid=(SELECT flags_sid FROM events WHERE id='00000')",[]).unwrap();
        assert_eq!(db.counts().unwrap().included_events, 10000);
        assert_eq!(db.days().unwrap(), vec![-28800]);
        assert_eq!(db.models().unwrap(), vec!["gpt-6.1-sol"]);
        assert_eq!(db.latest_per_thread(false).unwrap()["one"].id, "09999");
        assert!(db.query_events(None, None, true).is_err());
        let mut statement=db.conn.prepare("EXPLAIN QUERY PLAN SELECT event_key FROM events WHERE thread_key=? AND excluded=? AND anonymous_legacy=? AND timestamp<=? ORDER BY timestamp DESC,id DESC LIMIT 1").unwrap();
        let plan: Vec<String> = statement
            .query_map(params![1, 0, 0, 5000], |r| r.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter()
                .any(|line| line.contains("event_partition_time")),
            "{plan:?}"
        );
    }
    #[test]
    fn sqlite_changed_legacy_is_detected_without_overwriting_either_copy() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join("tokens.json");
        TokenStore::default().save(&legacy).unwrap();
        let db = TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
            .unwrap()
            .unwrap();
        db.checkpoint().unwrap();
        let database = db.path().to_owned();
        drop(db);
        let before = fs::read(&database).unwrap();
        let mut changed = fs::read(&legacy).unwrap();
        changed.extend_from_slice(b" ");
        fs::write(&legacy, &changed).unwrap();
        assert!(TokenDb::open_or_migrate(&legacy, false, &mut |_| {}).is_err());
        assert_eq!(fs::read(&legacy).unwrap(), changed);
        assert_eq!(fs::read(database).unwrap(), before);
    }
    #[test]
    fn sqlite_clean_readonly_inspection_creates_no_sidecars() {
        let dir = TempDir::new().unwrap();
        let lock = crate::storage::DataLock::acquire(dir.path()).unwrap();
        // '?' is not a legal Windows filename; '#' also exercises URI escaping.
        let path = dir.path().join(if cfg!(windows) {
            "tokens with # reserved.sqlite3"
        } else {
            "tokens with ? reserved.sqlite3"
        });
        let mut db = TokenDb::open(&path, true).unwrap();
        db.commit(&export(vec![event("a", "one", 1, false)], 1), 0)
            .unwrap();
        db.checkpoint().unwrap();
        drop(db);
        drop(lock);
        let snapshot = || {
            let mut files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
            for entry in fs::read_dir(dir.path()).unwrap() {
                let path = entry.unwrap().path();
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
            files
        };
        let before = snapshot();
        let db = TokenDb::open_readonly(&path).unwrap();
        assert!(db._shared_read_lock.is_some());
        assert_eq!(db.counts().unwrap().included_events, 1);
        drop(db);
        assert_eq!(snapshot(), before);
    }
    #[test]
    fn sqlite_live_reader_uses_wal_and_pins_one_revision() {
        let dir = TempDir::new().unwrap();
        let _lock = crate::storage::DataLock::acquire(dir.path()).unwrap();
        let path = dir.path().join("tokens.sqlite3");
        let mut writer = TokenDb::open(&path, true).unwrap();
        writer
            .commit(&export(vec![event("a", "one", 1, false)], 1), 0)
            .unwrap();
        let reader = TokenDb::open_readonly(&path).unwrap();
        assert!(reader._shared_read_lock.is_none());
        let snapshot = reader.read_snapshot().unwrap();
        assert_eq!(reader.header().unwrap().revision, 1);
        writer
            .commit(
                &export(
                    vec![event("a", "one", 1, false), event("b", "one", 1, false)],
                    2,
                ),
                1,
            )
            .unwrap();
        assert_eq!(reader.header().unwrap().revision, 1);
        assert_eq!(reader.counts().unwrap().included_events, 1);
        drop(snapshot);
        assert_eq!(reader.header().unwrap().revision, 2);
        assert_eq!(reader.counts().unwrap().included_events, 2);
    }
    #[test]
    fn sqlite_rate_only_commit_refreshes_public_costs_without_rewriting_shards() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let initial = export(vec![event("a", "one", 100, false)], 1);
        db.commit(&initial, 0).unwrap();
        let before = db.query_events_all(None, None, true).unwrap()[0]
            .cost_usd
            .unwrap();
        let mut changed = initial;
        changed.header.revision = 2;
        changed
            .header
            .pricing
            .as_mut()
            .unwrap()
            .rates
            .get_mut("gpt-6.1-sol")
            .unwrap()
            .uncached_input_per_million = 10.;
        changed.threads.clear();
        let stats = db.commit(&changed, 1).unwrap();
        assert_eq!(stats.threads_written, 0);
        assert_eq!(stats.events_written, 0);
        let expected = 0.001002;
        let row = &db.query_events_all(None, None, true).unwrap()[0];
        assert!((row.cost_usd.unwrap() - expected).abs() < 1e-12);
        assert_ne!(row.cost_usd.unwrap(), before);
        assert_eq!(
            db.latest_per_thread(false).unwrap()["one"].cost_usd,
            row.cost_usd
        );
        assert_eq!(
            db.query_events_filtered(None, None, false, 0, 1).unwrap()[0].cost_usd,
            row.cost_usd
        );
        assert_eq!(
            db.export_store().unwrap().events()[0].cost_usd,
            row.cost_usd
        );
        assert_eq!(
            db.load_thread("one").unwrap().unwrap().events[0].cost_usd,
            None
        );
    }
    #[test]
    fn sqlite_bound_only_policy_change_invalidates_effective_history() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let mut response = event("a", "one", 100, false);
        response.coverage_start_timestamp_ms = Some(200);
        let mut initial = export(vec![response], 1);
        initial.header.response_threads.insert("one".into());
        initial.header.anonymous_threads.insert("anonymous".into());
        initial
            .header
            .anonymous_bounds_ms
            .insert("anonymous".into(), 100);
        db.commit(&initial, 0).unwrap();
        assert!(db.query_events_all(None, None, false).unwrap()[0]
            .partial_flags
            .is_empty());
        initial.header.revision = 2;
        initial
            .header
            .anonymous_bounds_ms
            .insert("anonymous".into(), 300);
        initial.threads.clear();
        let stats = db.commit(&initial, 1).unwrap();
        assert!(stats.global_policy_changed);
        assert_eq!(stats.threads_written, 0);
        assert!(stats.event_changes.is_empty());
        assert!(db.query_events_all(None, None, false).unwrap()[0]
            .partial_flags
            .iter()
            .any(|flag| flag == "mixed_schema_legacy_overlap_suppressed"));
    }
    #[test]
    fn sqlite_task_and_turn_queries_match_effective_filter_and_use_thread_index() {
        let dir = TempDir::new().unwrap();
        let mut db = TokenDb::open(&dir.path().join("tokens.sqlite3"), true).unwrap();
        let mut events = Vec::new();
        for thread in ["selected", "other", "anonymous"] {
            for index in 0..8 {
                let mut row = event(&format!("{thread}-{index}"), thread, 100, index % 2 == 1);
                row.timestamp = 100 + index;
                row.turn_id = match index % 3 {
                    0 => None,
                    1 => Some("turn-a".into()),
                    _ => Some("turn-b".into()),
                };
                if thread == "anonymous" {
                    row.source_kind = Some("token_count".into());
                    row.partial_flags.push("session_identity_missing".into());
                }
                events.push(row);
            }
        }
        let mut delta = export(events, 1);
        delta.header.response_threads.insert("selected".into());
        delta.header.anonymous_threads.insert("anonymous".into());
        db.commit(&delta, 0).unwrap();
        // Pricing-only revision must also affect task/turn reads without raw writes.
        delta.header.revision = 2;
        delta
            .header
            .pricing
            .as_mut()
            .unwrap()
            .rates
            .get_mut("gpt-6.1-sol")
            .unwrap()
            .uncached_input_per_million = 10.;
        delta.threads.clear();
        db.commit(&delta, 1).unwrap();
        for thread in ["selected", "other", "anonymous", "missing", "' OR 1=1 --"] {
            for (from, to) in [(None, None), (Some(102), Some(107)), (Some(104), Some(104))] {
                for excluded in [false, true] {
                    let expected: Vec<_> = db
                        .query_events_all(from, to, true)
                        .unwrap()
                        .into_iter()
                        .filter(|row| {
                            row.session == thread && row.is_excluded_auto_review() == excluded
                        })
                        .collect();
                    assert_eq!(
                        db.query_task_events(thread, from, to, excluded).unwrap(),
                        expected
                    );
                    for turn in [
                        None,
                        Some("turn-a"),
                        Some("turn-b"),
                        Some("missing"),
                        Some("' OR 1=1 --"),
                    ] {
                        let wanted: Vec<_> = expected
                            .iter()
                            .filter(|row| row.turn_id.as_deref() == turn)
                            .cloned()
                            .collect();
                        assert_eq!(
                            db.query_turn_events(thread, turn, from, to, excluded)
                                .unwrap(),
                            wanted
                        );
                    }
                }
            }
        }
        let selected = db.query_task_events("selected", None, None, false).unwrap();
        assert!((selected[0].cost_usd.unwrap() - 0.001002).abs() < 1e-12);
        assert!(selected[0]
            .partial_flags
            .iter()
            .any(|flag| flag == "mixed_schema_legacy_overlap_suppressed"));
        let plan: Vec<String> = db.conn.prepare("EXPLAIN QUERY PLAN SELECT e.id FROM events e INDEXED BY event_thread_time WHERE e.thread_key=(SELECT thread_key FROM threads WHERE thread_id=?) AND e.excluded=? AND e.turn_id IS ? AND e.timestamp>=? AND e.timestamp<? ORDER BY e.timestamp,e.id").unwrap().query_map(params!["selected",false,"turn-a",102,107],|row|row.get(3)).unwrap().map(Result::unwrap).collect();
        assert!(
            plan.iter().any(|line| line.contains("event_thread_time")
                && line.contains("thread_key=?")
                && line.contains("timestamp>?")
                && line.contains("timestamp<?")),
            "{plan:?}"
        );
        // An unrelated broken event must not be decoded by the bounded APIs.
        db.conn
            .execute(
                "INSERT INTO strings(value) VALUES('deliberately invalid flags JSON')",
                [],
            )
            .unwrap();
        db.conn.execute("UPDATE events SET flags_sid=(SELECT sid FROM strings WHERE value='deliberately invalid flags JSON') WHERE thread_key=(SELECT thread_key FROM threads WHERE thread_id='other')",[]).unwrap();
        assert_eq!(
            db.query_task_events("selected", None, None, false).unwrap(),
            selected
        );
        assert!(db
            .query_turn_events("selected", Some("turn-a"), None, None, false)
            .is_ok());
        assert!(db.query_events_all(None, None, true).is_err());
    }
}

pub(crate) fn decode_index_chunk(
    data: &[u8],
) -> io::Result<Vec<crate::tokens::ImportedQuotaObservation>> {
    decode(data)
}
