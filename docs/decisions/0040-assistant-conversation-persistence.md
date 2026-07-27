# ADR 0040 — Assistant conversation persistence and the history browser

- **Status:** Accepted
- **Date:** 2026-07-27
- **Related:** [ADR 0038 — Inbound API-key authentication](0038-inbound-api-key-auth-and-usage.md)
- **Plan:** [`plans/2026-07-27-conversation-history-and-web-history-browser-v2.md`](../../plans/2026-07-27-conversation-history-and-web-history-browser-v2.md)

## Context

Dictation transcriptions have always been persisted to `history.sqlite`,
complete with the verified speaker name when speaker verification is on.
Assistant conversations were not: the rolling turn buffer lived only in
memory on the orchestrator and was discarded on daemon restart or when the
user hit "Forget conversation".

That produced three gaps:

1. **No resume.** Restarting the daemon mid-conversation lost all context.
2. **No review.** There was no way to see what the assistant actually said,
   or which tools it invoked on the user's behalf — an audit gap, given the
   tool catalogue can reach physical devices.
3. **No speaker attribution.** The daemon already resolves the verified
   speaker for every assistant turn and feeds it into the system prompt,
   then threw the value away.

Separately, users had no way to browse either history without reaching for
`sqlite3` by hand.

## Decision

### 1. Persist assistant conversations in a dedicated `conversations.sqlite`

A new store under the data directory (`$XDG_DATA_HOME/fono/` on desktop,
`/var/lib/fono/` for the system service), beside `history.sqlite`,
`api_keys.sqlite` and `speakers.sqlite`.

Two tables:

- `thread` — one row per conversation: timestamps, assistant backend,
  model, originating app class/title.
- `turn` — one row per utterance, reply, tool call or tool result, ordered
  within its thread.

Threads are segmented by an **idle timeout** (default 5 minutes) plus the
explicit "Forget conversation" action. Five minutes matches the assistant's
own in-memory `history_window_minutes`, so a thread ends at roughly the
moment its turns stop influencing the prompt — the saved boundary and the
remembered context agree.

### 2. Speaker is stored per *turn*, not per thread

A conversation can involve more than one person. The speaker name is
recorded on each turn, exactly as the dictation path already records it on
each transcription.

The name is stored as a **historical fact**, not a foreign key. Renaming or
deleting an enrolled speaker does not rewrite past history — and, more
importantly, erasing someone's biometric enrollment does not require
rewriting unrelated conversation rows.

Only the **name** is ever stored. Voice-print embeddings never leave
`speakers.sqlite` and never cross the HTTP boundary.

### 3. Same privacy posture as dictation history

- File clamped to owner-only `0600` on Unix, including WAL/SHM sidecars.
- Finite default retention (90 days), purged by the same scheduled sweep
  that already prunes dictation history.
- Opt-out via `[conversations] enabled = false`, which creates **no file at
  all** — the absence of the file is the only credible proof of opt-out.

**Amended 2026-07-28 — conversation turns are no longer redacted.** This
decision originally reused `fono_core::history::redact` on insert. In
practice that pass masks any run of 20 or more word characters, and
assistant turns are full of them: entity ids like
`binary_sensor.audi_e_tron_plug_lock_state`, turn ids, and error type names
were all replaced with `[REDACTED]`, which made stored threads unreadable.
Worse, those rows are replayed into the prompt on resume, so the model read
`[REDACTED]` as if it were content — a correctness bug, not only a display
wart.

The rule is not load-bearing here either way: a spoken conversation is
prose, not a place API keys are pasted, so the pattern had a high false
positive rate and a near-zero true positive rate. Dictation history keeps
its `redact_secrets` knob, where the user may genuinely dictate a key.
Conversations are stored verbatim, and the remaining protections — `0600`,
retention, the file-creating opt-out, and explicit per-thread delete — are
what carry the privacy posture.

### 4. "Forget conversation" ends the thread; it does not delete it

The tray action has always been a fresh-start affordance. Silently
promoting it to an erasure command would be a surprising regression.
Deletion is a separate, explicit control on the history page.

### 5. Browsing happens in the web UI, not the CLI

A `#/history` view in the existing web settings SPA, served from the same
token-gated, loopback-default HTTP surface as `#/doctor`. It shows
dictation transcriptions (with FTS5 search and speaker attribution) and
conversation threads (expandable into turns, with per-turn speaker and
distinctly-rendered tool calls), plus per-thread delete.

No new CLI subcommands. The store API leaves that option open later at low
cost, but the browser is the interface.

## Consequences

**Positive**

- Conversations survive a restart, and resume automatically when the
  restart falls inside the idle window.
- The user can see who Fono thought was speaking, per turn.
- Tool invocations are auditable after the fact.
- Data the user cannot inspect or delete is a liability; the history page
  removes that liability for both stores.

**Negative / accepted**

- Fono now durably records spoken conversations that previously evaporated.
  Mitigated by the `0600` clamp, finite retention, and a
  file-creating opt-out. Documented prominently.
- One more SQLite file. Accepted: it carries its own retention policy and
  its own erasure semantics, so folding it into `history.sqlite` would make
  "purge my dictation history" ambiguous.
- Writes on the assistant path. Mitigated by keeping the in-memory buffer
  authoritative for prompt construction and writing only at turn
  boundaries, never mid-token.

## Alternatives considered

- **Store conversations inside `history.sqlite`.** Fewer files, but
  entangles two different retention policies.
- **Append-only JSONL.** Greppable, but no indexed queries, no
  transactional integrity, and a second storage paradigm to maintain.
- **In-memory only, with an explicit export command.** Zero durable
  footprint, but does not solve resume — the gap that motivated this work.
- **Persist only on explicit "keep this".** The decision arrives too late;
  by the time you know you wanted it kept, the daemon has restarted.

## Deliberately unchanged

Two adjacent questions were raised and answered during this work; recording
them here so they need not be re-litigated.

**The four separate SQLite files stay separate.** Each carries a distinct
security and lifecycle profile: transcripts (retention-purged), credentials
(must survive a history wipe), biometric voice-prints (need an independent
erasure path), and the tool catalogue (machine-derived, disposable). A
single file would force the strictest posture onto everything and couple
credential storage to transcript retention.

**The XDG directory split stays.** `~/.config` (settings), `~/.local/share`
(data), `~/.cache` (gigabytes of re-downloadable model weights) and
`~/.local/state` (socket, pid) encode backup semantics. Merging them would
either force multi-gigabyte backups or destroy the "cache is safe to
delete" guarantee. The same split maps onto the native
`%APPDATA%` / `%LOCALAPPDATA%` convention on Windows.

Also note that the six tables visible in `history.sqlite` are **one**
designed table plus five FTS5 shadow tables that SQLite creates and manages
itself. They are not reducible without giving up full-text search.
