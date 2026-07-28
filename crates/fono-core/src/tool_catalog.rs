// SPDX-License-Identifier: GPL-3.0-only
//! Persisted, user-curated catalogue of the tools Fono may call
//! (Phase 1 of `plans/2026-07-26-voice-actions-v4.md`).
//!
//! Fono discovers tools from MCP servers it does not control (Home Assistant
//! today, anything tomorrow). Two measured findings drive this store:
//!
//! * **Fewer tools is both cheaper and more accurate.** Inlining a 155-entity
//!   catalogue dropped one model's pass rate from 0.81 to 0.56 — attention
//!   dilution, not just token cost. So the user must be able to switch tools
//!   off, and prompt size must be under Fono's control rather than the
//!   upstream server's.
//! * **A server can report success for a call that did nothing.** Each tool
//!   therefore records how strongly its outcome can be *verified*
//!   ([`VerifyClass`]), which later gates both what we tell the user and what
//!   we are allowed to learn as a deterministic shortcut.
//!
//! Lifecycle rule — **reconcile, never truncate.** A tool that disappears is
//! marked unavailable, never deleted, and its `enabled` flag is never reset.
//! A server restart or a network blip must not silently re-enable something
//! the user switched off.
//!
//! Storage mirrors the house pattern from [`crate::speakers`]: a dedicated
//! SQLite file, WAL, `CREATE TABLE IF NOT EXISTS`, additive columns probed
//! with `PRAGMA table_info`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// How strongly a tool's outcome can be verified after it runs.
///
/// Ordered weakest to strongest. This is a property *discovered* per tool, not
/// a per-vendor special case: `ResultContract` is generic by protocol (MCP
/// defines `isError` on every tool result), while `PostCondition` additionally
/// requires a readback tool in the same catalogue that observes the effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyClass {
    /// Fire-and-forget: no failure signal at all (a broadcast, an email).
    /// We must never claim such a tool succeeded — only that it was sent.
    None,
    /// The server reports structured failure, but the effect is not
    /// observable.
    ResultContract,
    /// A readback tool exists, so the effect can be re-read and asserted.
    /// The only definitive proof.
    PostCondition,
}

impl VerifyClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ResultContract => "result_contract",
            Self::PostCondition => "post_condition",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "post_condition" => Self::PostCondition,
            "result_contract" => Self::ResultContract,
            _ => Self::None,
        }
    }
}

/// Whether calling a tool is reversible/low-stakes or needs care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Safe,
    Dangerous,
}

impl Capability {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Dangerous => "dangerous",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "dangerous" {
            Self::Dangerous
        } else {
            Self::Safe
        }
    }
}

/// One tool as reported by a discovery pass, before it meets the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTool {
    pub name: String,
    /// The server's own one-line explanation. This is what the model reads
    /// when deciding which tool to reach for, so it is stored verbatim.
    pub description: String,
    /// The tool's JSON schema exactly as the server advertised it.
    pub schema: serde_json::Value,
    pub capability: Capability,
    pub verify_class: VerifyClass,
    /// Name of the tool whose output observes this one's effect, when one was
    /// identified (`GetLiveContext` for Home Assistant).
    pub readback_tool: Option<String>,
}

/// A stored tool, as the rest of Fono sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolRow {
    pub source: String,
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
    pub schema_hash: String,
    pub capability: Capability,
    pub verify_class: VerifyClass,
    pub readback_tool: Option<String>,
    /// Present in the most recent discovery pass.
    pub available: bool,
    /// The user allows Fono to use it. Defaults to `true`.
    pub enabled: bool,
    /// The user has expressed an explicit preference, so defaults must not
    /// overwrite it.
    pub user_touched: bool,
}

/// What one [`ToolCatalogStore::reconcile`] pass changed.
///
/// `prompt_dirty` is the field callers act on: it means the rendered prompt
/// catalogue may have changed, so any pinned prompt-cache prefix built from it
/// is stale and must be re-warmed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    pub added: Vec<String>,
    pub schema_changed: Vec<String>,
    pub went_missing: Vec<String>,
    pub returned: Vec<String>,
    pub prompt_dirty: bool,
}

/// SQLite-backed catalogue of discovered tools and the user's choices about
/// them.
pub struct ToolCatalogStore {
    conn: Connection,
}

impl ToolCatalogStore {
    /// Open (or create) the store at `path` and apply migrations.
    ///
    /// The DB file (and any pre-existing WAL/SHM sidecars) is clamped to
    /// owner-only `0600` on Unix, matching every other Fono store. The
    /// `device_name` / `place_name` tables enumerate the user's smart-home
    /// topology, so they must not be readable by other local users.
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

            CREATE TABLE IF NOT EXISTS tool_source(
                id             INTEGER PRIMARY KEY,
                name           TEXT NOT NULL UNIQUE,
                transport      TEXT NOT NULL,
                last_seen      INTEGER
            );

