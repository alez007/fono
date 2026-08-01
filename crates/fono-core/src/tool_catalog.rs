// SPDX-License-Identifier: GPL-3.0-only
//! Persisted, user-curated catalogue of the tools Fono may call.
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

/// One thing in the home that a command can operate.
///
/// The kind is what the server calls the sort of thing this is — `light`,
/// `cover`, `media_player`. Fono only ever repeats it back: it is used to
/// work out which kinds are present in this home, never interpreted. A
/// server that does not report one leaves it empty, and Fono falls back to
/// treating the device as being of no particular kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Device {
    pub name: String,
    pub domain: String,
    /// How many times a command has actually reached this device. Zero for
    /// everything in the home nothing has ever been asked to do, which is most
    /// of it.
    pub runs: i64,
    /// When it was last reached, in Unix seconds.
    pub last_run: Option<i64>,
    /// Whether that last attempt landed. `None` until there has been one.
    ///
    /// This is per *device*, not per command: a single instruction naming an area
    /// routinely switches five things and fails on the sixth, and only this
    /// tells you which sixth.
    pub last_ok: Option<bool>,
}

impl Device {
    /// Convenience for tests and for callers that only have a name.
    pub fn new(name: impl Into<String>, domain: impl Into<String>) -> Self {
        Self { name: name.into(), domain: domain.into(), ..Self::default() }
    }
}

/// The one name of a device that a command may actually use.
///
/// A home records a device's alternative names on the same line, comma
/// separated: one speaker in the home this was built against is called
/// `Office display, Boxa birou` — an English name and a Romanian one. The whole
/// line is kept, because a reply naming either has to be recognised as that
/// speaker, but only the leading name is ever offered to the model. The joined
/// string is a name the home itself refuses, so offering it hands the model a
/// device it cannot operate and makes the failure look like the model's.
pub fn primary_name(stored: &str) -> &str {
    stored.split(',').next().unwrap_or(stored).trim()
}

/// A phrase reduced to the form two utterances of it can be compared in.
///
/// Lowercase, no punctuation, single spaces. Matching is exact on this form,
/// because a fast path that guesses is only a worse model.
///
/// Diacritics are deliberately *not* folded. A recogniser that spells the same
/// Romanian sentence two ways simply produces two rows pointing at one command,
/// which is how this store already handles two languages, and each earns the
/// fast path on its own merits. Folding them would mean carrying a table of
/// letter equivalences for every language Fono is spoken in — maintenance for a
/// case the data model already covers.
#[must_use]
pub fn normalise_phrase(said: &str) -> String {
    let mut out = String::with_capacity(said.len());
    let mut space = true;
    for ch in said.chars() {
        if ch.is_ascii_punctuation() {
            continue;
        }
        if ch.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
            continue;
        }
        for low in ch.to_lowercase() {
            out.push(low);
            space = false;
        }
    }
    out.trim().to_owned()
}

/// How long after a reply a command about the same thing reads as a complaint
/// rather than as a new request.
///
/// Load-bearing rather than a tuning knob. Without the bound, the best
/// candidates for the fast path would be the ones it excludes: "turn on the
/// kitchen lights" said again half an hour later means someone switched them
/// off, not that Fono got it wrong. A complaint is immediate — an unobeyed user
/// repeats themselves at once — so this is generous for that and far too short
/// to catch a real second use.
pub const COMPLAINT_WINDOW_SECS: i64 = 30;

/// How many clean runs in a row earn a phrase the fast path.
///
/// Asymmetric with demotion on purpose: two to promote, one to demote. A
/// promotion that does not happen costs a couple of seconds once; a wrong
/// replay moves the wrong thing in the physical world.
pub const CLEAN_RUNS_TO_PROMOTE: i64 = 2;

/// Who wrote a phrase down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Fono heard it work.
    Learned,
    /// The user added it. Executed and judged exactly like a learned one, and
    /// never trusted more: it starts unpromoted like everything else.
    Written,
}

impl Origin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Learned => "learned",
            Self::Written => "written",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "written" {
            Self::Written
        } else {
            Self::Learned
        }
    }
}

/// Why a phrase cannot be replayed at the moment, when it cannot.
///
/// A shortcut is a standing hypothesis about the world, not a fact. The world
/// moves underneath it — a device is renamed, a server stops offering a tool,
/// the user switches one off — and a shortcut that kept firing blind would be
/// fast and confidently wrong, which is strictly worse than being slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stale {
    /// The command it replays is switched off, or its server is no longer
    /// offering it. Nothing has to be re-learned; it resumes when the tool does.
    Paused,
    /// The command's published shape has moved since this was learned, so what
    /// would be replayed is no longer what worked.
    Changed,
}

/// One phrase and the command it stands for.
///
/// The phrase is kept in the user's own words as well as in comparison form,
/// because the words are what the page shows and the form is what is matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shortcut {
    /// As spoken or typed.
    pub phrase: String,
    /// What the recogniser said the language was, when it said.
    pub lang: String,
    pub source: String,
    pub tool: String,
    /// The arguments as they were actually sent, after every correction.
    pub args: String,
    pub origin: Origin,
    pub runs: i64,
    /// Runs in a row that finished with no error and drew no complaint.
    pub clean: i64,
    pub last_run: Option<i64>,
    pub last_ok: Option<bool>,
    /// How long the command itself took last time, in milliseconds. This is
    /// the figure a fast path is measured by.
    pub last_ms: Option<i64>,
    /// Absent when the phrase can be replayed right now.
    pub stale: Option<Stale>,
}

impl Shortcut {
    /// Has this earned the right to run before the model is asked?
    #[must_use]
    pub fn fast(&self) -> bool {
        self.stale.is_none() && self.clean >= CLEAN_RUNS_TO_PROMOTE
    }

    /// The one word a page may put beside this phrase.
    ///
    /// Decided here rather than in the page, because it is the same rule
    /// [`Self::fast`] applies and a second copy of it would drift: a row that
    /// reads `fast` and is not replayed, or the reverse, is precisely the sort of
    /// disagreement between mechanism and display this whole catalogue exists to
    /// prevent.
    ///
    /// Why cannot-replay comes first: it is the only state that is *about the
    /// world* rather than about the phrase's record, so it outranks anything
    /// earned. A phrase whose tool is switched off has still earned its two
    /// clean runs, and saying `fast` while nothing would happen would be a lie.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self.stale {
            Some(Stale::Paused) => "paused",
            Some(Stale::Changed) => "changed",
            None if self.clean >= CLEAN_RUNS_TO_PROMOTE => "fast",
            None if self.origin == Origin::Written => "written",
            None => "learning",
        }
    }
}

/// One command a phrase produced, as the turn that ran it saw it.
#[derive(Debug, Clone, Copy)]
pub struct Said<'a> {
    /// The user's own words.
    pub phrase: &'a str,
    pub lang: &'a str,
    pub source: &'a str,
    pub tool: &'a str,
    /// Exactly what was sent, so a replay sends the same thing.
    pub args: &'a str,
    /// The things in the home the command reached, when the server names them.
    /// Empty changes what counts as a complaint — see
    /// [`ToolCatalogStore::settle`].
    pub devices: &'a [String],
    /// The reply reported no error.
    pub ok: bool,
    /// How long the command took, in milliseconds.
    pub ms: i64,
}

/// One server the catalogue has heard from, and when it last answered.
///
/// `last_seen` is a Unix timestamp, absent only for a row written before the
/// column existed. Kept separate from the configured server list because the
/// two answer different questions: config says what the user asked for, this
/// says what replied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRow {
    pub name: String,
    pub transport: String,
    pub last_seen: Option<i64>,
}

/// How a tool call turned out, as far as Fono is entitled to claim.
///
/// Deliberately not a boolean. A tool Fono cannot check afterwards
/// ([`VerifyClass::None`]) can only ever be reported as [`Self::Sent`] — the
/// request left, and nothing more is known. Collapsing that into "worked"
/// would put a claim on the page that nothing supports, which is the failure
/// this whole area keeps producing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// Checked afterwards, and the world is as the user asked.
    Confirmed,
    /// The server accepted it and reported no error.
    Accepted,
    /// The request went out; nothing about the result is knowable.
    Sent,
    /// It failed, or the check said the world did not change.
    Failed,
}

