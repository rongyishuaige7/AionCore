//! Full-text recall index over a resident role's thread, scheduled runs and
//! summary leaves (`<data_dir>/roles/recall.db`).
//!
//! Markdown files on disk and the conversation database remain the source of
//! truth; `recall.db` is a rebuildable SQLite FTS5 (`trigram`) index kept in
//! step through per-conversation watermarks. Deleting the file at any time
//! causes the next `sync` / `recall` to recreate and repopulate it cleanly.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aionui_api_types::{
    RoleExpandMessage, RoleExpandRequest, RoleExpandResponse, RoleRecallBound, RoleRecallHit, RoleRecallKind,
    RoleRecallRequest, RoleRecallResponse, RoleRecallScope, RoleRecallSource,
};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params, params_from_iter};

use crate::error::RoleError;
use crate::store::LeafFile;

const RECALL_DB_FILE: &str = "recall.db";

/// Maximum length of a `recall` query, in characters.
pub const MAX_RECALL_QUERY_CHARS: usize = 400;
/// Default number of hits returned by `recall`.
pub const DEFAULT_RECALL_LIMIT: u32 = 10;
/// Maximum number of hits returned by `recall`.
pub const MAX_RECALL_LIMIT: u32 = 50;
/// Default number of messages shown before and after a hit in `expand`.
pub const DEFAULT_EXPAND_WINDOW: u32 = 3;
/// Maximum `before` or `after` window accepted by `expand`.
pub const MAX_EXPAND_WINDOW: u32 = 20;
/// Per-message character budget in `expand`.
const EXPAND_MESSAGE_CHARS: usize = 4_000;
/// Total character budget across all messages returned by one `expand` call.
const EXPAND_TOTAL_CHARS: usize = 24_000;
/// Maximum snippet length returned in a `recall` hit, in characters.
const MAX_SNIPPET_CHARS: usize = 240;

const SCHEMA_SQL: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS docs (
    rowid           INTEGER PRIMARY KEY,
    role_id         TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    source          TEXT NOT NULL,
    kind            TEXT NOT NULL,
    ref_id          TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    speaker         TEXT NOT NULL,
    span            TEXT NOT NULL DEFAULT '',
    text            TEXT NOT NULL,
    UNIQUE(role_id, ref_id)
);

CREATE INDEX IF NOT EXISTS idx_docs_role_conv_order
    ON docs(role_id, conversation_id, kind, created_at, ref_id);

CREATE INDEX IF NOT EXISTS idx_docs_role_time
    ON docs(role_id, created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS docs_fts USING fts5(
    text,
    content='docs',
    content_rowid='rowid',
    tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS docs_ai AFTER INSERT ON docs BEGIN
    INSERT INTO docs_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TRIGGER IF NOT EXISTS docs_ad AFTER DELETE ON docs BEGIN
    INSERT INTO docs_fts(docs_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;

CREATE TRIGGER IF NOT EXISTS docs_au AFTER UPDATE ON docs BEGIN
    INSERT INTO docs_fts(docs_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    INSERT INTO docs_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TABLE IF NOT EXISTS watermarks (
    role_id         TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    id              TEXT NOT NULL,
    leaves_count    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (role_id, conversation_id)
);
";

/// How far a conversation of a role has been indexed in `recall.db`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallWatermark {
    pub created_at: i64,
    pub id: String,
    pub leaves_count: usize,
}

/// One carried message to index for `recall`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallMessage {
    pub id: String,
    pub created_at: i64,
    pub speaker: String,
    pub text: String,
}

/// New messages and updated cursor for one conversation of a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallConversationSync {
    pub conversation_id: String,
    pub source: RoleRecallSource,
    pub messages: Vec<RecallMessage>,
    /// New message watermark `(created_at, id)` once scanned, if it advanced.
    pub watermark: Option<(i64, String)>,
}

/// Incremental catch-up payload for a role's recall index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallSync {
    /// Current `thread.json` conversation id, if the role has a thread.
    pub active_thread: Option<String>,
    /// All live scheduled-run conversation ids of this role.
    pub active_runs: Vec<String>,
    /// Conversations that yielded new messages or watermark updates.
    pub batches: Vec<RecallConversationSync>,
}

#[derive(Debug, Clone)]
struct ParsedLeaf {
    ref_id: String,
    created_at: i64,
    span: String,
    text: String,
}

