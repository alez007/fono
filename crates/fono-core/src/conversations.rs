// SPDX-License-Identifier: GPL-3.0-only
//! SQLite-backed store of assistant conversations (ADR 0040).
//!
//! Dictation transcripts live in [`crate::history`]; this is the parallel
//! store for the *assistant* — the spoken back-and-forth, the tools it
//! invoked, and who Fono believed was speaking on each turn.
//!
//! Shape:
//!
//! - A **thread** is one conversation. Threads are segmented by an idle
//!   timeout and by the explicit "Forget conversation" action.
//! - A **turn** is one utterance, reply, tool call or tool result within a
//!   thread, ordered by `ordinal`.
//!
//! Privacy posture matches [`crate::history`] in every respect but one: the
//! DB file is clamped to owner-only `0600` on Unix and retention is bounded
//! by [`ConversationStore::purge_older_than`], but turns are stored
//! **verbatim**.
//!
//! Verbatim, because the key-shaped-blob heuristic that guards dictation
//! ([`crate::history::redact`], `[A-Za-z0-9_-]{20,}`) is wrong here in both
//! directions. It cannot fire on speech — a spoken sentence has spaces, so
//! no run of twenty word characters survives — and it fires constantly on
//! the machine text a conversation carries: entity ids
//! (`light.master_bedroom_ceiling_light`), tool arguments, and the error
//! prose a server sends back. Real turns were stored as
//! `HassTurnOn failed: … constraints=[REDACTED](name=None, …)`, which
//! destroys the audit trail the store exists for.
//!
//! It is also not merely cosmetic: a resumed thread is replayed into the
//! prompt (`ConversationSink::resume`), so a masked turn is read back to the
//! model as though `[REDACTED]` were something the user said.
//!
//! Dictation is the opposite case and keeps its redaction: it is arbitrary
//! text on its way into an editor or a terminal, so a pasted key really can
//! land there.
//!
//! The verified speaker name is recorded **per turn**, not per thread — a
//! conversation can involve more than one person. It is stored as a
//! historical fact rather than a foreign key, so renaming or deleting an
//! enrolled speaker never rewrites past conversations. Only the name is
//! ever stored; voice-print embeddings stay in [`crate::speakers`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, Result};

/// Schema version written to `meta`. Bumped when the layout changes in a
/// way that cannot be handled by an additive `ALTER TABLE`.
const SCHEMA_VERSION: i64 = 1;

/// Who (or what) produced a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// A spoken user utterance.
    User,
    /// The assistant's reply.
    Assistant,
    /// A tool the assistant chose to invoke.
    ToolCall,
    /// The result handed back to the assistant.
    ToolResult,
}

impl TurnRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }

    /// Parse a role back from its stored string. Infallible: an
    /// unrecognised value (a row written by a future version) degrades to
    /// `User` rather than failing the whole read, so the history page can
    /// still render everything else in the thread.
    #[must_use]
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "assistant" => Self::Assistant,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            _ => Self::User,
        }
    }
}

/// One turn as stored in `conversations.sqlite`.
#[derive(Debug, Clone)]
pub struct Turn {
    pub id: Option<i64>,
    pub thread_id: i64,
    pub ordinal: i64,
    pub role: TurnRole,
    pub text: String,
    pub ts: i64,
    /// Name of the enrolled speaker this turn was verified as, when
    /// speaker verification is enabled and produced a match. `None` when
    /// verification is off, nobody is enrolled, or the voice did not clear
    /// the threshold. Only the name is stored — never the embedding.
    pub speaker: Option<String>,
    /// Wall-clock cost of producing this turn, when known.
    pub latency_ms: Option<i64>,
    /// True when generation was cut short (user cancelled mid-reply). The
    /// text is what was produced up to that point.
    pub partial: bool,
}

impl Turn {
    #[must_use]
    pub fn new(thread_id: i64, role: TurnRole, text: impl Into<String>) -> Self {
        Self {
            id: None,
            thread_id,
            ordinal: 0,
            role,
            text: text.into(),
            ts: now_unix(),
            speaker: None,
            latency_ms: None,
            partial: false,
        }
    }
}

