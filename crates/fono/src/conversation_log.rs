// SPDX-License-Identifier: GPL-3.0-only
//! Write-through persistence for assistant conversations (ADR 0040).
//!
//! The in-memory [`fono_assistant::ConversationHistory`] stays
//! authoritative for prompt construction — it is on the latency-critical
//! path. This sink mirrors the same turns to `conversations.sqlite` at
//! turn boundaries (never mid-token) so a conversation survives a daemon
//! restart and can be reviewed afterwards in the history page.
//!
//! Everything here is best-effort: a failure to persist logs a warning and
//! is otherwise swallowed. Losing a history row must never break a reply
//! the user is waiting to hear.
//!
//! When `[conversations].enabled = false` the sink holds no store at all
//! and every method is a no-op, so `conversations.sqlite` is never created.

use std::path::Path;

use fono_core::config::Conversations as ConversationsConfig;
use fono_core::conversations::{ConversationStore, Turn, TurnRole};
use tracing::{debug, warn};

/// Write-through sink around [`ConversationStore`], tracking which thread
/// is currently open.
pub struct ConversationSink {
    /// `None` when persistence is disabled — the whole sink degrades to
    /// no-ops and no file is created.
    store: Option<ConversationStore>,
    cfg: ConversationsConfig,
    /// The thread turns are currently being appended to. Lazily opened on
    /// the first turn so an idle daemon creates no empty threads.
    thread: Option<i64>,
    /// Unix time of the last turn appended to `thread`. Drives idle
    /// segmentation without a DB round-trip on every turn.
    last_turn_at: i64,
}