#[derive(Clone)]
pub(crate) struct RecallIndex {
    path: PathBuf,
    conn: Arc<Mutex<Option<Connection>>>,
}

impl fmt::Debug for RecallIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecallIndex").field("path", &self.path).finish()
    }
}

impl RecallIndex {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            path: root.join(RECALL_DB_FILE),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    async fn run<T, F>(&self, work: F) -> Result<T, RoleError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, RoleError> + Send + 'static,
    {
        let path = self.path.clone();
        let slot = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            if !path.exists() {
                *guard = None;
            }
            if guard.is_none() {
                *guard = Some(open_or_rebuild(&path)?);
            }
            let conn = guard.as_mut().expect("connection initialized");
            let outcome = work(conn);
            if outcome.is_err() && !path.exists() {
                *guard = None;
            }
            outcome
        })
        .await
        .map_err(|err| RoleError::Io(std::io::Error::other(err)))?
    }

    /// Watermarks recorded for `role_id`. Returns an empty map without creating
    /// `recall.db` if the index file does not exist yet.
    pub(crate) async fn watermarks(&self, role_id: &str) -> Result<HashMap<String, RecallWatermark>, RoleError> {
        if !self.path.exists() {
            *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Ok(HashMap::new());
        }
        let role_id = role_id.to_owned();
        self.run(move |conn| read_watermarks_sync(conn, &role_id)).await
    }

    pub(crate) async fn sync(
        &self,
        role_id: &str,
        sync: RecallSync,
        thread_leaves: Vec<LeafFile>,
    ) -> Result<(), RoleError> {
        let role_id = role_id.to_owned();
        self.run(move |conn| sync_role_sync(conn, &role_id, sync, &thread_leaves))
            .await
    }

    pub(crate) async fn recall(
        &self,
        role_id: &str,
        request: &RoleRecallRequest,
        now: i64,
    ) -> Result<RoleRecallResponse, RoleError> {
        let validated = validate_recall_request(request)?;
        if !self.path.exists() {
            return Ok(RoleRecallResponse { hits: Vec::new() });
        }
        let role_id = role_id.to_owned();
        self.run(move |conn| search_sync(conn, &role_id, &validated, now)).await
    }

    pub(crate) async fn expand(
        &self,
        role_id: &str,
        request: &RoleExpandRequest,
    ) -> Result<RoleExpandResponse, RoleError> {
        let ref_id = request.ref_id.trim().to_owned();
        if ref_id.is_empty() {
            return Err(RoleError::InvalidRequest("ref_id cannot be empty".to_owned()));
        }
        let before = request.before.unwrap_or(DEFAULT_EXPAND_WINDOW);
        let after = request.after.unwrap_or(DEFAULT_EXPAND_WINDOW);
        if before > MAX_EXPAND_WINDOW {
            return Err(RoleError::InvalidRequest(format!(
                "`before` exceeds {MAX_EXPAND_WINDOW} messages"
            )));
        }
        if after > MAX_EXPAND_WINDOW {
            return Err(RoleError::InvalidRequest(format!(
                "`after` exceeds {MAX_EXPAND_WINDOW} messages"
            )));
        }
        if !self.path.exists() {
            return Err(RoleError::InvalidRequest(format!("unknown ref_id: {ref_id}")));
        }
        let role_id = role_id.to_owned();
        self.run(move |conn| expand_sync(conn, &role_id, &ref_id, before, after))
            .await
    }
}

fn open_or_rebuild(path: &Path) -> Result<Connection, RoleError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match open_and_init(path) {
        Ok(conn) => Ok(conn),
        Err(err) if is_corrupt(&err) => {
            tracing::warn!(path = %path.display(), error = %err, "role recall: rebuilding unreadable index");
            remove_db_files(path);
            open_and_init(path).map_err(sql_err)
        }
        Err(err) => Err(sql_err(err)),
    }
}