/// A conversation thread, with the denormalised summary fields the history
/// page needs so it can render a list without loading every turn.
#[derive(Debug, Clone)]
pub struct Thread {
    pub id: i64,
    pub started_at: i64,
    /// Timestamp of the most recent turn; also what the idle-timeout
    /// segmentation and the resume window are measured against.
    pub last_at: i64,
    /// Set when the thread was deliberately closed (idle timeout, daemon
    /// shutdown, or "Forget conversation"). `None` means still open — which
    /// is also what a crash leaves behind, and is treated as resumable
    /// rather than corrupt.
    pub ended_at: Option<i64>,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub app_class: Option<String>,
    pub app_title: Option<String>,
    pub turn_count: i64,
}

/// A thread plus the extra bits the history list renders: a text preview
/// and the distinct speakers seen across its turns.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub thread: Thread,
    /// First user utterance, truncated — enough to recognise the
    /// conversation without opening it.
    pub preview: Option<String>,
    /// Distinct non-null speaker names across the thread's turns.
    pub speakers: Vec<String>,
}

/// SQLite-backed store of assistant conversations.
pub struct ConversationStore {
    conn: Connection,
}

impl ConversationStore {
    /// Open (or create) the store at `path` and apply migrations. The DB
    /// file — and any WAL/SHM sidecars left by an earlier run — is clamped
    /// to owner-only `0600` on Unix: it holds spoken conversation
    /// transcripts, so it must never be readable by other local users.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|source| Error::Io { path: dir.to_path_buf(), source })?;
        }
        let conn = Connection::open(path)?;
        restrict_to_owner(path);
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        let db = Self { conn: Connection::open_in_memory()? };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS meta(
                key    TEXT PRIMARY KEY,
                value  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS thread(
                id          INTEGER PRIMARY KEY,
                started_at  INTEGER NOT NULL,
                last_at     INTEGER NOT NULL,
                ended_at    INTEGER,
                backend     TEXT,
                model       TEXT,
                app_class   TEXT,
                app_title   TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_thread_last_at
                ON thread(last_at);

            CREATE TABLE IF NOT EXISTS turn(
                id          INTEGER PRIMARY KEY,
                thread_id   INTEGER NOT NULL,
                ordinal     INTEGER NOT NULL,
                role        TEXT NOT NULL,
                text        TEXT NOT NULL,
                ts          INTEGER NOT NULL,
                speaker     TEXT,
                latency_ms  INTEGER,
                partial     INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (thread_id) REFERENCES thread(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_turn_thread
                ON turn(thread_id, ordinal);
            ",
        )?;
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// Start a new thread and return its id.
    pub fn open_thread(
        &self,
        backend: Option<&str>,
        model: Option<&str>,
        app_class: Option<&str>,
        app_title: Option<&str>,
    ) -> Result<i64> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO thread(started_at, last_at, backend, model, app_class, app_title)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
            params![now, backend, model, app_class, app_title],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Append a turn, assigning the next ordinal within its thread and
    /// bumping the thread's `last_at`. Returns the new row id.
    ///
    /// Text is stored **verbatim** — see the module note on why the
    /// dictation redactor does not belong on this path.
    pub fn append_turn(&self, t: &Turn) -> Result<i64> {
        let ordinal: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM turn WHERE thread_id = ?1",
            params![t.thread_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO turn(thread_id, ordinal, role, text, ts, speaker, latency_ms, partial)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                t.thread_id,
                ordinal,
                t.role.as_str(),
                t.text,
                t.ts,
                t.speaker,
                t.latency_ms,
                i64::from(t.partial),
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn
            .execute("UPDATE thread SET last_at = ?2 WHERE id = ?1", params![t.thread_id, t.ts])?;
        Ok(id)
    }

    /// Mark a thread closed. Idempotent — closing an already-closed thread
    /// leaves the original `ended_at` intact.
    pub fn close_thread(&self, thread_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE thread SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![thread_id, now_unix()],
        )?;
        Ok(())
    }

    /// The most recent thread, open or closed.
    pub fn latest_thread(&self) -> Result<Option<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.started_at, t.last_at, t.ended_at, t.backend, t.model,
                    t.app_class, t.app_title,
                    (SELECT COUNT(*) FROM turn WHERE thread_id = t.id)
             FROM thread t ORDER BY t.last_at DESC LIMIT 1",
        )?;
        Ok(stmt.query_row([], row_to_thread).optional()?)
    }

    /// The most recent thread if it is still open **and** its last turn is
    /// within `idle_secs` of now — i.e. the conversation a restart should
    /// pick back up. Returns `None` when the window has lapsed, so the next
    /// utterance starts a fresh thread.
    pub fn resumable_thread(&self, idle_secs: i64) -> Result<Option<Thread>> {
        let Some(t) = self.latest_thread()? else { return Ok(None) };
        if t.ended_at.is_some() || now_unix() - t.last_at > idle_secs {
            return Ok(None);
        }
        Ok(Some(t))
    }

    /// Every turn in a thread, oldest-first.
    pub fn turns(&self, thread_id: i64) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, thread_id, ordinal, role, text, ts, speaker, latency_ms, partial
             FROM turn WHERE thread_id = ?1 ORDER BY ordinal ASC",
        )?;
        let rows = stmt
            .query_map(params![thread_id], row_to_turn)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The last `limit` turns of a thread, oldest-first. Used to rehydrate
    /// the in-memory rolling window on resume without loading a long
    /// conversation in full.
    pub fn recent_turns(&self, thread_id: i64, limit: usize) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, thread_id, ordinal, role, text, ts, speaker, latency_ms, partial
             FROM turn WHERE thread_id = ?1 ORDER BY ordinal DESC LIMIT ?2",
        )?;
        let mut rows = stmt
            .query_map(params![thread_id, limit as i64], row_to_turn)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Recent threads, newest-first, each with a preview and its speakers.
    pub fn recent_threads(&self, limit: usize) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.started_at, t.last_at, t.ended_at, t.backend, t.model,
                    t.app_class, t.app_title,
                    (SELECT COUNT(*) FROM turn WHERE thread_id = t.id)
             FROM thread t ORDER BY t.last_at DESC LIMIT ?1",
        )?;
        let threads = stmt
            .query_map(params![limit as i64], row_to_thread)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        threads
            .into_iter()
            .map(|thread| {
                let preview = self.thread_preview(thread.id)?;
                let speakers = self.thread_speakers(thread.id)?;
                Ok(ThreadSummary { thread, preview, speakers })
            })
            .collect()
    }

    /// First user utterance in a thread, truncated for the list view.
    fn thread_preview(&self, thread_id: i64) -> Result<Option<String>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT text FROM turn WHERE thread_id = ?1 AND role = 'user'
                 ORDER BY ordinal ASC LIMIT 1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(text.map(|t| truncate_chars(&t, 160)))
    }

    /// Distinct speaker names seen across a thread's turns.
    fn thread_speakers(&self, thread_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT speaker FROM turn
             WHERE thread_id = ?1 AND speaker IS NOT NULL ORDER BY speaker",
        )?;
        let names = stmt
            .query_map(params![thread_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    /// Delete a thread and (via `ON DELETE CASCADE`) all of its turns.
    pub fn delete_thread(&self, thread_id: i64) -> Result<bool> {
        let n = self.conn.execute("DELETE FROM thread WHERE id = ?1", params![thread_id])?;
        Ok(n > 0)
    }

    /// Delete every thread. Backs the history page's "clear all" control.
    /// Returns the number removed.
    pub fn delete_all_threads(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM thread", [])?)
    }

    /// Delete threads whose last activity predates `retention_days`.
    /// Returns the number of threads removed. `0` disables retention,
    /// matching [`crate::history::HistoryDb::purge_older_than`].
    pub fn purge_older_than(&self, retention_days: u32) -> Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = now_unix() - i64::from(retention_days) * 86_400;
        let n = self.conn.execute("DELETE FROM thread WHERE last_at < ?1", params![cutoff])?;
        Ok(n)
    }

    /// Total thread count (tests, and the history page's empty state).
    pub fn thread_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM thread", [], |r| r.get::<_, i64>(0))?)
    }
}

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get(0)?,
        started_at: r.get(1)?,
        last_at: r.get(2)?,
        ended_at: r.get(3)?,
        backend: r.get(4)?,
        model: r.get(5)?,
        app_class: r.get(6)?,
        app_title: r.get(7)?,
        turn_count: r.get(8)?,
    })
}