impl ConversationSink {
    /// Build a sink for `path`. Returns a disabled sink when
    /// `cfg.enabled` is false, or when the store cannot be opened (the
    /// assistant must still work without its history).
    #[must_use]
    pub fn open(path: &Path, cfg: ConversationsConfig) -> Self {
        if !cfg.enabled {
            debug!("conversation persistence disabled; not opening conversations.sqlite");
            return Self { store: None, cfg, thread: None, last_turn_at: 0 };
        }
        let store = match ConversationStore::open(path) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("could not open conversation store; conversations will not be saved: {e:#}");
                None
            }
        };
        Self { store, cfg, thread: None, last_turn_at: 0 }
    }

    /// A disabled sink (tests, and the assistant-less configurations).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            store: None,
            cfg: ConversationsConfig { enabled: false, ..ConversationsConfig::default() },
            thread: None,
            last_turn_at: 0,
        }
    }

    /// Whether turns are actually being persisted.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.store.is_some()
    }

    /// Adopt the most recent thread if it is still open and within the
    /// idle window, and return its last `limit` turns oldest-first so the
    /// caller can rehydrate the in-memory rolling window.
    ///
    /// Called once at startup. Returns an empty vec when there is nothing
    /// to resume, which is also the disabled-sink answer.
    pub fn resume(&mut self, limit: usize) -> Vec<Turn> {
        let Some(store) = &self.store else { return Vec::new() };
        match store.resumable_thread(self.cfg.idle_secs()) {
            Ok(Some(thread)) => {
                let turns = store.recent_turns(thread.id, limit).unwrap_or_default();
                debug!(
                    thread = thread.id,
                    turns = turns.len(),
                    "resuming previous assistant conversation"
                );
                self.thread = Some(thread.id);
                self.last_turn_at = thread.last_at;
                turns
            }
            Ok(None) => Vec::new(),
            Err(e) => {
                warn!("could not check for a resumable conversation: {e:#}");
                Vec::new()
            }
        }
    }

    /// The thread to append to, opening one if needed. Also enforces idle
    /// segmentation: if the open thread has been silent for longer than
    /// the configured timeout, it is closed and a fresh one started.
    fn thread_for_turn(&mut self, backend: Option<&str>, model: Option<&str>) -> Option<i64> {
        let store = self.store.as_ref()?;
        let now = now_unix();
        if let Some(id) = self.thread {
            // Segment on idle: a long gap means the previous conversation
            // is over, even though nothing explicitly closed it.
            if now - self.last_turn_at > self.cfg.idle_secs() {
                let _ = store.close_thread(id);
                debug!(thread = id, "closing conversation thread after idle timeout");
                self.thread = None;
            }
        }
        if self.thread.is_none() {
            match store.open_thread(backend, model, None, None) {
                Ok(id) => {
                    debug!(thread = id, "started a new assistant conversation thread");
                    self.thread = Some(id);
                }
                Err(e) => warn!("could not start a conversation thread: {e:#}"),
            }
        }
        self.last_turn_at = now;
        self.thread
    }

    /// Record a spoken user turn, attributed to `speaker` when speaker
    /// verification produced a match.
    pub fn record_user(&mut self, text: &str, speaker: Option<&str>, backend: Option<&str>) {
        self.append(TurnRole::User, text, speaker, backend, None, false);
    }

    /// Record the assistant's reply. `partial` marks a turn that was cut
    /// short by the user cancelling mid-generation — the text is whatever
    /// was produced up to that point, which is exactly what someone
    /// reviewing the conversation later wants to see.
    pub fn record_assistant(
        &mut self,
        text: &str,
        partial: bool,
        latency_ms: Option<i64>,
        backend: Option<&str>,
    ) {
        self.append(TurnRole::Assistant, text, None, backend, latency_ms, partial);
    }

    /// Record a tool the assistant chose to invoke.
    pub fn record_tool_call(&mut self, name: &str, args: &str, backend: Option<&str>) {
        let text = if args.is_empty() { name.to_string() } else { format!("{name} {args}") };
        self.append(TurnRole::ToolCall, &text, None, backend, None, false);
    }

    /// Record what a tool handed back, along with the executor's verdict on
    /// it. The verdict is stored rather than left to be read back out of the
    /// text later, because it is not legible there: a Home Assistant call
    /// that worked returns a payload ending in `"failed": []`.
    pub fn record_tool_result(&mut self, summary: &str, failed: bool, backend: Option<&str>) {
        self.append_result(summary, backend, Some(!failed));
    }

    fn append(
        &mut self,
        role: TurnRole,
        text: &str,
        speaker: Option<&str>,
        backend: Option<&str>,
        latency_ms: Option<i64>,
        partial: bool,
    ) {
        self.write(role, text, speaker, backend, latency_ms, partial, None);
    }

    fn append_result(&mut self, text: &str, backend: Option<&str>, ok: Option<bool>) {
        self.write(TurnRole::ToolResult, text, None, backend, None, false, ok);
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        role: TurnRole,
        text: &str,
        speaker: Option<&str>,
        backend: Option<&str>,
        latency_ms: Option<i64>,
        partial: bool,
        ok: Option<bool>,
    ) {
        if self.store.is_none() || text.trim().is_empty() {
            return;
        }
        let Some(thread_id) = self.thread_for_turn(backend, None) else { return };
        let mut turn = Turn::new(thread_id, role, text);
        turn.speaker = speaker.map(str::to_owned);
        turn.latency_ms = latency_ms;
        turn.partial = partial;
        turn.ok = ok;
        if let Some(store) = &self.store {
            if let Err(e) = store.append_turn(&turn) {
                warn!("could not save a conversation turn: {e:#}");
            }
        }
    }

    /// Close the current thread so the next turn starts a fresh one.
    /// Backs the tray "Forget conversation" action and daemon shutdown.
    ///
    /// This deliberately **ends** the thread rather than deleting it —
    /// "forget" has always been a fresh-start affordance, and promoting it
    /// to an erasure command would surprise anyone relying on the old
    /// behaviour. Deletion lives on the history page.
    pub fn close_thread(&mut self) {
        let Some(id) = self.thread.take() else { return };
        if let Some(store) = &self.store {
            if let Err(e) = store.close_thread(id) {
                warn!("could not close the conversation thread: {e:#}");
            }
        }
    }

    /// Drop threads past the configured retention. Driven by the same
    /// scheduled sweep that prunes dictation history.
    pub fn purge_expired(&self) {
        let Some(store) = &self.store else { return };
        match store.purge_older_than(self.cfg.retention_days) {
            Ok(0) => {}
            Ok(n) => debug!("purged {n} conversation thread(s) past retention"),
            Err(e) => warn!("conversation retention sweep failed: {e:#}"),
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> ConversationsConfig {
        ConversationsConfig { enabled, retention_days: 90, idle_timeout_minutes: 5 }
    }

    #[test]
    fn disabled_sink_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        let mut sink = ConversationSink::open(&path, cfg(false));
        sink.record_user("hello", Some("Alex"), Some("groq"));
        sink.record_assistant("hi", false, None, Some("groq"));
        assert!(!sink.is_enabled());
        assert!(!path.exists(), "opt-out must not create the database");
    }

    #[test]
    fn turns_are_persisted_with_speaker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        let mut sink = ConversationSink::open(&path, cfg(true));
        sink.record_user("turn on the lights", Some("Alex"), Some("groq"));
        sink.record_tool_call("light.turn_on", "{}", Some("groq"));
        sink.record_tool_result("ok", false, Some("groq"));
        sink.record_assistant("Done.", false, Some(120), Some("groq"));

        let store = ConversationStore::open(&path).unwrap();
        let summaries = store.recent_threads(10).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].speakers, vec!["Alex".to_string()]);

        let turns = store.turns(summaries[0].thread.id).unwrap();
        let roles: Vec<_> = turns.iter().map(|t| t.role).collect();
        assert_eq!(
            roles,
            vec![TurnRole::User, TurnRole::ToolCall, TurnRole::ToolResult, TurnRole::Assistant]
        );
        assert_eq!(turns[0].speaker.as_deref(), Some("Alex"));
        assert_eq!(turns[3].latency_ms, Some(120));
    }

    #[test]
    fn empty_turns_are_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        let mut sink = ConversationSink::open(&path, cfg(true));
        sink.record_assistant("   ", false, None, None);
        let store = ConversationStore::open(&path).unwrap();
        assert_eq!(store.thread_count().unwrap(), 0, "blank turns must not open a thread");
    }

    #[test]
    fn close_then_record_starts_a_new_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        let mut sink = ConversationSink::open(&path, cfg(true));
        sink.record_user("first", None, None);
        sink.close_thread();
        sink.record_user("second", None, None);

        let store = ConversationStore::open(&path).unwrap();
        assert_eq!(store.thread_count().unwrap(), 2);
    }

    #[test]
    fn resume_picks_up_a_recent_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        {
            let mut sink = ConversationSink::open(&path, cfg(true));
            sink.record_user("before the restart", Some("Alex"), None);
            sink.record_assistant("noted", false, None, None);
        }
        // A "restart": brand-new sink over the same file.
        let mut sink = ConversationSink::open(&path, cfg(true));
        let resumed = sink.resume(8);
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].text, "before the restart");
        assert_eq!(resumed[0].speaker.as_deref(), Some("Alex"));

        // Continuing appends to the resumed thread, not a new one.
        sink.record_user("after the restart", None, None);
        let store = ConversationStore::open(&path).unwrap();
        assert_eq!(store.thread_count().unwrap(), 1);
    }

    #[test]
    fn resume_skips_a_closed_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversations.sqlite");
        {
            let mut sink = ConversationSink::open(&path, cfg(true));
            sink.record_user("done talking", None, None);
            sink.close_thread();
        }
        let mut sink = ConversationSink::open(&path, cfg(true));
        assert!(sink.resume(8).is_empty(), "an ended thread must not resume");
    }
}