fn open_and_init(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

fn is_corrupt(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn remove_db_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(base) = path.to_str() {
        let _ = std::fs::remove_file(format!("{base}-wal"));
        let _ = std::fs::remove_file(format!("{base}-shm"));
    }
}

fn sql_err(err: rusqlite::Error) -> RoleError {
    RoleError::Io(std::io::Error::other(err))
}

fn read_watermarks_sync(conn: &mut Connection, role_id: &str) -> Result<HashMap<String, RecallWatermark>, RoleError> {
    let mut stmt = conn
        .prepare("SELECT conversation_id, created_at, id, leaves_count FROM watermarks WHERE role_id = ?1")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![role_id], |row| {
            let conv_id: String = row.get(0)?;
            let created_at: i64 = row.get(1)?;
            let id: String = row.get(2)?;
            let leaves_count: i64 = row.get(3)?;
            Ok((
                conv_id,
                RecallWatermark {
                    created_at,
                    id,
                    leaves_count: usize::try_from(leaves_count.max(0)).unwrap_or(0),
                },
            ))
        })
        .map_err(sql_err)?;
    let mut out = HashMap::new();
    for row in rows {
        let (conv_id, wm) = row.map_err(sql_err)?;
        out.insert(conv_id, wm);
    }
    Ok(out)
}

fn sync_role_sync(
    conn: &mut Connection,
    role_id: &str,
    sync: RecallSync,
    thread_leaves: &[LeafFile],
) -> Result<(), RoleError> {
    let existing_watermarks = read_watermarks_sync(conn, role_id)?;
    let run_set: HashSet<&str> = sync.active_runs.iter().map(String::as_str).collect();
    let thread_id = sync.active_thread.as_deref();

    let tx = conn.transaction().map_err(sql_err)?;

    // Prune thread documents/watermarks if the role's thread moved to a new conversation or disappeared.
    match thread_id {
        Some(tid) => {
            tx.execute(
                "DELETE FROM docs WHERE role_id = ?1 AND source = 'thread' AND conversation_id != ?2",
                params![role_id, tid],
            )
            .map_err(sql_err)?;
        }
        None => {
            tx.execute(
                "DELETE FROM docs WHERE role_id = ?1 AND source = 'thread'",
                params![role_id],
            )
            .map_err(sql_err)?;
        }
    }

    // Prune scheduled-run documents and stale watermarks for deleted conversations.
    let mut indexed_convs = HashSet::new();
    {
        let mut stmt = tx
            .prepare("SELECT DISTINCT conversation_id, source FROM docs WHERE role_id = ?1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![role_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        for row in rows {
            indexed_convs.insert(row.map_err(sql_err)?);
        }
    }
    for (conv_id, source) in indexed_convs {
        if source == "run" && !run_set.contains(conv_id.as_str()) {
            tx.execute(
                "DELETE FROM docs WHERE role_id = ?1 AND source = 'run' AND conversation_id = ?2",
                params![role_id, conv_id],
            )
            .map_err(sql_err)?;
        }
    }
    for conv_id in existing_watermarks.keys() {
        let keep = thread_id == Some(conv_id.as_str()) || run_set.contains(conv_id.as_str());
        if !keep {
            tx.execute(
                "DELETE FROM watermarks WHERE role_id = ?1 AND conversation_id = ?2",
                params![role_id, conv_id],
            )
            .map_err(sql_err)?;
        }
    }

    // Ingest message batches and advance message watermarks.
    for batch in &sync.batches {
        let source_str = source_as_str(batch.source);
        for message in &batch.messages {
            let text = message.text.trim();
            if text.is_empty() {
                continue;
            }
            let ref_id = message_ref_id(&batch.conversation_id, &message.id);
            tx.execute(
                "INSERT INTO docs (role_id, conversation_id, source, kind, ref_id, created_at, speaker, span, text)
                 VALUES (?1, ?2, ?3, 'message', ?4, ?5, ?6, '', ?7)
                 ON CONFLICT(role_id, ref_id) DO UPDATE SET
                     conversation_id = excluded.conversation_id,
                     source = excluded.source,
                     created_at = excluded.created_at,
                     speaker = excluded.speaker,
                     text = excluded.text",
                params![
                    role_id,
                    batch.conversation_id,
                    source_str,
                    ref_id,
                    message.created_at,
                    message.speaker,
                    text,
                ],
            )
            .map_err(sql_err)?;
        }
        if let Some((wm_at, ref wm_id)) = batch.watermark {
            tx.execute(
                "INSERT INTO watermarks (role_id, conversation_id, created_at, id, leaves_count)
                 VALUES (?1, ?2, ?3, ?4, 0)
                 ON CONFLICT(role_id, conversation_id) DO UPDATE SET
                     created_at = excluded.created_at,
                     id = excluded.id",
                params![role_id, batch.conversation_id, wm_at, wm_id],
            )
            .map_err(sql_err)?;
        }
    }

    // Ingest new summary leaves for the active thread.
    if let Some(tid) = thread_id {
        let recorded_leaves = existing_watermarks.get(tid).map_or(0, |wm| wm.leaves_count);
        let start_from = if thread_leaves.len() < recorded_leaves {
            tx.execute(
                "DELETE FROM docs WHERE role_id = ?1 AND conversation_id = ?2 AND kind = 'leaf'",
                params![role_id, tid],
            )
            .map_err(sql_err)?;
            0
        } else {
            recorded_leaves
        };

        for leaf_file in &thread_leaves[start_from..] {
            if let Some(parsed) = parse_leaf(tid, leaf_file) {
                tx.execute(
                    "INSERT INTO docs (role_id, conversation_id, source, kind, ref_id, created_at, speaker, span, text)
                     VALUES (?1, ?2, 'thread', 'leaf', ?3, ?4, 'leaf', ?5, ?6)
                     ON CONFLICT(role_id, ref_id) DO UPDATE SET
                         created_at = excluded.created_at,
                         span = excluded.span,
                         text = excluded.text",
                    params![role_id, tid, parsed.ref_id, parsed.created_at, parsed.span, parsed.text],
                )
                .map_err(sql_err)?;
            }
        }

        if thread_leaves.len() != recorded_leaves || start_from == 0 && !thread_leaves.is_empty() {
            let count = i64::try_from(thread_leaves.len()).unwrap_or(i64::MAX);
            tx.execute(
                "INSERT INTO watermarks (role_id, conversation_id, created_at, id, leaves_count)
                 VALUES (?1, ?2, 0, '', ?3)
                 ON CONFLICT(role_id, conversation_id) DO UPDATE SET
                     leaves_count = excluded.leaves_count",
                params![role_id, tid, count],
            )
            .map_err(sql_err)?;
        }
    }

    tx.commit().map_err(sql_err)
}