            CREATE TABLE IF NOT EXISTS tool(
                id             INTEGER PRIMARY KEY,
                source_id      INTEGER NOT NULL REFERENCES tool_source(id) ON DELETE CASCADE,
                name           TEXT NOT NULL,
                schema_json    TEXT NOT NULL,
                schema_hash    TEXT NOT NULL,
                capability     TEXT NOT NULL,
                verify_class   TEXT NOT NULL,
                readback_tool  TEXT,
                first_seen     INTEGER NOT NULL,
                last_seen      INTEGER NOT NULL,
                available      INTEGER NOT NULL DEFAULT 1,
                enabled        INTEGER NOT NULL DEFAULT 1,
                user_touched   INTEGER NOT NULL DEFAULT 0,
                UNIQUE(source_id, name)
            );

            CREATE INDEX IF NOT EXISTS idx_tool_source ON tool(source_id);

            -- The rooms (or equivalent places) a server knows about, learned
            -- when it is connected and re-read on every refresh. Kept here
            -- rather than fetched per command so naming a room costs nothing
            -- at the moment the user is waiting.
            CREATE TABLE IF NOT EXISTS place_name(
                source_id      INTEGER NOT NULL REFERENCES tool_source(id) ON DELETE CASCADE,
                name           TEXT NOT NULL,
                UNIQUE(source_id, name)
            );