fn row_to_turn(r: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        id: Some(r.get(0)?),
        thread_id: r.get(1)?,
        ordinal: r.get(2)?,
        role: TurnRole::from_db_str(&r.get::<_, String>(3)?),
        text: r.get(4)?,
        ts: r.get(5)?,
        speaker: r.get(6)?,
        latency_ms: r.get(7)?,
        partial: r.get::<_, i64>(8)? != 0,
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Best-effort clamp to owner-only `0600` (main DB + WAL/SHM sidecars).
/// Failure is non-fatal — a read-only FS must not break the assistant —
/// but the common case (0644 from the process umask) is tightened.
fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, mode.clone());
        for suffix in ["-wal", "-shm"] {
            let mut os = path.as_os_str().to_owned();
            os.push(suffix);
            let sidecar = std::path::PathBuf::from(os);
            if sidecar.exists() {
                let _ = std::fs::set_permissions(&sidecar, mode.clone());
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ConversationStore {
        ConversationStore::open_in_memory().unwrap()
    }

    #[test]
    fn append_orders_turns_and_tracks_speaker() {
        let db = store();
        let tid = db.open_thread(Some("groq"), Some("llama-3"), None, None).unwrap();

        let mut user = Turn::new(tid, TurnRole::User, "turn on the lights");
        user.speaker = Some("Alex".into());
        db.append_turn(&user).unwrap();

        let reply = Turn::new(tid, TurnRole::Assistant, "Done.");
        db.append_turn(&reply).unwrap();

        let turns = db.turns(tid).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].ordinal, 0);
        assert_eq!(turns[1].ordinal, 1);
        assert_eq!(turns[0].role, TurnRole::User);
        assert_eq!(turns[0].speaker.as_deref(), Some("Alex"));
        // The assistant's own reply is not attributed to a speaker.
        assert_eq!(turns[1].speaker, None);
    }

    #[test]
    fn tool_calls_are_distinct_rows() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        db.append_turn(&Turn::new(tid, TurnRole::User, "lights")).unwrap();
        db.append_turn(&Turn::new(tid, TurnRole::ToolCall, "light.turn_on")).unwrap();
        db.append_turn(&Turn::new(tid, TurnRole::ToolResult, "ok")).unwrap();
        let roles: Vec<_> = db.turns(tid).unwrap().iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![TurnRole::User, TurnRole::ToolCall, TurnRole::ToolResult]);
    }

    /// The dictation redactor must never be reintroduced on this path.
    ///
    /// Its heuristic is a run of 20+ word characters. That cannot occur in
    /// speech, and occurs constantly in the machine text a conversation
    /// carries — so it only ever destroyed the audit trail. These are real
    /// strings from a real thread, and every one of them was mangled.
    #[test]
    fn conversation_turns_are_stored_verbatim() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        let verbatim = [
            "turn on the light in the master bedroom",
            r#"HassTurnOn {"area":"Master bedroom","domain":["light"]}"#,
            r#"{"success": [{"name": "Master bedroom warm light", "type": "entity", "id": "light.master_bedroom_warm_light"}]}"#,
            "HassTurnOn failed: Error calling tool: <MatchFailedError \
             no_match_reason=<MatchFailedReason.INVALID_AREA: 9>, \
             constraints=MatchTargetsConstraints(name=None, single_target=False)>",
        ];
        for text in verbatim {
            db.append_turn(&Turn::new(tid, TurnRole::User, text)).unwrap();
        }
        let stored = db.turns(tid).unwrap();
        for (row, want) in stored.iter().zip(verbatim) {
            assert_eq!(row.text, want, "a conversation turn must be stored exactly as it happened");
            assert!(!row.text.contains("[REDACTED]"));
        }
    }

    #[test]
    fn partial_turns_round_trip() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        let mut t = Turn::new(tid, TurnRole::Assistant, "I was saying som");
        t.partial = true;
        db.append_turn(&t).unwrap();
        assert!(db.turns(tid).unwrap()[0].partial);
    }

    #[test]
    fn resumable_only_inside_the_idle_window() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        db.append_turn(&Turn::new(tid, TurnRole::User, "hi")).unwrap();
        assert!(db.resumable_thread(900).unwrap().is_some(), "fresh thread must resume");

        // Age the thread past the window.
        db.conn
            .execute("UPDATE thread SET last_at = last_at - 3600 WHERE id = ?1", params![tid])
            .unwrap();
        assert!(db.resumable_thread(900).unwrap().is_none(), "stale thread must not resume");
    }

    #[test]
    fn closed_thread_is_not_resumable() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        db.append_turn(&Turn::new(tid, TurnRole::User, "hi")).unwrap();
        db.close_thread(tid).unwrap();
        assert!(db.resumable_thread(900).unwrap().is_none());
        // But it is still browsable — "forget" ends, it does not delete.
        assert_eq!(db.thread_count().unwrap(), 1);
    }

    #[test]
    fn recent_turns_returns_the_tail_in_order() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        for i in 0..10 {
            db.append_turn(&Turn::new(tid, TurnRole::User, format!("m{i}"))).unwrap();
        }
        let tail = db.recent_turns(tid, 3).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].text, "m7");
        assert_eq!(tail[2].text, "m9");
    }

    #[test]
    fn summary_carries_preview_and_speakers() {
        let db = store();
        let tid = db.open_thread(Some("groq"), None, None, None).unwrap();
        let mut a = Turn::new(tid, TurnRole::User, "first thing said");
        a.speaker = Some("Alex".into());
        db.append_turn(&a).unwrap();
        let mut b = Turn::new(tid, TurnRole::User, "second thing");
        b.speaker = Some("Sam".into());
        db.append_turn(&b).unwrap();

        let s = &db.recent_threads(10).unwrap()[0];
        assert_eq!(s.preview.as_deref(), Some("first thing said"));
        assert_eq!(s.speakers, vec!["Alex".to_string(), "Sam".to_string()]);
        assert_eq!(s.thread.turn_count, 2);
    }

    #[test]
    fn delete_thread_cascades_to_turns() {
        let db = store();
        let tid = db.open_thread(None, None, None, None).unwrap();
        db.append_turn(&Turn::new(tid, TurnRole::User, "hi")).unwrap();
        assert!(db.delete_thread(tid).unwrap());
        assert_eq!(db.thread_count().unwrap(), 0);
        assert!(db.turns(tid).unwrap().is_empty());
        assert!(!db.delete_thread(tid).unwrap(), "second delete is a no-op");
    }

    #[test]
    fn retention_purges_only_stale_threads() {
        let db = store();
        let old = db.open_thread(None, None, None, None).unwrap();
        db.append_turn(&Turn::new(old, TurnRole::User, "ancient")).unwrap();
        db.conn
            .execute(
                "UPDATE thread SET last_at = ?2 WHERE id = ?1",
                params![old, now_unix() - 100 * 86_400],
            )
            .unwrap();
        let fresh = db.open_thread(None, None, None, None).unwrap();
        db.append_turn(&Turn::new(fresh, TurnRole::User, "recent")).unwrap();

        assert_eq!(db.purge_older_than(0).unwrap(), 0, "0 disables retention");
        assert_eq!(db.purge_older_than(30).unwrap(), 1);
        assert_eq!(db.thread_count().unwrap(), 1);
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = store();
        db.migrate().unwrap();
        db.migrate().unwrap();
        let v: i64 = db
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn open_clamps_db_file_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        // Pre-create world-readable (simulates a DB from before the clamp).
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _db = ConversationStore::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "conversation db must be owner-only, got {mode:o}");
    }
}