pub fn message_ref_id(conversation_id: &str, message_id: &str) -> String {
    format!("msg:{conversation_id}:{message_id}")
}

pub fn leaf_ref_id(conversation_id: &str, stem: &str) -> String {
    format!("leaf:{conversation_id}:{stem}")
}

fn parse_leaf(conversation_id: &str, file: &LeafFile) -> Option<ParsedLeaf> {
    let raw = file.content.trim();
    if raw.is_empty() {
        return None;
    }
    let (span, body) = match raw.split_once('\n') {
        Some((first, rest)) if first.starts_with("<!--") && first.ends_with("-->") => {
            let span = first.trim_start_matches("<!--").trim_end_matches("-->").trim();
            (span.to_owned(), rest.trim().to_owned())
        }
        _ => (String::new(), raw.to_owned()),
    };
    if body.is_empty() {
        return None;
    }
    let created_at = parse_span_end_ms(&span).unwrap_or(file.updated_at);
    Some(ParsedLeaf {
        ref_id: leaf_ref_id(conversation_id, &file.stem),
        created_at,
        span,
        text: body,
    })
}

/// Extract the end timestamp from `YYYY-MM-DD HH:MM → YYYY-MM-DD HH:MM · N messages`.
fn parse_span_end_ms(span: &str) -> Option<i64> {
    let after_arrow = span.split_once('→')?.1;
    let end_part = after_arrow.split('·').next()?.trim();
    parse_local_datetime_ms(end_part)
}

struct ValidatedRecall {
    terms: Vec<String>,
    long_terms: Vec<String>,
    short_terms: Vec<String>,
    limit: u32,
    scope: RoleRecallScope,
    before: Option<i64>,
    after: Option<i64>,
}

fn validate_recall_request(request: &RoleRecallRequest) -> Result<ValidatedRecall, RoleError> {
    let trimmed = request.query.trim();
    if trimmed.is_empty() {
        return Err(RoleError::InvalidRequest("query cannot be empty".to_owned()));
    }
    if trimmed.chars().count() > MAX_RECALL_QUERY_CHARS {
        return Err(RoleError::InvalidRequest(format!(
            "query exceeds {MAX_RECALL_QUERY_CHARS} characters"
        )));
    }
    let limit = request.limit.unwrap_or(DEFAULT_RECALL_LIMIT);
    if limit == 0 || limit > MAX_RECALL_LIMIT {
        return Err(RoleError::InvalidRequest(format!(
            "limit must be between 1 and {MAX_RECALL_LIMIT}"
        )));
    }
    let before = request
        .before
        .as_ref()
        .map(|bound| parse_bound(bound, "before"))
        .transpose()?;
    let after = request
        .after
        .as_ref()
        .map(|bound| parse_bound(bound, "after"))
        .transpose()?;
    if let (Some(after_ms), Some(before_ms)) = (after, before)
        && after_ms >= before_ms
    {
        return Err(RoleError::InvalidRequest(
            "`after` must be earlier than `before`".to_owned(),
        ));
    }

    let terms: Vec<String> = trimmed.split_whitespace().map(str::to_owned).collect();
    let mut long_terms = Vec::new();
    let mut short_terms = Vec::new();
    for term in &terms {
        if term.chars().count() >= 3 {
            long_terms.push(term.clone());
        } else {
            short_terms.push(term.clone());
        }
    }

    Ok(ValidatedRecall {
        terms,
        long_terms,
        short_terms,
        limit,
        scope: request.scope.unwrap_or_default(),
        before,
        after,
    })
}