            -- The devices a server can actually operate, learned alongside the
            -- places. Home Assistant matches a device by its exact name and
            -- nothing else, so a model left to guess a shortened or reordered
            -- name fails every time. Telling it the real names removes the
            -- guess.
            CREATE TABLE IF NOT EXISTS device_name(
                source_id      INTEGER NOT NULL REFERENCES tool_source(id) ON DELETE CASCADE,
                name           TEXT NOT NULL,
                UNIQUE(source_id, name)
            );
            ",
        )?;
        // Added after the table shipped. `ALTER TABLE … ADD COLUMN` errors
        // when the column is already there, which is the ordinary case, so
        // the failure is the success path and is deliberately ignored.
        let _ = self
            .conn
            .execute("ALTER TABLE tool ADD COLUMN description TEXT NOT NULL DEFAULT ''", []);
        Ok(())
    }

    fn source_id(&self, source: &str, transport: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tool_source(name, transport, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET transport = ?2, last_seen = ?3",
            params![source, transport, now_unix()],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM tool_source WHERE name = ?1",
            params![source],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Fold one discovery pass for `source` into the store.
    ///
    /// Never deletes and never resets `enabled`: a tool absent from `tools` is
    /// marked `available = 0` and keeps the user's choice for when it returns.
    pub fn reconcile(
        &self,
        source: &str,
        transport: &str,
        tools: &[DiscoveredTool],
    ) -> Result<ReconcileReport> {
        let source_id = self.source_id(source, transport)?;
        let now = now_unix();
        let mut report = ReconcileReport::default();

        for tool in tools {
            let schema_json = canonical_json(&tool.schema);
            let hash = sha256_hex(&schema_json);
            let existing: Option<(String, bool)> = self
                .conn
                .query_row(
                    "SELECT schema_hash, available FROM tool WHERE source_id = ?1 AND name = ?2",
                    params![source_id, tool.name],
                    |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
                )
                .optional()?;

            match existing {
                None => {
                    self.conn.execute(
                        "INSERT INTO tool(source_id, name, schema_json, schema_hash, capability,
                                          verify_class, readback_tool, first_seen, last_seen,
                                          available, enabled, user_touched, description)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 1, 1, 0, ?9)",
                        params![
                            source_id,
                            tool.name,
                            schema_json,
                            hash,
                            tool.capability.as_str(),
                            tool.verify_class.as_str(),
                            tool.readback_tool,
                            now,
                            tool.description,
                        ],
                    )?;
                    report.added.push(tool.name.clone());
                    report.prompt_dirty = true;
                }
                Some((old_hash, was_available)) => {
                    // `enabled` and `user_touched` are deliberately absent from
                    // this UPDATE: only the user may change them.
                    self.conn.execute(
                        "UPDATE tool SET schema_json = ?3, schema_hash = ?4, capability = ?5,
                                         verify_class = ?6, readback_tool = ?7, last_seen = ?8,
                                         available = 1, description = ?9
                         WHERE source_id = ?1 AND name = ?2",
                        params![
                            source_id,
                            tool.name,
                            schema_json,
                            hash,
                            tool.capability.as_str(),
                            tool.verify_class.as_str(),
                            tool.readback_tool,
                            now,
                            tool.description,
                        ],
                    )?;
                    if old_hash != hash {
                        report.schema_changed.push(tool.name.clone());
                        report.prompt_dirty = true;
                    }
                    if !was_available {
                        report.returned.push(tool.name.clone());
                        report.prompt_dirty = true;
                    }
                }
            }
        }

        // Anything not in this pass is gone for now — but only for now.
        let seen: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let mut stmt = self
            .conn
            .prepare("SELECT name, enabled FROM tool WHERE source_id = ?1 AND available = 1")?;
        let rows = stmt
            .query_map(params![source_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (name, enabled) in rows {
            if seen.contains(&name.as_str()) {
                continue;
            }
            self.conn.execute(
                "UPDATE tool SET available = 0 WHERE source_id = ?1 AND name = ?2",
                params![source_id, name],
            )?;
            report.went_missing.push(name);
            // Only a tool that was actually in the prompt can dirty it.
            report.prompt_dirty |= enabled;
        }

        report.added.sort();
        report.schema_changed.sort();
        report.went_missing.sort();
        report.returned.sort();
        Ok(report)
    }

    /// Record the user's explicit choice for one tool. Sets `user_touched`, so
    /// a later change of defaults cannot overwrite it.
    pub fn set_enabled(&self, source: &str, name: &str, enabled: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE tool SET enabled = ?3, user_touched = 1
             WHERE name = ?2 AND source_id = (SELECT id FROM tool_source WHERE name = ?1)",
            params![source, name, i64::from(enabled)],
        )?;
        if changed == 0 {
            return Err(Error::Other(format!("unknown tool {source}/{name}")));
        }
        Ok(())
    }

    /// Forget every source the user no longer has configured, and its tools.
    ///
    /// Deliberately a *delete*, not the `available = 0` marking that
    /// [`Self::reconcile`] does. The two look similar but mean opposite
    /// things: a tool going missing from a discovery pass may be a restart or
    /// a network blip, so the user's choice must survive it. Removing the
    /// server is the user saying they are done with it — leaving its tools
    /// listed forever would be a bug, not caution.
    ///
    /// Returns the number of tools forgotten.
    pub fn forget_sources_except(&self, keep: &[String]) -> Result<usize> {
        // `NOT IN ()` has no valid spelling, and `NOT IN (NULL)` is never true
        // — it would silently keep everything, which is the opposite of what
        // "the user removed their last server" means.
        if keep.is_empty() {
            let forgotten = self.conn.execute("DELETE FROM tool", [])?;
            self.conn.execute("DELETE FROM tool_source", [])?;
            return Ok(forgotten);
        }
        // A source list is a handful of rows; build the IN-list by parameter
        // rather than interpolating names into SQL.
        let placeholders = (1..=keep.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let args: Vec<&dyn rusqlite::ToSql> =
            keep.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let doomed = format!("SELECT id FROM tool_source WHERE name NOT IN ({placeholders})");
        let forgotten: usize = self
            .conn
            .execute(&format!("DELETE FROM tool WHERE source_id IN ({doomed})"), args.as_slice())?;
        self.conn.execute(
            &format!("DELETE FROM tool_source WHERE name NOT IN ({placeholders})"),
            args.as_slice(),
        )?;
        Ok(forgotten)
    }

    /// Replace what we know about a server's places.
    ///
    /// Called at connect and refresh, never on the request path — the whole
    /// point is that the model can be told the real names without anyone
    /// waiting for a round-trip to find them out.
    ///
    /// Returns whether the stored set actually changed. Callers use that to
    /// decide whether the warm prompt prefix is stale: these names are *in*
    /// the prompt, so gaining or losing one invalidates a cached prefix built
    /// from the old list, while a refresh that finds the same rooms must not
    /// pay to rebuild it.
    pub fn set_place_names(&self, source: &str, names: &[String]) -> Result<bool> {
        self.replace_names("place_name", source, names)
    }

    /// Every place name across every source, sorted and de-duplicated.
    ///
    /// Sorted because these go into the system prompt, and a prompt whose
    /// bytes shift between turns cannot be a cached prefix.
    pub fn place_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT name FROM place_name ORDER BY name COLLATE NOCASE")?;
        let names = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    /// Replace what we know about a server's operable devices.
    ///
    /// Same timing as [`Self::set_place_names`] — learned at connect and
    /// refresh so naming a device costs nothing while the user is waiting.
    ///
    /// Returns whether the stored set actually changed, for the same reason as
    /// [`Self::set_place_names`]. This is the one that bites in practice: a
    /// lamp renamed in the home stays wrong in Fono's copy until something
    /// rediscovers, and the assistant then tells the user, at length, that a
    /// device they are looking at does not exist.
    pub fn set_device_names(&self, source: &str, names: &[String]) -> Result<bool> {
        self.replace_names("device_name", source, names)
    }

    /// Swap one source's rows in a name table, reporting whether anything moved.
    ///
    /// Compared as a set after trimming, because that is exactly how the names
    /// reach the prompt: de-duplicated and sorted by the readers below. Two
    /// refreshes that find the same rooms in a different order have changed
    /// nothing a model could notice, and must not invalidate the warm prefix.
    fn replace_names(&self, table: &str, source: &str, names: &[String]) -> Result<bool> {
        let id = self.source_id(source, "sse")?;
        let mut stmt =
            self.conn.prepare(&format!("SELECT name FROM {table} WHERE source_id = ?1"))?;
        let before: std::collections::BTreeSet<String> = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        let after: std::collections::BTreeSet<String> =
            names.iter().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).collect();

        // A home that answers but lists nothing is almost always a home that
        // has not finished waking up, not a home someone emptied. Keeping the
        // last known names is the safe reading: the worst case is one stale
        // refresh, where wiping them would leave the assistant insisting the
        // house has no devices at all.
        if after.is_empty() && !before.is_empty() {
            return Ok(false);
        }

        self.conn.execute(&format!("DELETE FROM {table} WHERE source_id = ?1"), params![id])?;
        for n in &after {
            self.conn.execute(
                &format!("INSERT OR IGNORE INTO {table}(source_id, name) VALUES (?1, ?2)"),
                params![id, n],
            )?;
        }
        Ok(before != after)
    }

    /// Every device name across every source, sorted and de-duplicated.
    ///
    /// Sorted for the same reason as the places: these become part of a
    /// system prompt we want to stay byte-stable between turns.
    pub fn device_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT name FROM device_name ORDER BY name COLLATE NOCASE")?;
        let names = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    /// Every tool ever seen from every source, sorted by `(source, name)`.
    pub fn all_tools(&self) -> Result<Vec<ToolRow>> {
        self.select_tools(false)
    }

    /// The tools that may actually be offered to the model: enabled by the
    /// user *and* present in the last discovery pass.
    pub fn active_tools(&self) -> Result<Vec<ToolRow>> {
        self.select_tools(true)
    }

    fn select_tools(&self, active_only: bool) -> Result<Vec<ToolRow>> {
        let filter = if active_only { "WHERE t.enabled = 1 AND t.available = 1" } else { "" };
        let sql = format!(
            "SELECT s.name, t.name, t.schema_json, t.schema_hash, t.capability, t.verify_class,
                    t.readback_tool, t.available, t.enabled, t.user_touched, t.description
             FROM tool t JOIN tool_source s ON s.id = t.source_id
             {filter}
             ORDER BY s.name, t.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |r| {
                let schema_json: String = r.get(2)?;
                Ok(ToolRow {
                    source: r.get(0)?,
                    name: r.get(1)?,
                    schema: serde_json::from_str(&schema_json).unwrap_or(serde_json::Value::Null),
                    schema_hash: r.get(3)?,
                    capability: Capability::parse(&r.get::<_, String>(4)?),
                    verify_class: VerifyClass::parse(&r.get::<_, String>(5)?),
                    readback_tool: r.get(6)?,
                    available: r.get::<_, i64>(7)? != 0,
                    enabled: r.get::<_, i64>(8)? != 0,
                    user_touched: r.get::<_, i64>(9)? != 0,
                    description: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Canonical rendering of the active catalogue, for the model prompt.
    ///
    /// Byte-stable: rows are sorted by `(source, name)` and every schema is
    /// re-serialised with sorted keys, so the same catalogue always produces
    /// the same bytes regardless of discovery order or SQLite row order. That
    /// stability is what lets a pinned prompt-cache prefix stay valid.
    pub fn render_catalogue(&self) -> Result<String> {
        let tools = self.active_tools()?;
        let mut out = String::new();
        for tool in &tools {
            out.push_str(&tool.name);
            out.push('\t');
            out.push_str(&canonical_json(&tool.schema));
            out.push('\n');
        }
        Ok(out)
    }

    /// Fingerprint of the rendered active catalogue. Changes exactly when the
    /// prompt would change, so callers can cheaply decide whether a warmed
    /// prefix is still valid.
    pub fn catalogue_hash(&self) -> Result<String> {
        Ok(sha256_hex(&self.render_catalogue()?))
    }
}

/// Serialise with sorted keys. `serde_json`'s default map is a `BTreeMap`
/// (the `preserve_order` feature is not enabled anywhere in the workspace),
/// so this is deterministic across processes and platforms.
fn canonical_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Names that mean "this tool reads the world back" — a `PostCondition`
/// verifier needs one of these in the same catalogue.
const READBACK_HINTS: [&str; 3] = ["context", "state", "status"];

/// Verbs that mean "the effect leaves before we can check it".
const FIRE_AND_FORGET: [&str; 4] = ["broadcast", "notify", "announce", "send"];

/// Verbs whose effect a user would not want replayed from a learned shortcut,
/// or performed on a misheard word.
const DANGEROUS: [&str; 8] =
    ["unlock", "disarm", "delete", "remove", "purge", "reset", "pay", "purchase"];

/// Pick the tool that can observe other tools' effects, if the catalogue has
/// one. `GetLiveContext` for Home Assistant.
///
/// A heuristic by design: the result is *stored*, so a wrong guess is one row
/// to correct rather than a code change. Prefers a read-shaped name that also
/// mentions state/context/status, so `GetDateTime` never wins over
/// `GetLiveContext`.
#[must_use]
pub fn pick_readback_tool(names: &[String]) -> Option<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    sorted
        .into_iter()
        .find(|n| {
            let lower = n.to_lowercase();
            let reads = ["get", "list", "read", "query"].iter().any(|v| lower.contains(v));
            reads && READBACK_HINTS.iter().any(|h| lower.contains(h))
        })
        .cloned()
}

/// Classify one discovered tool: how dangerous is it, and how well can we
/// prove it worked?
///
/// `readback` is the catalogue's readback tool (see [`pick_readback_tool`]);
/// pass `None` when the server offers none.
///
/// The classes are deliberately conservative. A tool we cannot check is
/// [`VerifyClass::None`], which means Fono may say it *sent* the request but
/// never that it *worked* — and it can never be promoted to a replayed
/// shortcut.
#[must_use]
pub fn classify(name: &str, readback: Option<&str>) -> (Capability, VerifyClass, Option<String>) {
    let lower = name.to_lowercase();
    let capability = if DANGEROUS.iter().any(|v| lower.contains(v)) {
        Capability::Dangerous
    } else {
        Capability::Safe
    };

    // The readback tool itself observes rather than changes: nothing to
    // re-read, but the server still reports structured failure.
    let is_readback = readback == Some(name);
    let fire_and_forget = FIRE_AND_FORGET.iter().any(|v| lower.contains(v));

    let (verify, tool) = match (fire_and_forget, is_readback, readback) {
        // Nothing observable happens, so the request is all we can report.
        (true, _, _) => (VerifyClass::None, None),
        // The readback tool only observes: nothing to re-read afterwards,
        // but the server still reports structured failure.
        (_, true, _) => (VerifyClass::ResultContract, None),
        (_, _, Some(r)) => (VerifyClass::PostCondition, Some(r.to_owned())),
        (_, _, None) => (VerifyClass::ResultContract, None),
    };
    (capability, verify, tool)
}

fn sha256_hex(s: &str) -> String {
    use std::fmt::Write as _;
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Best-effort clamp to owner-only `0600` (main DB + WAL/SHM sidecars).
/// Failure is non-fatal — a read-only FS must not break tool discovery.
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

    #[cfg(unix)]
    #[test]
    fn open_clamps_db_file_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.sqlite");
        // Pre-create world-readable, as every catalogue written before the
        // clamp landed will be. Opening must tighten it in place.
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _db = ToolCatalogStore::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tool catalogue must be owner-only, got {mode:o}");
    }

    /// The exact 26 tools a real Home Assistant 2026.7 advertises. Pinned so
    /// the heuristics are anchored to a catalogue that exists rather than to
    /// invented names.
    const REAL_HA_TOOLS: [&str; 26] = [
        "GetDateTime",
        "GetLiveContext",
        "HassBroadcast",
        "HassCancelAllTimers",
        "HassClimateSetTemperature",
        "HassLightSet",
        "HassListAddItem",
        "HassListCompleteItem",
        "HassListRemoveItem",
        "HassMediaNext",
        "HassMediaPause",
        "HassMediaPlayerMute",
        "HassMediaPlayerUnmute",
        "HassMediaPrevious",
        "HassMediaSearchAndPlay",
        "HassMediaUnpause",
        "HassSetPosition",
        "HassSetVolume",
        "HassSetVolumeRelative",
        "HassStopMoving",
        "HassTurnOff",
        "HassTurnOn",
        "HassVacuumCleanArea",
        "HassVacuumReturnToBase",
        "HassVacuumStart",
        "todo_get_items",
    ];

    #[test]
    fn classifies_the_real_home_assistant_catalogue() {
        let names: Vec<String> = REAL_HA_TOOLS.iter().map(|s| (*s).to_string()).collect();
        let readback = pick_readback_tool(&names);
        // `GetLiveContext`, not `GetDateTime` — the clock says nothing about
        // whether a light came on.
        assert_eq!(readback.as_deref(), Some("GetLiveContext"));

        let of = |n: &str| classify(n, readback.as_deref());

        // The commands we actually care about can be proven by re-reading.
        let (cap, verify, tool) = of("HassTurnOn");
        assert_eq!(cap, Capability::Safe);
        assert_eq!(verify, VerifyClass::PostCondition);
        assert_eq!(tool.as_deref(), Some("GetLiveContext"));

        // A broadcast leaves no trace to check, so Fono may only say "sent".
        assert_eq!(of("HassBroadcast").1, VerifyClass::None);

        // The readback tool observes; there is nothing to re-read after it.
        assert_eq!(of("GetLiveContext").1, VerifyClass::ResultContract);

        // Deleting is never replayed from a shortcut, even though a list
        // item is harmless — the heuristic errs toward caution.
        assert_eq!(of("HassListRemoveItem").0, Capability::Dangerous);
    }

    fn tool(name: &str, schema: serde_json::Value) -> DiscoveredTool {
        DiscoveredTool {
            name: name.to_string(),
            description: format!("does {name}"),
            schema,
            capability: Capability::Safe,
            verify_class: VerifyClass::PostCondition,
            readback_tool: Some("GetLiveContext".to_string()),
        }
    }

    fn light_on() -> DiscoveredTool {
        tool("HassTurnOn", serde_json::json!({ "properties": { "area": { "type": "string" } } }))
    }

    fn light_off() -> DiscoveredTool {
        tool("HassTurnOff", serde_json::json!({ "properties": { "area": { "type": "string" } } }))
    }

    /// Removing a server is not the same event as a server going quiet, even
    /// though both end with tools that are not there. A blip must leave the
    /// user's choices intact; a removal must leave nothing behind at all.
    #[test]
    fn removing_a_server_forgets_its_tools_but_a_blip_does_not() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        db.set_enabled("ha", "HassTurnOff", false).unwrap();

        // A blip: the server answers with nothing. The rows survive, and the
        // switched-off choice survives with them.
        db.reconcile("ha", "sse", &[]).unwrap();
        let rows = db.all_tools().unwrap();
        assert_eq!(rows.len(), 2, "a quiet server must not erase what we knew");
        assert!(rows.iter().all(|r| !r.available));
        assert!(!rows.iter().find(|r| r.name == "HassTurnOff").unwrap().enabled);

        // Removal: the user deleted the server from their config.
        let forgotten = db.forget_sources_except(&["other".to_string()]).unwrap();
        assert_eq!(forgotten, 2);
        assert!(db.all_tools().unwrap().is_empty(), "a removed server must leave no tools listed");

        // Removing every server is the empty-list case, which must not be
        // read as "keep everything".
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        db.forget_sources_except(&[]).unwrap();
        assert!(db.all_tools().unwrap().is_empty());
    }

    /// Room names are learned once and read on every turn, so two things
    /// matter: refreshing must not accumulate stale rooms, and removing a
    /// server must not leave the model being told about rooms that are gone.
    #[test]
    fn room_names_are_replaced_on_refresh_and_forgotten_with_the_server() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        db.set_place_names("ha", &names(&["Kitchen", "Office"])).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Kitchen", "Office"]);

        // A rename upstream must not leave the old name behind: the point of
        // the list is that every name in it is real.
        db.set_place_names("ha", &names(&["Kitchen", "Study"])).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Kitchen", "Study"]);

        // Two servers pool their rooms, sorted, so the prompt bytes do not
        // depend on which one was connected first.
        db.reconcile("cabin", "sse", &[light_on()]).unwrap();
        db.set_place_names("cabin", &names(&["Attic"])).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Attic", "Kitchen", "Study"]);

        db.forget_sources_except(&["cabin".to_string()]).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Attic"]);
    }

    /// Device names follow the same rules as room names, for the same reason:
    /// every name we hand the model has to be one the home will still match.
    #[test]
    fn device_names_are_replaced_on_refresh_and_forgotten_with_the_server() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        db.set_device_names("ha", &names(&["Office outdoor light", "Kitchen lights"])).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Kitchen lights", "Office outdoor light"]);

        // A lamp that was renamed or unexposed must drop out, or we would keep
        // offering the model a name that no longer resolves.
        db.set_device_names("ha", &names(&["Kitchen lights"])).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Kitchen lights"]);

        db.reconcile("cabin", "sse", &[light_on()]).unwrap();
        db.set_device_names("cabin", &names(&["Attic lamp"])).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Attic lamp", "Kitchen lights"]);

        db.forget_sources_except(&["cabin".to_string()]).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Attic lamp"]);
    }

    /// The refresh that runs at every startup has to say whether it found
    /// anything different, because that answer decides whether the warm prompt
    /// cache is thrown away. Getting it wrong either way is expensive: a false
    /// "no change" leaves the assistant using yesterday's device list, and a
    /// false "changed" makes the next command wait for a needless rebuild.
    #[test]
    fn a_refresh_reports_whether_the_names_actually_moved() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        // First sight of a house is a change — there was nothing before.
        assert!(db.set_device_names("ha", &names(&["Kitchen lights"])).unwrap());
        assert!(db.set_place_names("ha", &names(&["Kitchen"])).unwrap());

        // Finding the same house again is not.
        assert!(!db.set_device_names("ha", &names(&["Kitchen lights"])).unwrap());
        assert!(!db.set_place_names("ha", &names(&["Kitchen"])).unwrap());

        // Order and stray spacing are not changes: the names are sorted and
        // trimmed before they ever reach the model, so the prompt is identical.
        assert!(!db.set_device_names("ha", &names(&["  Kitchen lights  ", ""])).unwrap());

        // A rename is the case that matters — this is the one that used to
        // leave fono insisting a lamp the user is looking at does not exist.
        assert!(db.set_device_names("ha", &names(&["Lampa de sare"])).unwrap());
        assert_eq!(db.device_names().unwrap(), vec!["Lampa de sare"]);

        // And an addition.
        assert!(db.set_device_names("ha", &names(&["Lampa de sare", "Hallway AC"])).unwrap());
    }

    /// A home that is still waking up answers with an empty list. Startup is
    /// exactly when that happens, and wiping the catalogue would leave the
    /// assistant telling the user their house has no devices in it.
    #[test]
    fn an_empty_answer_never_wipes_a_known_house() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        db.set_device_names("ha", &names(&["Kitchen lights"])).unwrap();
        db.set_place_names("ha", &names(&["Kitchen"])).unwrap();

        assert!(!db.set_device_names("ha", &[]).unwrap());
        assert!(!db.set_place_names("ha", &[]).unwrap());
        assert_eq!(db.device_names().unwrap(), vec!["Kitchen lights"]);
        assert_eq!(db.place_names().unwrap(), vec!["Kitchen"]);

        // Deliberately removing a server still forgets it — that path is
        // `forget_sources_except`, and it is not weakened by the guard above.
        db.forget_sources_except(&[]).unwrap();
        assert!(db.device_names().unwrap().is_empty());
    }

    #[test]
    fn newly_discovered_tools_are_enabled_by_default() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        let report = db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();

        assert_eq!(report.added, vec!["HassTurnOff", "HassTurnOn"]);
        assert!(report.prompt_dirty);
        let active = db.active_tools().unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|t| t.enabled && t.available && !t.user_touched));
    }

    #[test]
    fn a_tool_the_user_disabled_stays_disabled_when_it_disappears_and_returns() {
        // The load-bearing lifecycle rule: a server restart or network blip
        // must never silently re-enable something the user switched off.
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        db.set_enabled("ha", "HassTurnOff", false).unwrap();

        // The server drops off entirely.
        let gone = db.reconcile("ha", "sse", &[]).unwrap();
        assert_eq!(gone.went_missing, vec!["HassTurnOff", "HassTurnOn"]);
        assert!(db.active_tools().unwrap().is_empty());

        // ... and comes back with both tools.
        let back = db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        assert_eq!(back.returned, vec!["HassTurnOff", "HassTurnOn"]);
        assert!(back.added.is_empty(), "returning tools must not be re-inserted");

        let active = db.active_tools().unwrap();
        assert_eq!(
            active.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["HassTurnOn"],
            "the disabled tool must not come back enabled"
        );
        let all = db.all_tools().unwrap();
        let off = all.iter().find(|t| t.name == "HassTurnOff").unwrap();
        assert!(off.available && !off.enabled && off.user_touched);
    }

    #[test]
    fn disappearing_disabled_tool_does_not_dirty_the_prompt() {
        // It was not in the prompt, so its absence cannot change the prompt —
        // and needlessly invalidating the warmed prefix costs a cold prefill.
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        db.set_enabled("ha", "HassTurnOff", false).unwrap();

        let report = db.reconcile("ha", "sse", &[light_on()]).unwrap();
        assert_eq!(report.went_missing, vec!["HassTurnOff"]);
        assert!(!report.prompt_dirty);
    }

    #[test]
    fn a_changed_schema_is_reported_and_invalidates_the_prompt() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let before = db.catalogue_hash().unwrap();

        let evolved = tool(
            "HassTurnOn",
            serde_json::json!({ "properties": { "area": { "type": "string" },
                                                "domain": { "type": "array" } } }),
        );
        let report = db.reconcile("ha", "sse", &[evolved]).unwrap();

        assert_eq!(report.schema_changed, vec!["HassTurnOn"]);
        assert!(report.prompt_dirty);
        assert_ne!(db.catalogue_hash().unwrap(), before);
    }

    #[test]
    fn an_unchanged_pass_changes_nothing() {
        // Re-discovery is frequent; it must not thrash the prompt cache.
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        let hash = db.catalogue_hash().unwrap();

        let report = db.reconcile("ha", "sse", &[light_off(), light_on()]).unwrap();
        assert_eq!(report, ReconcileReport::default());
        assert_eq!(db.catalogue_hash().unwrap(), hash);
    }

    #[test]
    fn the_rendered_catalogue_is_byte_stable_regardless_of_discovery_order() {
        // A pinned prompt-cache prefix is only valid while these bytes are.
        let a = ToolCatalogStore::open_in_memory().unwrap();
        a.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        let b = ToolCatalogStore::open_in_memory().unwrap();
        b.reconcile("ha", "sse", &[light_off(), light_on()]).unwrap();

        assert_eq!(a.render_catalogue().unwrap(), b.render_catalogue().unwrap());
        assert_eq!(a.catalogue_hash().unwrap(), b.catalogue_hash().unwrap());
    }

    #[test]
    fn disabling_a_tool_shrinks_the_prompt() {
        // The measured reason this store exists: fewer tools is both cheaper
        // and more accurate.
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        let full = db.render_catalogue().unwrap();

        db.set_enabled("ha", "HassTurnOff", false).unwrap();
        let pruned = db.render_catalogue().unwrap();

        assert!(pruned.len() < full.len());
        assert!(!pruned.contains("HassTurnOff"));
        assert!(pruned.contains("HassTurnOn"));
    }

    #[test]
    fn verification_class_survives_a_round_trip() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        let broadcast = DiscoveredTool {
            name: "HassBroadcast".to_string(),
            description: "announce something".to_string(),
            schema: serde_json::json!({}),
            capability: Capability::Dangerous,
            verify_class: VerifyClass::None,
            readback_tool: None,
        };
        db.reconcile("ha", "sse", &[broadcast, light_on()]).unwrap();

        let all = db.all_tools().unwrap();
        let bc = all.iter().find(|t| t.name == "HassBroadcast").unwrap();
        assert_eq!(bc.verify_class, VerifyClass::None);
        assert_eq!(bc.capability, Capability::Dangerous);
        assert_eq!(bc.readback_tool, None);

        let on = all.iter().find(|t| t.name == "HassTurnOn").unwrap();
        assert_eq!(on.verify_class, VerifyClass::PostCondition);
        assert_eq!(on.readback_tool.as_deref(), Some("GetLiveContext"));
    }

    #[test]
    fn sources_are_kept_apart() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        db.reconcile("notes", "stdio", &[tool("AddNote", serde_json::json!({}))]).unwrap();

        // A pass for one source must not mark the other's tools missing.
        let report = db.reconcile("ha", "sse", &[light_on()]).unwrap();
        assert!(report.went_missing.is_empty());
        assert_eq!(db.active_tools().unwrap().len(), 2);
    }

    #[test]
    fn choices_survive_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.sqlite");
        {
            let db = ToolCatalogStore::open(&path).unwrap();
            db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
            db.set_enabled("ha", "HassTurnOff", false).unwrap();
        }
        let db = ToolCatalogStore::open(&path).unwrap();
        let active = db.active_tools().unwrap();
        assert_eq!(active.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["HassTurnOn"]);
    }

    #[test]
    fn setting_an_unknown_tool_is_an_error() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        assert!(db.set_enabled("ha", "NoSuchTool", false).is_err());
    }

    /// A slice of the real Home Assistant catalogue (26 tools, 2026.7.3).
    fn ha_names() -> Vec<String> {
        ["GetDateTime", "GetLiveContext", "HassBroadcast", "HassTurnOn", "HassTurnOff"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    #[test]
    fn readback_pick_prefers_the_state_reader_over_other_getters() {
        // `GetDateTime` sorts first and is read-shaped, but tells us nothing
        // about the world we just changed.
        assert_eq!(pick_readback_tool(&ha_names()).as_deref(), Some("GetLiveContext"));
    }

    #[test]
    fn a_server_without_a_reader_gets_no_readback() {
        let names = vec!["SendEmail".to_owned(), "HassBroadcast".to_owned()];
        assert_eq!(pick_readback_tool(&names), None);
    }

    #[test]
    fn a_mutation_with_a_reader_is_provable() {
        let (cap, verify, readback) = classify("HassTurnOn", Some("GetLiveContext"));
        assert_eq!(cap, Capability::Safe);
        assert_eq!(verify, VerifyClass::PostCondition);
        assert_eq!(readback.as_deref(), Some("GetLiveContext"));
    }

    #[test]
    fn a_broadcast_is_never_provable_even_with_a_reader() {
        // The message has left the building; reading state back proves
        // nothing about whether anyone received it.
        let (_, verify, readback) = classify("HassBroadcast", Some("GetLiveContext"));
        assert_eq!(verify, VerifyClass::None);
        assert_eq!(readback, None);
    }

    #[test]
    fn the_reader_does_not_verify_itself() {
        let (_, verify, readback) = classify("GetLiveContext", Some("GetLiveContext"));
        assert_eq!(verify, VerifyClass::ResultContract);
        assert_eq!(readback, None);
    }

    #[test]
    fn without_a_reader_we_fall_back_to_the_error_contract() {
        let (_, verify, readback) = classify("HassTurnOn", None);
        assert_eq!(verify, VerifyClass::ResultContract);
        assert_eq!(readback, None);
    }

    #[test]
    fn irreversible_verbs_are_flagged_dangerous() {
        for name in ["HassUnlock", "delete_file", "DisarmAlarm", "purchase_item"] {
            assert_eq!(classify(name, None).0, Capability::Dangerous, "{name}");
        }
        assert_eq!(classify("HassTurnOn", None).0, Capability::Safe);
    }
}