impl RunOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Accepted => "accepted",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "confirmed" => Some(Self::Confirmed),
            "accepted" => Some(Self::Accepted),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The last time a tool actually ran, and how it went.
///
/// One row per tool rather than a full log: the questions this answers are
/// "did this ever work, when, and for whom", and a growing history of every
/// command anyone has ever spoken is a privacy cost with no matching use. The
/// count is kept because "ran once" and "ran forty times" are different
/// things to a reader, and because Fono needs it later to decide what may be
/// replayed — but it is a counter, not a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastRun {
    /// Unix seconds.
    pub at: i64,
    pub outcome: RunOutcome,
    /// How long the call itself took, in milliseconds — the round trip to the
    /// server plus whatever re-reading was needed to confirm it.
    pub ms: i64,
    /// How long the assistant spent deciding to make this call, in
    /// milliseconds: from the moment the turn's tools were built (or the
    /// previous call returned) to the moment this one was issued. Usually the
    /// larger of the two, and the reason a command feels slow — so reporting
    /// only `ms` reads as a boast rather than a measurement. `None` for runs
    /// recorded before it was measured.
    pub think_ms: Option<i64>,
    /// The enrolled speaker Fono recognised, when it recognised one. `None`
    /// means nobody was identified — not that nobody spoke.
    pub speaker: Option<String>,
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
    /// How many times this has been run. Zero for everything discovered and
    /// never used, which is most of a catalogue.
    pub runs: i64,
    /// The most recent run, absent until there has been one.
    pub last_run: Option<LastRun>,
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

            -- The areas (or equivalent places) a server knows about, learned
            -- when it is connected and re-read on every refresh. Kept here
            -- rather than fetched per command so naming an area costs nothing
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

            -- Phrases that have already produced a working command, and the
            -- command each one produced. One row per phrase and no history:
            -- many phrases may point at the same command, which is how one
            -- action is reachable in two languages without anything being
            -- translated or guessed at.
            --
            -- Not keyed to tool_source by id, because a server the user
            -- removes takes its tools with it and these have to go too —
            -- see `forget_sources_except`.
            CREATE TABLE IF NOT EXISTS shortcut(
                id              INTEGER PRIMARY KEY,
                phrase_norm     TEXT NOT NULL UNIQUE,
                phrase_raw      TEXT NOT NULL,
                lang            TEXT NOT NULL DEFAULT '',
                source          TEXT NOT NULL,
                tool            TEXT NOT NULL,
                args_json       TEXT NOT NULL,
                -- The shape the command had when it was learned. A tool whose
                -- schema has moved since is no longer the tool that worked.
                schema_hash     TEXT NOT NULL DEFAULT '',
                origin          TEXT NOT NULL DEFAULT 'learned',
                runs            INTEGER NOT NULL DEFAULT 0,
                clean           INTEGER NOT NULL DEFAULT 0,
                last_run        INTEGER,
                last_ok         INTEGER,
                last_ms         INTEGER,
                -- The one run not yet judged: when its reply finished, and
                -- which devices it reached. Judging is lazy, so this is where
                -- a run waits for its window to close.
                pending_at      INTEGER,
                pending_devices TEXT NOT NULL DEFAULT ''
            );
            ",
        )?;
        // Added after the table shipped. `ALTER TABLE … ADD COLUMN` errors
        // when the column is already there, which is the ordinary case, so
        // the failure is the success path and is deliberately ignored.
        let _ = self
            .conn
            .execute("ALTER TABLE tool ADD COLUMN description TEXT NOT NULL DEFAULT ''", []);
        // The kind of thing each device is — a light, a blind, a speaker.
        // Added after `device_name` shipped, same ignore-the-error reason as
        // above. Empty for a row written before this column existed, and for
        // any server that does not say; readers treat empty as "unknown" and
        // simply fall back to the full device list.
        let _ = self
            .conn
            .execute("ALTER TABLE device_name ADD COLUMN domain TEXT NOT NULL DEFAULT ''", []);
        // What has actually run, as opposed to what was merely offered. Added
        // after the table shipped; same ignore-the-error reason as above.
        // Deliberately five columns on the existing row rather than a history
        // table: a log of every command anyone ever spoke is a standing
        // privacy cost, and the questions being answered ("has this ever
        // worked, when, for whom, how slow") need only the latest.
        for col in [
            "runs INTEGER NOT NULL DEFAULT 0",
            "last_run INTEGER",
            "last_outcome TEXT",
            "last_ms INTEGER",
            "last_speaker TEXT",
            "last_think_ms INTEGER",
        ] {
            let _ = self.conn.execute(&format!("ALTER TABLE tool ADD COLUMN {col}"), []);
        }
        // The same three questions per *device*, which is the unit a person
        // actually thinks in: "the office lamp never works" is a sentence
        // people say, "HassTurnOn never works" is not. Same
        // ignore-the-error reason as above, and the same deliberate absence of
        // a history table.
        for col in ["runs INTEGER NOT NULL DEFAULT 0", "last_run INTEGER", "last_ok INTEGER"] {
            let _ = self.conn.execute(&format!("ALTER TABLE device_name ADD COLUMN {col}"), []);
        }
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
            self.conn.execute("DELETE FROM shortcut", [])?;
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
        // The phrases that replayed those tools go with them: a phrase pointing
        // at a server the user has removed can never fire again, and leaving it
        // on the page as permanently paused would be a bug rather than caution.
        self.conn.execute(
            &format!("DELETE FROM shortcut WHERE source NOT IN ({placeholders})"),
            args.as_slice(),
        )?;
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
    /// from the old list, while a refresh that finds the same areas must not
    /// pay to rebuild it.
    pub fn set_place_names(&self, source: &str, names: &[String]) -> Result<bool> {
        self.replace_names("place_name", source, names)
    }

    /// Every place name across every source, sorted and de-duplicated.
    ///
    /// Sorted because these go into the system prompt, and a prompt whose
    /// bytes shift between turns cannot be a cached prefix.
    pub fn place_names(&self) -> Result<Vec<String>> {
        self.place_names_where(None)
    }

    /// The place names one server reported.
    ///
    /// Two servers can each have a home, and then the merged list of the one
    /// above is nobody's: it offers server A's areas while acting on server B.
    /// Anything holding a model to an area name wants this one.
    pub fn place_names_of(&self, source: &str) -> Result<Vec<String>> {
        self.place_names_where(Some(source))
    }

    fn place_names_where(&self, source: Option<&str>) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT p.name FROM place_name p JOIN tool_source s ON s.id = p.source_id
              WHERE ?1 IS NULL OR s.name = ?1
              ORDER BY p.name COLLATE NOCASE",
        )?;
        let names = stmt
            .query_map(params![source], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
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
    ///
    /// Each device carries the kind of thing it is, which is what lets Fono
    /// offer the model the list of kinds actually present in this home rather
    /// than every kind that could exist. A server that does not say leaves it
    /// empty, and everything still works — see [`Device`].
    pub fn set_devices(&self, source: &str, devices: &[Device]) -> Result<bool> {
        let id = self.source_id(source, "sse")?;
        let mut stmt =
            self.conn.prepare("SELECT name, domain FROM device_name WHERE source_id = ?1")?;
        let before: std::collections::BTreeSet<(String, String)> = stmt
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        let after: std::collections::BTreeSet<(String, String)> = devices
            .iter()
            .map(|d| (d.name.trim().to_owned(), d.domain.trim().to_owned()))
            .filter(|(n, _)| !n.is_empty())
            .collect();

        // Same reading as `replace_names`: a home that answers but lists
        // nothing has not finished waking up, so keep what we knew.
        if after.is_empty() && !before.is_empty() {
            return Ok(false);
        }

        // Updated in place, with only the names that actually vanished
        // removed — deliberately not the delete-everything-and-reinsert this
        // used to be. Each row now carries how many times that device has been
        // operated, and discovery runs on every reconnect: wiping and rewriting
        // the table would reset every device's history several times a day,
        // which is the same as not keeping one.
        for (name, domain) in &after {
            self.conn.execute(
                "INSERT INTO device_name(source_id, name, domain) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_id, name) DO UPDATE SET domain = excluded.domain",
                params![id, name, domain],
            )?;
        }
        let kept: std::collections::BTreeSet<&str> =
            after.iter().map(|(n, _)| n.as_str()).collect();
        for (name, _) in &before {
            if !kept.contains(name.as_str()) {
                self.conn.execute(
                    "DELETE FROM device_name WHERE source_id = ?1 AND name = ?2",
                    params![id, name],
                )?;
            }
        }
        Ok(before != after)
    }

    /// The kinds of device this home actually contains — `light`, `cover`,
    /// `media_player` and so on — sorted and de-duplicated.
    ///
    /// Devices whose kind was never recorded contribute nothing, so an older
    /// catalogue simply reports an empty list instead of a wrong one.
    pub fn device_domains(&self) -> Result<Vec<String>> {
        self.device_domains_where(None)
    }

    /// The kinds of device one server reported, for the same reason as
    /// [`Self::place_names_of`]: a kind is only a real answer for the home that
    /// has one.
    pub fn device_domains_of(&self, source: &str) -> Result<Vec<String>> {
        self.device_domains_where(Some(source))
    }

    fn device_domains_where(&self, source: Option<&str>) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT d.domain FROM device_name d JOIN tool_source s ON s.id = d.source_id
              WHERE d.domain <> '' AND (?1 IS NULL OR s.name = ?1)
              ORDER BY d.domain",
        )?;
        let names = stmt
            .query_map(params![source], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    /// Swap one source's rows in a name table, reporting whether anything moved.
    ///
    /// Compared as a set after trimming, because that is exactly how the names
    /// reach the prompt: de-duplicated and sorted by the readers below. Two
    /// refreshes that find the same areas in a different order have changed
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

    /// Every device, with the kind of thing it is, across every source.
    ///
    /// [`Self::device_names`] is what the prompt needs; this is what a person
    /// needs. Reading a list of names alone, a lamp filed under the wrong kind
    /// looks exactly like a lamp filed correctly — and the kind is what
    /// decides whether an area-wide command reaches it.
    pub fn devices(&self) -> Result<Vec<Device>> {
        // Grouped rather than `DISTINCT` because two servers can offer the same
        // device, and the reader wants one row saying it has worked nine times —
        // not two rows saying four and five. `MAX(last_run)` with a bare
        // `last_ok` is SQLite's documented pick-from-the-max-row behaviour, so
        // the outcome shown always belongs to the run whose time is shown.
        let mut stmt = self.conn.prepare(
            "SELECT name, domain, SUM(COALESCE(runs, 0)), MAX(last_run), last_ok
             FROM device_name GROUP BY name, domain ORDER BY domain, name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Device {
                    // The name a command may use, never the joined alias list.
                    name: primary_name(&r.get::<_, String>(0)?).to_owned(),
                    domain: r.get(1)?,
                    runs: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    last_run: r.get(3)?,
                    last_ok: r.get::<_, Option<i64>>(4)?.map(|v| v != 0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Note that a command reached one thing in the home, and whether it landed.
    ///
    /// See [`primary_name`]: the stored name may carry aliases, so the target is
    /// matched against every one of them.
    ///
    /// The name is whatever the server called it, so it is matched against the
    /// devices already learned from that server — exactly, then against each
    /// comma-separated alias, then ignoring case. An unmatched name is dropped:
    /// a reply is evidence about the home, not a source of new devices, and
    /// inventing a row from one would fill the list with things that do not
    /// exist. Returns whether a device was found, for tests and traces.
    pub fn record_device_run(&self, source: &str, target: &str, landed: bool) -> Result<bool> {
        let target = target.trim();
        if target.is_empty() {
            return Ok(false);
        }
        let mut stmt = self.conn.prepare(
            "SELECT d.name FROM device_name d JOIN tool_source s ON s.id = d.source_id
             WHERE s.name = ?1",
        )?;
        let known = stmt
            .query_map(params![source], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let matches =
            |stored: &str| stored.split(',').any(|alias| alias.trim().eq_ignore_ascii_case(target));
        let Some(stored) = known.iter().find(|n| matches(n)) else { return Ok(false) };

        self.conn.execute(
            "UPDATE device_name
                SET runs = COALESCE(runs, 0) + 1, last_run = ?1, last_ok = ?2
              WHERE name = ?3 AND source_id = (SELECT id FROM tool_source WHERE name = ?4)",
            params![now_unix(), i64::from(landed), stored, source],
        )?;
        Ok(true)
    }

    /// Every server the catalogue holds tools from, and when it last answered.
    ///
    /// The config is the authority on which servers *exist*; this says which
    /// ones have ever actually replied, which is a different question and the
    /// one someone asks when a command stops working.
    pub fn sources(&self) -> Result<Vec<SourceRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, transport, last_seen FROM tool_source ORDER BY name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SourceRow { name: r.get(0)?, transport: r.get(1)?, last_seen: r.get(2)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every device name across every source, sorted and de-duplicated.
    ///
    /// Sorted for the same reason as the places: these become part of a
    /// system prompt we want to stay byte-stable between turns.
    /// Only the leading name of each device reaches this list — see
    /// [`primary_name`] for why the rest are kept but never offered.
    pub fn device_names(&self) -> Result<Vec<String>> {
        self.device_names_where(None)
    }

    /// The devices one server reported.
    ///
    /// The list that holds a model to a real device has to be this one. `name`
    /// is the commonest parameter name there is, so a merged list narrows some
    /// other server's `name` field to a house it knows nothing about — and then
    /// no legal value exists at all, which is worse than not narrowing it.
    pub fn device_names_of(&self, source: &str) -> Result<Vec<String>> {
        self.device_names_where(Some(source))
    }

    fn device_names_where(&self, source: Option<&str>) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT d.name FROM device_name d JOIN tool_source s ON s.id = d.source_id
              WHERE ?1 IS NULL OR s.name = ?1
              ORDER BY d.name COLLATE NOCASE",
        )?;
        let mut names: Vec<String> = stmt
            .query_map(params![source], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?
            .iter()
            .map(|n| primary_name(n).to_owned())
            .filter(|n| !n.is_empty())
            .collect();
        // Two devices can share a leading name once the aliases are dropped,
        // and the same name twice in a prompt teaches nothing.
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
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
                    t.readback_tool, t.available, t.enabled, t.user_touched, t.description,
                    t.runs, t.last_run, t.last_outcome, t.last_ms, t.last_speaker,
                    t.last_think_ms
             FROM tool t JOIN tool_source s ON s.id = t.source_id
             {filter}
             ORDER BY s.name, t.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |r| {
                let schema_json: String = r.get(2)?;
                // A run is only reported when both the time and the outcome
                // are there. Half a record would be shown as a run that
                // happened and cannot be described, which reads as a bug in
                // the page rather than in the data.
                let at: Option<i64> = r.get(12)?;
                let outcome: Option<String> = r.get(13)?;
                let last_run = match (at, outcome.as_deref().and_then(RunOutcome::parse)) {
                    (Some(at), Some(outcome)) => Some(LastRun {
                        at,
                        outcome,
                        ms: r.get::<_, Option<i64>>(14)?.unwrap_or(0),
                        speaker: r.get::<_, Option<String>>(15)?.filter(|s| !s.is_empty()),
                        think_ms: r.get::<_, Option<i64>>(16)?.filter(|&v| v > 0),
                    }),
                    _ => None,
                };
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
                    runs: r.get::<_, Option<i64>>(11)?.unwrap_or(0),
                    last_run,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Note that a tool ran, and how it went.
    ///
    /// Called once per executed call. Overwrites the previous run and bumps
    /// the counter; nothing accumulates, so this cannot grow the database with
    /// use. Unknown `(source, name)` pairs are ignored rather than inserted —
    /// a run belongs to a discovered tool, and inventing a catalogue row from
    /// a call would let a typo masquerade as something the server offers.
    ///
    /// `speaker` is the enrolled name Fono recognised, when it recognised
    /// one; `None` records that nobody was identified.
    pub fn record_run(
        &self,
        source: &str,
        name: &str,
        outcome: RunOutcome,
        ms: i64,
        think_ms: Option<i64>,
        speaker: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tool
                SET runs = COALESCE(runs, 0) + 1,
                    last_run = ?1, last_outcome = ?2, last_ms = ?3, last_speaker = ?4,
                    last_think_ms = ?5
              WHERE name = ?6
                AND source_id = (SELECT id FROM tool_source WHERE name = ?7)",
            params![now_unix(), outcome.as_str(), ms, speaker, think_ms, name, source],
        )?;
        Ok(())
    }

    /// Judge every run that has been waiting for its window to close.
    ///
    /// Call this once per turn that could act, *before* [`Self::remember`],
    /// passing what was said and which devices this turn reached.
    ///
    /// The rule it applies is one sentence: **a run is clean if the reply
    /// reported no error and the user did not come back about the same thing
    /// within [`COMPLAINT_WINDOW_SECS`]**. "Came back about the same thing" is
    /// deliberately the *device*, not the words: a word list for *no* / *not
    /// that one* / *undo* needs an entry per language and Fono is spoken in
    /// several, while a device is the same device in every language. Where the
    /// server does not name what it touched, the phrase itself is the only
    /// handle and repeating it inside the window is the complaint.
    ///
    /// Both error directions are cheap, which is what makes the signal safe to
    /// use at all: a missed complaint delays a promotion, and a false complaint
    /// keeps a phrase slow. Neither moves a device.
    ///
    /// Judging here rather than on a timer means it costs nothing: a run is
    /// always scored after its window has closed, and a phrase nobody ever says
    /// again is never promoted — which is correct.
    pub fn settle(&self, said: &str, devices: &[String]) -> Result<()> {
        // A window that has closed is settled whoever asks, so that half of the
        // rule does not wait for a turn to be about the same phrase.
        self.close_windows()?;
        let said = normalise_phrase(said);
        let touched: Vec<String> =
            devices.iter().map(|d| d.trim().to_lowercase()).filter(|d| !d.is_empty()).collect();
        let mut stmt = self.conn.prepare(
            "SELECT id, phrase_norm, pending_devices FROM shortcut WHERE pending_at IS NOT NULL",
        )?;
        let waiting = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        // Everything still waiting is inside its window, so this turn can only
        // be a complaint about it. Age was ruled out above, and that ordering is
        // the whole point of the window: the same command an hour later is a new
        // request, not a complaint about the old one.
        for (id, phrase, devs) in waiting {
            let complained = if devs.is_empty() {
                phrase == said
            } else {
                devs.split('\n').any(|d| touched.iter().any(|t| t == d))
            };
            if complained {
                self.spoiled(id)?;
            }
        }
        Ok(())
    }

    /// Count every waiting run whose complaint window has closed.
    ///
    /// Read paths call this too, and that is the point. A run earns its place by
    /// **the window closing with no complaint**, which is true the moment the
    /// clock passes it — not the next time a turn happens to ask. Scoring it
    /// only inside a turn cost a whole extra utterance: the run that had just
    /// earned its window was counted a turn late, so the third time a phrase was
    /// said it was still slow when it should already have been fast.
    ///
    /// Nothing here can promote a phrase the rule would not: a run inside its
    /// window is untouched, and a complaint still resets the count to zero.
    fn close_windows(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE shortcut SET clean = clean + 1, pending_at = NULL, pending_devices = ''
              WHERE pending_at IS NOT NULL AND ?1 - pending_at > ?2",
            params![now_unix(), COMPLAINT_WINDOW_SECS],
        )?;
        Ok(())
    }

    /// The waiting run of one phrase was a mistake: forget its progress.
    fn spoiled(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE shortcut SET clean = 0, pending_at = NULL, pending_devices = '' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Write down that a phrase produced a command.
    ///
    /// Returns whether anything was recorded. Three commands are refused
    /// outright: one whose tool the catalogue does not offer, one whose name
    /// marks it [`Capability::Dangerous`] — an unlock or a purchase is never
    /// replayed from a phrase, however often it has worked — and one that asks
    /// for an *amount* rather than naming a thing, which would double when
    /// replayed ([`names_a_thing`]).
    ///
    /// A successful run becomes the phrase's one *pending* run, judged later by
    /// [`Self::settle`]. A failed one is a dirty run at once and resets the
    /// count: one bad run makes a phrase slow again.
    ///
    /// A phrase that now produces a *different* command starts earning from
    /// zero. Two clean runs promote a phrase together with the command it made,
    /// not the phrase alone.
    pub fn remember(&self, said: &Said<'_>) -> Result<bool> {
        let norm = normalise_phrase(said.phrase);
        if norm.is_empty() || !names_a_thing(said.args) {
            return Ok(false);
        }
        let found: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT t.schema_hash, t.capability FROM tool t
                 JOIN tool_source s ON s.id = t.source_id
                 WHERE s.name = ?1 AND t.name = ?2",
                params![said.source, said.tool],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // A run belongs to a tool the catalogue knows about; there is nothing to
        // key a replay to otherwise.
        let Some((hash, capability)) = found else { return Ok(false) };
        if Capability::parse(&capability) == Capability::Dangerous {
            return Ok(false);
        }

        let args = stable_args(said.args);
        let now = now_unix();
        let devices: Vec<String> = said
            .devices
            .iter()
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        let pending = said.ok.then(|| devices.join("\n"));

        let existing: Option<(i64, String, String, String)> = self
            .conn
            .query_row(
                "SELECT id, tool, args_json, schema_hash FROM shortcut WHERE phrase_norm = ?1",
                params![norm],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        match existing {
            None => {
                if !said.ok {
                    // Nothing to learn from a command that did not work, and a
                    // row for it would only ever read as a failure.
                    return Ok(false);
                }
                self.conn.execute(
                    "INSERT INTO shortcut(phrase_norm, phrase_raw, lang, source, tool, args_json,
                                          schema_hash, origin, runs, clean, last_run, last_ok,
                                          last_ms, pending_at, pending_devices)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 0, ?9, 1, ?10, ?9, ?11)",
                    params![
                        norm,
                        said.phrase.trim(),
                        said.lang,
                        said.source,
                        said.tool,
                        args,
                        hash,
                        Origin::Learned.as_str(),
                        now,
                        said.ms,
                        pending.unwrap_or_default(),
                    ],
                )?;
            }
            Some((id, tool, stored_args, stored_hash)) => {
                // Case-folded, so two utterances that named the same lamp with
                // a different capital letter are one command rather than two.
                let same = tool == said.tool
                    && stored_args.eq_ignore_ascii_case(&args)
                    && stored_hash == hash;
                let clean_now = if said.ok && same { None } else { Some(0_i64) };
                self.conn.execute(
                    "UPDATE shortcut
                        SET tool = ?2, args_json = ?3, schema_hash = ?4, runs = runs + 1,
                            clean = COALESCE(?5, clean), last_run = ?6, last_ok = ?7,
                            last_ms = ?8, pending_at = ?9, pending_devices = ?10
                      WHERE id = ?1",
                    params![
                        id,
                        said.tool,
                        args,
                        hash,
                        clean_now,
                        now,
                        i64::from(said.ok),
                        said.ms,
                        said.ok.then_some(now),
                        pending.unwrap_or_default(),
                    ],
                )?;
            }
        }
        Ok(true)
    }

    /// The command a phrase has earned the right to run before the model is
    /// asked, if it has earned it.
    ///
    /// `None` is the ordinary answer and costs one indexed lookup.
    pub fn replay(&self, said: &str) -> Result<Option<Shortcut>> {
        let norm = normalise_phrase(said);
        if norm.is_empty() {
            return Ok(None);
        }
        let row = self.select_shortcuts(Some(&norm))?.into_iter().next();
        Ok(row.filter(Shortcut::fast))
    }

    /// Every phrase Fono has written down, newest use first.
    pub fn shortcuts(&self) -> Result<Vec<Shortcut>> {
        self.select_shortcuts(None)
    }

    fn select_shortcuts(&self, phrase_norm: Option<&str>) -> Result<Vec<Shortcut>> {
        // A read scores what the clock has already decided, so the fast path and
        // the page can never disagree about whether a phrase has earned it —
        // and neither of them waits for a turn to notice. See `close_windows`.
        self.close_windows()?;
        // Left joins throughout: a phrase whose server has gone quiet still has
        // a row to show, and saying so is the point of the page.
        let filter = if phrase_norm.is_some() { "WHERE s.phrase_norm = ?1" } else { "" };
        let sql = format!(
            "SELECT s.phrase_raw, s.lang, s.source, s.tool, s.args_json, s.origin, s.runs,
                    s.clean, s.last_run, s.last_ok, s.last_ms, s.schema_hash,
                    t.schema_hash, t.available, t.enabled
             FROM shortcut s
             LEFT JOIN tool_source src ON src.name = s.source
             LEFT JOIN tool t ON t.source_id = src.id AND t.name = s.tool
             {filter}
             ORDER BY s.last_run DESC, s.phrase_norm"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| {
            let learned_hash: String = r.get(11)?;
            let live_hash: Option<String> = r.get(12)?;
            let available: Option<i64> = r.get(13)?;
            let enabled: Option<i64> = r.get(14)?;
            let offered = available.unwrap_or(0) != 0 && enabled.unwrap_or(0) != 0;
            let stale = if !offered {
                Some(Stale::Paused)
            } else if live_hash.as_deref() != Some(learned_hash.as_str()) {
                Some(Stale::Changed)
            } else {
                None
            };
            Ok(Shortcut {
                phrase: r.get(0)?,
                lang: r.get(1)?,
                source: r.get(2)?,
                tool: r.get(3)?,
                args: r.get(4)?,
                origin: Origin::parse(&r.get::<_, String>(5)?),
                runs: r.get(6)?,
                clean: r.get(7)?,
                last_run: r.get(8)?,
                last_ok: r.get::<_, Option<i64>>(9)?.map(|v| v != 0),
                last_ms: r.get(10)?,
                stale,
            })
        };
        let rows = match phrase_norm {
            Some(p) => stmt.query_map(params![p], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// Add another way of saying something Fono already knows.
    ///
    /// The new phrase points at the same command and inherits its standing: an
    /// extra wording for a phrase already on the fast path runs at once, and one
    /// for a phrase still learning still has to earn it. What the gate protects
    /// is the *command* — that this tool with these arguments is the right thing
    /// to do — and that has already been won twice over. Only the wording is
    /// new, the user typed it deliberately, and a wording that turns out to be
    /// wrong is one click to forget.
    ///
    /// A command still cannot be typed in from nothing: this copies an existing
    /// row or it fails.
    pub fn add_phrase(&self, like: &str, phrase: &str) -> Result<()> {
        let from = normalise_phrase(like);
        let norm = normalise_phrase(phrase);
        if norm.is_empty() {
            return Err(Error::Other("a phrase cannot be blank".into()));
        }
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO shortcut(phrase_norm, phrase_raw, lang, source, tool,
                                            args_json, schema_hash, origin, clean)
             SELECT ?2, ?3, lang, source, tool, args_json, schema_hash, ?4, clean
               FROM shortcut WHERE phrase_norm = ?1",
            params![from, norm, phrase.trim(), Origin::Written.as_str()],
        )?;
        if changed == 0 {
            return Err(Error::Other(format!("nothing is known about {like:?}")));
        }
        Ok(())
    }

    /// Forget one phrase. Deliberately a delete: the user saying they do not
    /// want it is not the same as the world temporarily changing underneath it.
    pub fn forget_phrase(&self, phrase: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM shortcut WHERE phrase_norm = ?1",
            params![normalise_phrase(phrase)],
        )?;
        Ok(())
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

/// The arguments of one command, in a form two runs of it can be compared in.
///
/// Only the key order is settled; every value is passed through exactly as it
/// was sent, because this same text is what a replay sends. Two commands that
/// differ only in the order the model happened to write the fields in are the
/// same command, and counting them as different would stop a phrase ever
/// earning its second clean run.
fn stable_args(args: &str) -> String {
    // Something that is not JSON at all is kept verbatim rather than dropped:
    // comparing it as text is still correct, and a server that takes something
    // else is not ours to second-guess.
    serde_json::from_str::<serde_json::Value>(args)
        .map_or_else(|_| args.trim().to_owned(), |v| canonical_json(&v))
}

/// Does this command *name a thing*, rather than *ask for an amount*?
///
/// Only the first kind may be replayed. "Turn on the hall lamp" names a state
/// the world should end in, so running it twice is running it once; "two
/// degrees warmer" names a change, and running it twice is four degrees. A
/// shortcut is by definition run again, so the second kind must never become
/// one.
///
/// The test is the *shape of the values*, not the name of the tool: a name is
/// text, an amount is a number. Nothing here knows a single tool name, so a
/// server Fono has never seen is judged by the same rule as a familiar one,
/// and a server that adds a tool next year needs no change. A tool-name table
/// would have been the other way to write this, and it would have to be
/// corrected at every release of every server.
///
/// Conservative in the direction that costs least. An absolute amount — "set
/// the brightness to forty" — is safe to replay and is refused anyway, so it
/// stays slow. The plan's asymmetry applies: a promotion that does not happen
/// costs a couple of seconds once, and a wrong replay moves something in the
/// physical world.
///
/// Text in a list or an object still counts, because one command may name
/// several devices. Blank values are already gone by this point, stripped
/// before the call was sent.
fn names_a_thing(args: &str) -> bool {
    fn all_text(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(_) | serde_json::Value::Null => true,
            serde_json::Value::Array(a) => a.iter().all(all_text),
            serde_json::Value::Object(o) => o.values().all(all_text),
            // A number or a boolean. Neither is the name of anything.
            _ => false,
        }
    }
    // Something that is not JSON says nothing about its own shape, so it is
    // refused for the same reason an amount is: this is the safe direction to
    // be wrong in.
    serde_json::from_str::<serde_json::Value>(args).is_ok_and(|v| all_text(&v))
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

    /// Devices with no particular kind — for the tests that only care about
    /// names. Kind-aware tests build [`Device`] values directly.
    fn devs(v: &[&str]) -> Vec<Device> {
        v.iter().map(|n| Device::new(*n, "")).collect()
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

    /// Area names are learned once and read on every turn, so two things
    /// matter: refreshing must not accumulate stale areas, and removing a
    /// server must not leave the model being told about areas that are gone.
    #[test]
    fn area_names_are_replaced_on_refresh_and_forgotten_with_the_server() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        db.set_place_names("ha", &names(&["Kitchen", "Office"])).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Kitchen", "Office"]);

        // A rename upstream must not leave the old name behind: the point of
        // the list is that every name in it is real.
        db.set_place_names("ha", &names(&["Kitchen", "Study"])).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Kitchen", "Study"]);

        // Two servers pool their areas, sorted, so the prompt bytes do not
        // depend on which one was connected first.
        db.reconcile("cabin", "sse", &[light_on()]).unwrap();
        db.set_place_names("cabin", &names(&["Attic"])).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Attic", "Kitchen", "Study"]);

        db.forget_sources_except(&["cabin".to_string()]).unwrap();
        assert_eq!(db.place_names().unwrap(), vec!["Attic"]);
    }

    /// Device names follow the same rules as area names, for the same reason:
    /// every name we hand the model has to be one the home will still match.
    #[test]
    fn device_names_are_replaced_on_refresh_and_forgotten_with_the_server() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();

        db.set_devices("ha", &devs(&["Office outdoor light", "Kitchen lights"])).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Kitchen lights", "Office outdoor light"]);

        // A lamp that was renamed or unexposed must drop out, or we would keep
        // offering the model a name that no longer resolves.
        db.set_devices("ha", &devs(&["Kitchen lights"])).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Kitchen lights"]);

        db.reconcile("cabin", "sse", &[light_on()]).unwrap();
        db.set_devices("cabin", &devs(&["Attic lamp"])).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Attic lamp", "Kitchen lights"]);

        db.forget_sources_except(&["cabin".to_string()]).unwrap();
        assert_eq!(db.device_names().unwrap(), vec!["Attic lamp"]);
    }

    /// A device with a second-language name is one device, and the name the
    /// model is offered has to be one the home will match. `Office display,
    /// Boxa birou` is one speaker in the home this was built against; offered
    /// whole, the command is refused outright and the model takes the blame.
    /// Either name still finds it — that half is
    /// `a_device_remembers_how_often_it_worked_across_a_refresh`.
    #[test]
    fn a_device_with_two_names_is_offered_under_the_first_of_them() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let home = [
            Device::new("Office display, Boxa birou", "media_player"),
            Device::new("Bec", "light"),
        ];
        db.set_devices("ha", &home).unwrap();

        assert_eq!(db.device_names().unwrap(), vec!["Bec", "Office display"]);
        let listed: Vec<String> = db.devices().unwrap().into_iter().map(|d| d.name).collect();
        assert_eq!(listed, vec!["Bec", "Office display"]);
    }

    /// Two servers, two houses, and each reader has to be able to answer for
    /// one of them.
    ///
    /// The merged answer is nobody's: it offers the cabin's areas while acting
    /// on the flat. Anything that holds a model to a real area, device or kind
    /// asks per server, so these are the readers that matter — the merged ones
    /// stay for the pages that genuinely mean "everything Fono knows".
    #[test]
    fn each_server_can_be_asked_about_its_own_house() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        for s in ["flat", "cabin"] {
            db.reconcile(s, "sse", &[light_on()]).unwrap();
        }
        db.set_place_names("flat", &names(&["Kitchen"])).unwrap();
        db.set_place_names("cabin", &names(&["Attic"])).unwrap();
        db.set_devices("flat", &[Device::new("Hall lamp", "light")]).unwrap();
        db.set_devices("cabin", &[Device::new("Attic blind", "cover")]).unwrap();

        assert_eq!(db.place_names_of("flat").unwrap(), vec!["Kitchen"]);
        assert_eq!(db.place_names_of("cabin").unwrap(), vec!["Attic"]);
        assert_eq!(db.device_names_of("flat").unwrap(), vec!["Hall lamp"]);
        assert_eq!(db.device_names_of("cabin").unwrap(), vec!["Attic blind"]);
        assert_eq!(db.device_domains_of("flat").unwrap(), vec!["light"]);
        assert_eq!(db.device_domains_of("cabin").unwrap(), vec!["cover"]);

        // A server nobody has told us anything about answers nothing, rather
        // than answering with somebody else's home.
        assert!(db.place_names_of("shell").unwrap().is_empty());
        assert!(db.device_names_of("shell").unwrap().is_empty());

        // And the merged readers still merge.
        assert_eq!(db.place_names().unwrap(), vec!["Attic", "Kitchen"]);
        assert_eq!(db.device_names().unwrap(), vec!["Attic blind", "Hall lamp"]);
        assert_eq!(db.device_domains().unwrap(), vec!["cover", "light"]);
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
        assert!(db.set_devices("ha", &devs(&["Kitchen lights"])).unwrap());
        assert!(db.set_place_names("ha", &names(&["Kitchen"])).unwrap());

        // Finding the same house again is not.
        assert!(!db.set_devices("ha", &devs(&["Kitchen lights"])).unwrap());
        assert!(!db.set_place_names("ha", &names(&["Kitchen"])).unwrap());

        // Order and stray spacing are not changes: the names are sorted and
        // trimmed before they ever reach the model, so the prompt is identical.
        assert!(!db.set_devices("ha", &devs(&["  Kitchen lights  ", ""])).unwrap());

        // A rename is the case that matters — this is the one that used to
        // leave fono insisting a lamp the user is looking at does not exist.
        assert!(db.set_devices("ha", &devs(&["Lampa de sare"])).unwrap());
        assert_eq!(db.device_names().unwrap(), vec!["Lampa de sare"]);

        // And an addition.
        assert!(db.set_devices("ha", &devs(&["Lampa de sare", "Hallway AC"])).unwrap());
    }

    /// A home that is still waking up answers with an empty list. Startup is
    /// exactly when that happens, and wiping the catalogue would leave the
    /// assistant telling the user their house has no devices in it.
    #[test]
    fn an_empty_answer_never_wipes_a_known_house() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        db.set_devices("ha", &devs(&["Kitchen lights"])).unwrap();
        db.set_place_names("ha", &names(&["Kitchen"])).unwrap();

        assert!(!db.set_devices("ha", &[]).unwrap());
        assert!(!db.set_place_names("ha", &[]).unwrap());
        assert_eq!(db.device_names().unwrap(), vec!["Kitchen lights"]);
        assert_eq!(db.place_names().unwrap(), vec!["Kitchen"]);

        // Deliberately removing a server still forgets it — that path is
        // `forget_sources_except`, and it is not weakened by the guard above.
        db.forget_sources_except(&[]).unwrap();
        assert!(db.device_names().unwrap().is_empty());
    }

    /// The kind of each device is what lets Fono offer the model only the
    /// kinds this home actually has. It has to survive a refresh, disappear
    /// with its server, and stay absent rather than wrong when the server
    /// never said what kind a thing was.
    #[test]
    fn device_kinds_are_learned_and_forgotten_with_their_server() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();

        db.set_devices(
            "ha",
            &[
                Device::new("Kitchen lights", "light"),
                Device::new("Office outdoor light", "light"),
                Device::new("Living room blind", "cover"),
            ],
        )
        .unwrap();
        assert_eq!(db.device_domains().unwrap(), vec!["cover", "light"]);

        // Losing the only blind loses the kind too, so the model is never
        // offered a kind the home no longer contains.
        db.set_devices("ha", &[Device::new("Kitchen lights", "light")]).unwrap();
        assert_eq!(db.device_domains().unwrap(), vec!["light"]);

        // A server that does not say leaves the kind unknown. That must read
        // as "no kinds" rather than inventing one.
        db.set_devices("ha", &devs(&["Mystery gadget"])).unwrap();
        assert!(db.device_domains().unwrap().is_empty());
        assert_eq!(db.device_names().unwrap(), vec!["Mystery gadget"]);

        // Changing only the kind of a device is still a change: it moves what
        // the model is allowed to say, so a warm prompt built on the old list
        // is stale.
        assert!(db.set_devices("ha", &[Device::new("Mystery gadget", "fan")]).unwrap());
        assert!(!db.set_devices("ha", &[Device::new("Mystery gadget", "fan")]).unwrap());

        db.forget_sources_except(&[]).unwrap();
        assert!(db.device_domains().unwrap().is_empty());
    }

    /// What a person reading the page needs, which is not what the prompt
    /// needs: the kind beside every device, and which servers have replied.
    #[test]
    fn the_page_can_see_the_house_and_who_reported_it() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        db.set_devices(
            "ha",
            &[
                Device::new("Living room blind", "cover"),
                Device::new("Kitchen lights", "light"),
                Device::new("Mystery gadget", ""),
            ],
        )
        .unwrap();

        // Grouped by kind, so a device filed under the wrong one stands out —
        // and an unreported kind reads as blank rather than as a guess.
        assert_eq!(
            db.devices().unwrap(),
            vec![
                Device::new("Mystery gadget", ""),
                Device::new("Living room blind", "cover"),
                Device::new("Kitchen lights", "light"),
            ]
        );

        let sources = db.sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "ha");
        assert_eq!(sources[0].transport, "sse");
        assert!(sources[0].last_seen.unwrap_or(0) > 0, "a server that answered has a time");

        // Removing the server takes both readings with it: the page must never
        // show a house belonging to a server the user deleted.
        db.forget_sources_except(&[]).unwrap();
        assert!(db.devices().unwrap().is_empty());
        assert!(db.sources().unwrap().is_empty());
    }

    /// "The office lamp never works" is a sentence people actually say, so the
    /// count has to live on the device and survive the thing that happens most
    /// often to it: another discovery pass.
    #[test]
    fn a_device_remembers_how_often_it_worked_across_a_refresh() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();
        let home = [
            Device::new("Office display, Boxa birou", "media_player"),
            Device::new("Hall lamp", "light"),
        ];
        db.set_devices("ha", &home).unwrap();

        assert!(db.record_device_run("ha", "Hall lamp", true).unwrap());
        assert!(db.record_device_run("ha", "Hall lamp", false).unwrap());
        // Home Assistant answers with whichever alias matched, and case is not
        // reliable either — both have to find the one stored row.
        assert!(db.record_device_run("ha", "boxa birou", true).unwrap());
        // A name that is not in this home is dropped rather than added: a reply
        // must not be able to invent a device.
        assert!(!db.record_device_run("ha", "Ghost lamp", true).unwrap());
        assert!(!db.record_device_run("ha", "  ", true).unwrap());

        let by_name =
            |name: &str| db.devices().unwrap().into_iter().find(|d| d.name == name).unwrap();
        let lamp = by_name("Hall lamp");
        assert_eq!(lamp.runs, 2);
        assert_eq!(lamp.last_ok, Some(false), "the latest attempt is the one shown");
        assert!(lamp.last_run.unwrap_or(0) > 0);
        // Listed under its leading name, though the alias is what matched.
        assert_eq!(by_name("Office display").runs, 1);
        assert_eq!(db.devices().unwrap().iter().filter(|d| d.runs == 0).count(), 0);

        // The load-bearing part. Discovery runs on every reconnect, several
        // times a day; a history that resets then is a history nobody can use.
        db.set_devices("ha", &home).unwrap();
        assert_eq!(by_name("Hall lamp").runs, 2, "a refresh must not erase the count");
        assert_eq!(by_name("Hall lamp").last_ok, Some(false));

        // A device that genuinely leaves the home does go, along with its
        // history — it is no longer something the page can honestly list.
        db.set_devices("ha", &[Device::new("Hall lamp", "light")]).unwrap();
        assert_eq!(db.devices().unwrap().len(), 1);
        assert_eq!(by_name("Hall lamp").runs, 2);
    }

    /// Both clocks, not one. Reporting only the round trip to the server makes
    /// Fono look several times faster than it feels, and hides the half of the
    /// wait — the assistant choosing a command — that is usually the reason
    /// anyone opened this page.
    #[test]
    fn a_run_records_how_long_the_thinking_took_as_well_as_the_call() {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on()]).unwrap();

        db.record_run("ha", "HassTurnOn", RunOutcome::Confirmed, 401, Some(2870), Some("Bogdan"))
            .unwrap();
        let row = db.active_tools().unwrap().pop().unwrap();
        let last = row.last_run.clone().expect("the run is remembered");
        assert_eq!(last.ms, 401, "the call itself");
        assert_eq!(last.think_ms, Some(2870), "and what it cost to decide on it");
        assert_eq!(last.speaker.as_deref(), Some("Bogdan"));
        assert_eq!(row.runs, 1);

        // A run recorded before the second clock existed has no figure for it,
        // and a page printing "0 ms to decide" would be stating something
        // untrue rather than admitting it does not know.
        db.conn.execute("UPDATE tool SET last_think_ms = NULL", []).unwrap();
        assert_eq!(db.active_tools().unwrap()[0].last_run.as_ref().unwrap().think_ms, None);
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

    // ---- phrases that have worked before ----

    const HALL: &str = r#"{"name":"Hall lamp"}"#;

    fn house() -> ToolCatalogStore {
        let db = ToolCatalogStore::open_in_memory().unwrap();
        db.reconcile("ha", "sse", &[light_on(), light_off()]).unwrap();
        db
    }

    fn one(phrase: &str, args: &str, devices: &[String]) -> Said<'static> {
        // Leaked so the fixture reads as one line at each call site; a handful
        // of short strings in a test binary.
        Said {
            phrase: Box::leak(phrase.to_owned().into_boxed_str()),
            lang: "en",
            source: "ha",
            tool: "HassTurnOn",
            args: Box::leak(args.to_owned().into_boxed_str()),
            devices: Box::leak(devices.to_vec().into_boxed_slice()),
            ok: true,
            ms: 120,
        }
    }

    fn lamp() -> Vec<String> {
        vec!["Hall lamp".to_owned()]
    }

    /// Push every waiting run back past the complaint window, standing in for
    /// the clock so the next `settle` has something to judge.
    fn age_out(db: &ToolCatalogStore) {
        db.conn
            .execute(
                "UPDATE shortcut SET pending_at = pending_at - ?1",
                params![COMPLAINT_WINDOW_SECS + 1],
            )
            .unwrap();
    }

    /// Run a phrase until it has earned the fast path.
    fn earn(db: &ToolCatalogStore, phrase: &str, devices: &[String]) {
        for _ in 0..CLEAN_RUNS_TO_PROMOTE {
            assert!(db.remember(&one(phrase, HALL, devices)).unwrap());
            age_out(db);
            db.settle("an unrelated command", &[]).unwrap();
        }
    }

    /// The whole promotion rule in one test: two clean runs and no sooner.
    /// Asymmetry is the point — a promotion that comes late costs a couple of
    /// seconds, and one that comes early moves the wrong thing.
    #[test]
    fn a_phrase_is_replayed_only_after_it_has_worked_twice() {
        let db = house();
        db.remember(&one("turn on the hall lamp", HALL, &lamp())).unwrap();
        assert!(db.replay("turn on the hall lamp").unwrap().is_none(), "one run is not evidence");

        age_out(&db);
        db.settle("an unrelated command", &[]).unwrap();
        assert!(db.replay("turn on the hall lamp").unwrap().is_none(), "still only one clean run");

        db.remember(&one("turn on the hall lamp", HALL, &lamp())).unwrap();
        age_out(&db);
        db.settle("an unrelated command", &[]).unwrap();
        let fast = db.replay("Turn on the hall lamp!").unwrap().expect("two clean runs promote");
        assert_eq!(fast.tool, "HassTurnOn");
        assert_eq!(fast.args, HALL);
        assert_eq!(fast.runs, 2);
    }

    /// The same rule again, in the order a *turn* applies it — which is not the
    /// order the test above uses. A turn asks for the fast path before it does
    /// anything, and writes itself down at the end. So: say a thing twice and
    /// the third time is already fast, one utterance per clean run and none
    /// spent waiting for a turn to notice. This is the count the page promises.
    #[test]
    fn said_twice_the_third_time_is_already_fast() {
        let db = house();
        let p = "stinge luminile în baie";
        // Exactly what `actions::Learning::finished` does, in that order.
        let turn = |db: &ToolCatalogStore| {
            db.settle(p, &lamp()).unwrap();
            db.remember(&Said { tool: "HassTurnOff", ..one(p, HALL, &lamp()) }).unwrap();
        };

        assert!(db.replay(p).unwrap().is_none(), "nothing is written down yet");
        turn(&db);

        age_out(&db); // a minute of not being contradicted
        assert!(db.replay(p).unwrap().is_none(), "one clean run is not two");
        turn(&db);

        age_out(&db);
        let fast = db.replay("Stinge luminile în baie.").unwrap().expect("the third time is fast");
        assert_eq!(fast.tool, "HassTurnOff");
        assert_eq!(fast.runs, 2, "and no third run was needed to notice");
    }

    /// The complaint signal, and the reason it is the device rather than a list
    /// of words meaning "no": asking for the same lamp again straight away is
    /// the same objection in every language.
    #[test]
    fn coming_back_about_the_same_device_at_once_keeps_a_phrase_slow() {
        let db = house();
        earn(&db, "turn on the hall lamp", &lamp());
        assert!(db.replay("turn on the hall lamp").unwrap().is_some());

        // Third run, then a different phrase about the same lamp before its
        // window closes.
        db.remember(&one("turn on the hall lamp", HALL, &lamp())).unwrap();
        db.settle("no, the other one", &lamp()).unwrap();
        assert!(
            db.replay("turn on the hall lamp").unwrap().is_none(),
            "one complaint makes it slow again"
        );
    }

    /// The thirty seconds are load-bearing, not a knob. The same command an
    /// hour later means somebody switched the lamp off, and treating that as a
    /// complaint would exclude the best candidates for the fast path.
    #[test]
    fn the_same_command_much_later_is_a_new_request_not_a_complaint() {
        let db = house();
        db.remember(&one("turn on the hall lamp", HALL, &lamp())).unwrap();
        age_out(&db);
        db.settle("turn on the hall lamp", &lamp()).unwrap();
        assert_eq!(
            db.shortcuts().unwrap()[0].clean,
            1,
            "outside the window the same command is a fresh request"
        );
    }

    /// Where the server does not name what it touched — every server Fono has
    /// no specific knowledge of — the phrase is the only handle, and repeating
    /// it inside the window is the complaint.
    #[test]
    fn without_named_devices_the_phrase_itself_is_the_handle() {
        let db = house();
        db.remember(&one("do the thing", HALL, &[])).unwrap();
        db.settle("something completely different", &[]).unwrap();
        assert_eq!(db.shortcuts().unwrap()[0].clean, 0, "not judged yet, and not spoiled either");

        db.settle("do the thing", &[]).unwrap();
        assert_eq!(db.shortcuts().unwrap()[0].clean, 0, "repeating it at once is the complaint");
    }

    /// A run that failed is dirty immediately: there is nothing to wait for.
    #[test]
    fn one_failed_run_makes_a_fast_phrase_slow_again() {
        let db = house();
        earn(&db, "turn on the hall lamp", &lamp());
        let failed = Said { ok: false, ..one("turn on the hall lamp", HALL, &lamp()) };
        db.remember(&failed).unwrap();
        assert!(db.replay("turn on the hall lamp").unwrap().is_none());
        assert_eq!(db.shortcuts().unwrap()[0].last_ok, Some(false));
    }

    /// Two clean runs promote a phrase *together with the command it made*. A
    /// phrase that now resolves somewhere else has not been verified there.
    #[test]
    fn a_phrase_that_now_means_a_different_command_starts_earning_again() {
        let db = house();
        earn(&db, "turn on the lamp", &lamp());
        db.remember(&one("turn on the lamp", r#"{"name":"Desk lamp"}"#, &lamp())).unwrap();
        assert!(db.replay("turn on the lamp").unwrap().is_none());
        assert_eq!(db.shortcuts().unwrap()[0].args, r#"{"name":"Desk lamp"}"#);
    }

    /// The order the model happened to write the fields in is not a different
    /// command, and counting it as one would stop anything ever being promoted.
    #[test]
    fn the_order_of_the_arguments_is_not_part_of_the_command() {
        let db = house();
        let a = r#"{"area":"Hall","name":"Hall lamp"}"#;
        let b = r#"{"name":"Hall lamp","area":"Hall"}"#;
        db.remember(&one("lights on", a, &lamp())).unwrap();
        age_out(&db);
        db.settle("unrelated", &[]).unwrap();
        db.remember(&one("lights on", b, &lamp())).unwrap();
        age_out(&db);
        db.settle("unrelated", &[]).unwrap();
        assert!(db.replay("lights on").unwrap().is_some(), "same command written two ways");
    }

    /// Never, however many times it has worked. An unlock replayed on a
    /// misheard word is not a latency problem.
    #[test]
    fn a_dangerous_command_is_never_learned() {
        let db = house();
        let unlock = DiscoveredTool {
            capability: Capability::Dangerous,
            ..tool("HassUnlock", serde_json::json!({}))
        };
        db.reconcile("ha", "sse", &[light_on(), light_off(), unlock]).unwrap();
        let said = Said { tool: "HassUnlock", ..one("unlock the front door", "{}", &[]) };
        assert!(!db.remember(&said).unwrap());
        assert!(db.shortcuts().unwrap().is_empty());
    }

    /// A phrase for a tool nobody offers is not a phrase for anything.
    #[test]
    fn a_command_the_catalogue_does_not_offer_is_not_learned() {
        let db = house();
        let said = Said { tool: "HassLightSet", ..one("dim the hall lamp", "{}", &[]) };
        assert!(!db.remember(&said).unwrap());
    }

    /// The reason a shortcut may only ever *name* something: replaying it runs
    /// the same command a second time, so "two degrees warmer" would be four.
    /// Judged on the shape of the values, so no tool name appears here and a
    /// server nobody has heard of is held to the same rule.
    #[test]
    fn a_command_asking_for_an_amount_is_never_learned() {
        let db = house();
        let named = r#"{"name":"Hall lamp"}"#;
        assert!(db.remember(&one("turn on the hall lamp", named, &lamp())).unwrap());

        for amount in [
            r#"{"name":"Hall lamp","brightness":40}"#,
            r#"{"name":"Hall lamp","temperature":-2.5}"#,
            r#"{"name":"Hall lamp","toggle":true}"#,
            // Buried a level down, where a shallow check would miss it.
            r#"{"target":{"name":"Hall lamp","step":10}}"#,
            r#"{"name":["Hall lamp",3]}"#,
            // Nothing can be said about the shape of what is not JSON.
            "brightness=40",
        ] {
            assert!(
                !db.remember(&one("make it warmer", amount, &lamp())).unwrap(),
                "learned an amount: {amount}"
            );
        }
        // Several devices by name is still naming things, not an amount.
        let two = r#"{"name":["Hall lamp","Desk lamp"]}"#;
        assert!(db.remember(&one("both lamps on", two, &lamp())).unwrap());
        assert_eq!(db.shortcuts().unwrap().len(), 2, "only the two naming rows exist");
    }

    /// Switching a tool off must not throw away what was learned about it —
    /// same rule as the tool rows themselves. Switching it back on resumes.
    #[test]
    fn switching_a_tool_off_pauses_its_phrases_rather_than_forgetting_them() {
        let db = house();
        earn(&db, "turn on the hall lamp", &lamp());
        db.set_enabled("ha", "HassTurnOn", false).unwrap();
        assert_eq!(db.shortcuts().unwrap()[0].stale, Some(Stale::Paused));
        assert!(db.replay("turn on the hall lamp").unwrap().is_none());

        db.set_enabled("ha", "HassTurnOn", true).unwrap();
        assert!(db.replay("turn on the hall lamp").unwrap().is_some(), "it resumes as it was");
    }

    /// A tool whose published shape has moved is not the tool that worked, so
    /// what would be replayed is no longer what was verified.
    #[test]
    fn a_tool_whose_shape_moved_is_no_longer_the_one_that_worked() {
        let db = house();
        earn(&db, "turn on the hall lamp", &lamp());
        let widened = tool(
            "HassTurnOn",
            serde_json::json!({ "properties": { "area": { "type": "string" },
                                                "brightness": { "type": "integer" } } }),
        );
        db.reconcile("ha", "sse", &[widened, light_off()]).unwrap();
        assert_eq!(db.shortcuts().unwrap()[0].stale, Some(Stale::Changed));
        assert!(db.replay("turn on the hall lamp").unwrap().is_none());
    }

    /// Another way of saying the same thing is a second row, not a rename, and
    /// it runs as readily as the phrase it was copied from: the command was
    /// verified, only the wording is new.
    #[test]
    fn a_written_phrase_points_at_the_same_command_and_runs_as_readily() {
        let db = house();
        earn(&db, "turn on the hall lamp", &lamp());
        db.add_phrase("turn on the hall lamp", "aprinde lampa din hol").unwrap();

        let rows = db.shortcuts().unwrap();
        assert_eq!(rows.len(), 2, "two ways of saying it, one command");
        let written = rows.iter().find(|r| r.origin == Origin::Written).unwrap();
        assert_eq!(written.tool, "HassTurnOn");
        assert_eq!(written.args, HALL);
        assert!(written.fast(), "it inherits what the phrase it copies has earned");
        assert!(db.replay("aprinde lampa din hol").unwrap().is_some());

        // The learned one is untouched by the addition.
        assert!(db.replay("turn on the hall lamp").unwrap().is_some());

        db.forget_phrase("Aprinde lampa din hol.").unwrap();
        assert_eq!(db.shortcuts().unwrap().len(), 1);
        assert!(db.add_phrase("nothing said this", "x").is_err());
    }

    /// And a wording copied from a phrase that has not earned the fast path
    /// does not conjure one: it inherits the standing, whatever that is.
    #[test]
    fn a_written_phrase_inherits_an_unearned_standing_too() {
        let db = house();
        db.remember(&one("turn on the hall lamp", HALL, &lamp())).unwrap();
        db.add_phrase("turn on the hall lamp", "aprinde lampa din hol").unwrap();
        assert!(db.replay("aprinde lampa din hol").unwrap().is_none());
    }

    /// Removing a server takes its phrases with it: one that points at a
    /// server the user deleted can never fire, and leaving it on the page
    /// permanently paused would be a bug rather than caution.
    #[test]
    fn removing_a_server_forgets_the_phrases_that_used_it() {
        let db = house();
        earn(&db, "turn on the hall lamp", &lamp());
        db.forget_sources_except(&["elsewhere".to_owned()]).unwrap();
        assert!(db.shortcuts().unwrap().is_empty());
    }

    #[test]
    fn phrases_are_compared_without_punctuation_case_or_extra_spaces() {
        assert_eq!(normalise_phrase("  Turn ON  the hall lamp!  "), "turn on the hall lamp");
        assert_eq!(normalise_phrase("Aprinde lampa, te rog."), "aprinde lampa te rog");
        // Diacritics are kept: two spellings are two rows pointing at one
        // command, which is what this store already does with two languages.
        assert_eq!(normalise_phrase("Lumină"), "lumină");
        assert_eq!(normalise_phrase("   "), "");
    }
}