fn parse_bound(bound: &RoleRecallBound, field: &str) -> Result<i64, RoleError> {
    match bound {
        RoleRecallBound::Millis(ms) => Ok(*ms),
        RoleRecallBound::Text(raw) => {
            let text = raw.trim();
            if text.is_empty() {
                return Err(RoleError::InvalidRequest(format!("`{field}` cannot be empty")));
            }
            if let Ok(ms) = text.parse::<i64>() {
                return Ok(ms);
            }
            if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
                return Ok(dt.timestamp_millis());
            }
            if let Some(ms) = parse_local_datetime_ms(text) {
                return Ok(ms);
            }
            if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d")
                && let Some(naive) = date.and_hms_opt(0, 0, 0)
                && let Some(local) = Local.from_local_datetime(&naive).earliest()
            {
                return Ok(local.timestamp_millis());
            }
            Err(RoleError::InvalidRequest(format!(
                "invalid `{field}` timestamp: {raw} (expected YYYY-MM-DD, YYYY-MM-DD HH:MM, RFC 3339, or epoch ms)"
            )))
        }
    }
}

fn parse_local_datetime_ms(text: &str) -> Option<i64> {
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, fmt)
            && let Some(local) = Local.from_local_datetime(&naive).earliest()
        {
            return Some(local.timestamp_millis());
        }
    }
    None
}

fn search_sync(
    conn: &mut Connection,
    role_id: &str,
    query: &ValidatedRecall,
    now: i64,
) -> Result<RoleRecallResponse, RoleError> {
    let mut sql = String::new();
    let mut values: Vec<SqlValue> = Vec::new();

    if !query.long_terms.is_empty() {
        let fts_match = query
            .long_terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        sql.push_str(
            "SELECT d.ref_id, d.kind, d.source, d.conversation_id, d.speaker, d.created_at, d.span, d.text,
                    snippet(docs_fts, 0, '', '', '…', 64)
             FROM docs_fts
             INNER JOIN docs d ON d.rowid = docs_fts.rowid
             WHERE docs_fts MATCH ? AND d.role_id = ?",
        );
        values.push(SqlValue::Text(fts_match));
        values.push(SqlValue::Text(role_id.to_owned()));
        append_common_filters(
            &mut sql,
            &mut values,
            query.scope,
            query.after,
            query.before,
            &query.short_terms,
        );
        sql.push_str(
            " ORDER BY (-bm25(docs_fts)) + 0.5 / (1.0 + MAX(0.0, (? - d.created_at) / 86400000.0) / 7.0) DESC,
                       d.created_at DESC, d.rowid DESC
              LIMIT ?",
        );
        values.push(SqlValue::Real(now as f64));
        values.push(SqlValue::Integer(i64::from(query.limit)));
    } else {
        sql.push_str(
            "SELECT d.ref_id, d.kind, d.source, d.conversation_id, d.speaker, d.created_at, d.span, d.text, ''
             FROM docs d
             WHERE d.role_id = ?",
        );
        values.push(SqlValue::Text(role_id.to_owned()));
        append_common_filters(
            &mut sql,
            &mut values,
            query.scope,
            query.after,
            query.before,
            &query.short_terms,
        );
        sql.push_str(" ORDER BY d.created_at DESC, d.rowid DESC LIMIT ?");
        values.push(SqlValue::Integer(i64::from(query.limit)));
    }

    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(params_from_iter(values), |row| {
            let ref_id: String = row.get(0)?;
            let kind_raw: String = row.get(1)?;
            let source_raw: String = row.get(2)?;
            let conversation_id: String = row.get(3)?;
            let speaker: String = row.get(4)?;
            let at: i64 = row.get(5)?;
            let span: String = row.get(6)?;
            let full_text: String = row.get(7)?;
            let fts_snippet: String = row.get(8)?;
            Ok(RoleRecallHit {
                ref_id,
                kind: parse_kind(&kind_raw),
                source: parse_source(&source_raw),
                conversation_id,
                speaker,
                at,
                span: (!span.is_empty()).then_some(span),
                snippet: format_snippet(&fts_snippet, &full_text, &query.terms),
            })
        })
        .map_err(sql_err)?;

    let mut hits = Vec::new();
    for hit in rows {
        hits.push(hit.map_err(sql_err)?);
    }
    Ok(RoleRecallResponse { hits })
}

fn append_common_filters(
    sql: &mut String,
    values: &mut Vec<SqlValue>,
    scope: RoleRecallScope,
    after: Option<i64>,
    before: Option<i64>,
    like_terms: &[String],
) {
    match scope {
        RoleRecallScope::Thread => sql.push_str(" AND d.source = 'thread'"),
        RoleRecallScope::Runs => sql.push_str(" AND d.source = 'run'"),
        RoleRecallScope::All => {}
    }
    if let Some(after_ms) = after {
        sql.push_str(" AND d.created_at >= ?");
        values.push(SqlValue::Integer(after_ms));
    }
    if let Some(before_ms) = before {
        sql.push_str(" AND d.created_at < ?");
        values.push(SqlValue::Integer(before_ms));
    }
    for term in like_terms {
        sql.push_str(" AND d.text LIKE ? ESCAPE '\\'");
        values.push(SqlValue::Text(format!("%{}%", escape_like(term))));
    }
}

fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for ch in term.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn format_snippet(fts_snippet: &str, full_text: &str, terms: &[String]) -> String {
    let candidate = collapse_whitespace(fts_snippet);
    if !candidate.is_empty() && candidate.chars().count() <= MAX_SNIPPET_CHARS {
        return candidate;
    }
    make_excerpt(&collapse_whitespace(full_text), terms, MAX_SNIPPET_CHARS)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn make_excerpt(text: &str, terms: &[String], max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_owned();
    }
    let lower = text.to_lowercase();
    let match_char_idx = terms
        .iter()
        .filter_map(|term| {
            let needle = term.to_lowercase();
            lower.find(&needle).map(|byte_idx| lower[..byte_idx].chars().count())
        })
        .min()
        .unwrap_or(0);

    let lead = max_chars / 3;
    let mut start = match_char_idx.saturating_sub(lead);
    if start + max_chars > chars.len() {
        start = chars.len() - max_chars;
    }
    let end = (start + max_chars).min(chars.len());
    let slice: String = chars[start..end].iter().collect();
    match (start > 0, end < chars.len()) {
        (true, true) => format!("…{}…", slice.trim()),
        (true, false) => format!("…{}", slice.trim_start()),
        (false, true) => format!("{}…", slice.trim_end()),
        (false, false) => slice,
    }
}

fn expand_sync(
    conn: &mut Connection,
    role_id: &str,
    ref_id: &str,
    before: u32,
    after: u32,
) -> Result<RoleExpandResponse, RoleError> {
    let target = conn
        .query_row(
            "SELECT conversation_id, source, kind, created_at, speaker, span, text
             FROM docs
             WHERE role_id = ?1 AND ref_id = ?2",
            params![role_id, ref_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| RoleError::InvalidRequest(format!("unknown ref_id: {ref_id}")))?;

    let (conversation_id, source_raw, kind_raw, hit_at, hit_speaker, span, hit_text) = target;
    let source = parse_source(&source_raw);
    let kind = parse_kind(&kind_raw);

    if kind == RoleRecallKind::Leaf {
        return Ok(RoleExpandResponse {
            ref_id: ref_id.to_owned(),
            kind: RoleRecallKind::Leaf,
            source,
            conversation_id,
            span: (!span.is_empty()).then_some(span),
            leaf: Some(hit_text),
            messages: Vec::new(),
            truncated: false,
        });
    }

    let before_rows = if before > 0 {
        query_neighbors(conn, role_id, &conversation_id, hit_at, ref_id, before, true)?
    } else {
        Vec::new()
    };
    let after_rows = if after > 0 {
        query_neighbors(conn, role_id, &conversation_id, hit_at, ref_id, after, false)?
    } else {
        Vec::new()
    };

    let mut truncated = false;
    let (hit_clipped, hit_was_cut) = clip_message(&hit_text, EXPAND_MESSAGE_CHARS);
    truncated |= hit_was_cut;
    let mut used_chars = hit_clipped.chars().count();

    let mut kept_before = Vec::new();
    let mut kept_after = Vec::new();
    let max_steps = before_rows.len().max(after_rows.len());
    let mut before_stopped = false;
    let mut after_stopped = false;

    for idx in 0..max_steps {
        if !before_stopped && let Some((r_id, spk, at, txt)) = before_rows.get(idx) {
            let (clipped, cut) = clip_message(txt, EXPAND_MESSAGE_CHARS);
            let len = clipped.chars().count();
            if used_chars + len <= EXPAND_TOTAL_CHARS {
                used_chars += len;
                truncated |= cut;
                kept_before.push(RoleExpandMessage {
                    ref_id: r_id.clone(),
                    speaker: spk.clone(),
                    at: *at,
                    text: clipped,
                    hit: false,
                });
            } else {
                truncated = true;
                before_stopped = true;
            }
        }
        if !after_stopped && let Some((r_id, spk, at, txt)) = after_rows.get(idx) {
            let (clipped, cut) = clip_message(txt, EXPAND_MESSAGE_CHARS);
            let len = clipped.chars().count();
            if used_chars + len <= EXPAND_TOTAL_CHARS {
                used_chars += len;
                truncated |= cut;
                kept_after.push(RoleExpandMessage {
                    ref_id: r_id.clone(),
                    speaker: spk.clone(),
                    at: *at,
                    text: clipped,
                    hit: false,
                });
            } else {
                truncated = true;
                after_stopped = true;
            }
        }
    }

    kept_before.reverse();
    let mut messages = Vec::with_capacity(kept_before.len() + 1 + kept_after.len());
    messages.extend(kept_before);
    messages.push(RoleExpandMessage {
        ref_id: ref_id.to_owned(),
        speaker: hit_speaker,
        at: hit_at,
        text: hit_clipped,
        hit: true,
    });
    messages.extend(kept_after);

    Ok(RoleExpandResponse {
        ref_id: ref_id.to_owned(),
        kind: RoleRecallKind::Message,
        source,
        conversation_id,
        span: None,
        leaf: None,
        messages,
        truncated,
    })
}

fn query_neighbors(
    conn: &Connection,
    role_id: &str,
    conversation_id: &str,
    hit_at: i64,
    hit_ref: &str,
    limit: u32,
    preceding: bool,
) -> Result<Vec<(String, String, i64, String)>, RoleError> {
    let sql = if preceding {
        "SELECT ref_id, speaker, created_at, text
         FROM docs
         WHERE role_id = ?1
           AND conversation_id = ?2
           AND kind = 'message'
           AND (created_at < ?3 OR (created_at = ?3 AND ref_id < ?4))
         ORDER BY created_at DESC, ref_id DESC
         LIMIT ?5"
    } else {
        "SELECT ref_id, speaker, created_at, text
         FROM docs
         WHERE role_id = ?1
           AND conversation_id = ?2
           AND kind = 'message'
           AND (created_at > ?3 OR (created_at = ?3 AND ref_id > ?4))
         ORDER BY created_at ASC, ref_id ASC
         LIMIT ?5"
    };
    let mut stmt = conn.prepare(sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(
            params![role_id, conversation_id, hit_at, hit_ref, i64::from(limit)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(sql_err)?);
    }
    Ok(out)
}

fn clip_message(text: &str, cap: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= cap {
        return (text.to_owned(), false);
    }
    let keep = cap / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(count - keep).collect();
    (
        format!("{head}\n[… {} characters left out …]\n{tail}", count - 2 * keep),
        true,
    )
}

fn source_as_str(source: RoleRecallSource) -> &'static str {
    match source {
        RoleRecallSource::Thread => "thread",
        RoleRecallSource::Run => "run",
    }
}

fn parse_source(raw: &str) -> RoleRecallSource {
    match raw {
        "run" => RoleRecallSource::Run,
        _ => RoleRecallSource::Thread,
    }
}

fn parse_kind(raw: &str) -> RoleRecallKind {
    match raw {
        "leaf" => RoleRecallKind::Leaf,
        _ => RoleRecallKind::Message,
    }
}
