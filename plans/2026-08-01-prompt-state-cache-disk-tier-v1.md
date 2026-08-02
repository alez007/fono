# Disk-Backed Prompt-State Cache — Feasibility and Staged Plan

## Objective

Decide whether to give the existing in-memory prompt-state (KV) cache a
persistent, disk-backed second tier so that many conversation heads survive
eviction and daemon restarts, and so that repeat callers — especially the
stateless HTTP/Ollama surface — stop paying cold prefill.

Verdict up front: **the mechanism is easy, the economics are excellent, and it
is almost certainly the wrong thing to build first.** The cache does not miss
today because it runs out of room. It misses because the prompt prefix keeps
changing underneath it. Disk fixes capacity; it does nothing for divergence.

---

## Assessment

### What already exists

`crates/fono-core/src/prompt_cache.rs:206-406` is already the hard part:
content-addressed keys, LRU, a byte budget, pinning, longest-token-prefix
matching, and dominated-entry pruning. Entries are deliberately *standalone*
snapshots with no inter-entry references
(`crates/fono-core/src/prompt_cache.rs:12-21`) — which is exactly the property
that makes independent files on disk safe. The key already has a filename
built into it (`crates/fono-core/src/prompt_cache.rs:137-146`), and the payload
is a plain `Vec<u8>` (`crates/fono-core/src/prompt_cache.rs:156-173`).

### The economics are not close

Measured on `gemma-4-e2b` Q4_0, the default for both roles
(`crates/fono-core/src/config.rs:1043-1044`):

| Quantity | Value |
|---|---|
| KV state per token | **≈ 18.4 KB** (linear across nine data points, 0.5 MB → 62 MB) |
| Restore (memcpy into a fresh context) | **flat 14–39 ms**, independent of blob size |
| Prefill | **≈ 8.7–14 ms/token** (72–115 tok/s, degrading with depth) |
| Same figures, `gemma-4-26B-A4B` MoE | ≈ 225 KB/token, 68–90 ms/token prefill |

Break-even disk bandwidth — the read rate at which pulling a blob costs the
same as recomputing it — is `18.4 KB / 8.7 ms` = **≈ 2.1 MB/s** for the small
model and ≈ 3.3 MB/s for the MoE. Every storage medium in existence beats
that, including a USB 2.0 stick. A 30 MB checkpoint is ~12 ms off NVMe, ~60 ms
off SATA, ~300 ms off a spinning disk, against 20+ seconds of prefill.

**So the read side is unconditionally a win.** The cost of this feature is
entirely in write amplification, format fragility, and privacy. Not latency.

### But capacity is not what is failing

- Observed live occupancy is **4–5 entries / 13–15 MB** against a 256 MiB
  budget — five percent. The **8-entry cap** binds first, never the bytes
  (`crates/fono-core/src/prompt_cache.rs:218-222`).
- Three pinnable layers (`crates/fono-core/src/prompt_cache.rs:91-93`) plus one
  `F7Context` per focused app already crowd those eight slots, and
  `evict_over_budget` refuses to drop pins
  (`crates/fono-core/src/prompt_cache.rs:387-393`).
- A single maximal assistant checkpoint at the default `n_ctx = 8192`
  (`crates/fono-core/src/config.rs:1500-1507`) is ~151 MB — **59% of the entire
  byte budget**. Two deep conversations cannot both be resident.

The one-line fix — raise `max_entries`, make both knobs configurable — captures
most of the "more pinned things, more heads" you are after, at zero risk and
zero new failure surface. That should land before anything touches a disk.

### The real ceiling is prefix stability

Research surfaced eight distinct prefix-breakers. Four of them fire constantly:

1. **`forget_after_action` wipes history after every tool-using turn**
   (`crates/fono/src/assistant.rs:1354-1356`). Its own doc comment
   (`crates/fono/src/assistant.rs:1341-1347`) admits it was added *partly* to
   protect the cache. On the smart-home path the conversation is reset to
   length zero more or less continuously — there is no deep prefix to persist.
2. **History pruning pops from the front**
   (`crates/fono-assistant/src/history.rs:182-197`), and under Gemma the system
   prompt is welded to the first user turn
   (`crates/fono-assistant/src/llama_local.rs:2109-2115`). The first eviction
   relocates the system block and invalidates **every** cached prefix at once.
   Defaults are 5 minutes / 12 turns
   (`crates/fono-assistant/src/history.rs:201-207`).
3. **The tool block and the area hint sit at the front of the head**
   (`crates/fono-assistant/src/local_tools.rs:71-115`,
   `crates/fono/src/actions/mod.rs:798-822`), rebuilt per turn from SQLite. Any
   catalogue re-sync or a renamed room re-keys everything behind it.
4. **The HTTP surface never pins and never warms.** `pin_prefix` requires empty
   history and no turn notes (`crates/fono-assistant/src/llama_local.rs:2317`),
   so a client on turn 5 never pins; the code already notes the LLM server
   "never warms at all" (`crates/fono-assistant/src/llama_local.rs:2306-2308`).

A persistent cache addresses none of these. Building it now would mean
carefully preserving, on disk, prefixes that the prompt builder is about to
invalidate anyway.

### On the tree idea — an important distinction

**Tree as an index: yes, cheap, safe.** A radix/trie over token ids replaces
the O(n·m) linear scan in `find_longest_prefix`
(`crates/fono-core/src/prompt_cache.rs:355-372`) and gives natural per-branch
accounting. Worth doing once entry counts pass a few hundred. Honest caveat:
at 500 entries × 2000 tokens the current linear scan still costs about a
millisecond, so this is a nice-to-have, not a prerequisite.

**Tree as storage — storing deltas with parent/child KV chains: effectively
infeasible with this API, and it undoes a deliberate decision.**
`llama_copy_state_data` always emits the *whole* context state; there is no way
to serialize "just the cells after position N". Building a delta tree would
need per-cell surgery llama.cpp does not expose. It would also re-introduce
precisely what `crates/fono-core/src/prompt_cache.rs:12-21` rejected on
purpose: evicting a parent invalidates every child, so arbitrary LRU eviction
stops being safe.

**The available way to get real "more heads" is sequence ids.** The binding
already wraps `state_seq_get` / `state_seq_set` safely, supports cross-sequence
restore, and — with `PARTIAL_ONLY` — documents itself as the *only* correct
rewind for Gated Delta Net models, which is what `gemma-4` is. Fono uses none
of it: every batch hardcodes sequence `0`, and `with_n_seq_max` is never
called. That is a larger change than a disk tier but attacks a real defect
(see risk 3).

---

## Implementation Plan

### Stage 1 — Capacity and evidence (do this first; it may be all you need)

- [ ] Task 1. Make the cache budget configurable and raise the entry cap.
      Add `max_entries` / `max_bytes` keys for both roles in
      `crates/fono-core/src/config.rs`, replacing the two hardcoded
      `PromptStateCache::default()` call sites
      (`crates/fono-assistant/src/llama_local.rs:264`,
      `crates/fono-polish/src/llama_local.rs:112`). Default the entry cap well
      above 8 — the byte budget is the meaningful limit and is barely touched.
      Rationale: this is the entire "more heads" ask, in a day, with no new
      failure modes.
- [ ] Task 2. Exclude pinned entries from the entry cap, or account for them
      separately. Today three pins consume three of eight slots and
      `evict_over_budget` will not reclaim them, so the effective cap for
      conversation heads is five.
- [ ] Task 3. Instrument the **miss taxonomy**, not the hit rate. On every
      lookup, record why the deepest match was not deeper: capacity eviction,
      prefix divergence (and at which token position and against which layer),
      or runtime-key change. Count `decoded_prefix_tokens`, never hit counts —
      a 72-token pin registering as a "hit" alongside a 958-token cold read has
      already misled this project once.
- [ ] Task 4. Add a high-water mark for `cache_bytes` / `cache_entries` at turn
      end. The per-lookup values are already emitted
      (`crates/fono-assistant/src/llama_local.rs:482`,
      `crates/fono-polish/src/llama_local.rs:548`) but never aggregated.
- [ ] Task 5. Run the workload for a week and read Task 3's output. **Gate:
      proceed to Stage 3 only if capacity eviction, not divergence, dominates
      the wasted prefill tokens.** If divergence dominates, go to Stage 2
      instead and stop.

### Stage 2 — Fix prefix stability (likely the higher-value work)

- [ ] Task 6. Stop history pruning from moving the system block. Under Gemma
      the system prompt rides on the first user turn, so front-eviction is
      catastrophic. Either render the system block as its own leading segment
      independent of turn 1, or prune by dropping *whole trailing* turns from a
      fixed anchor. Rationale: this single change converts the most common
      total invalidation into a partial one.
- [ ] Task 7. Move the volatile front-of-prompt material behind the stable
      material. The area/device hint (`crates/fono/src/actions/mod.rs:798-822`)
      and the tool block currently precede the static instructions, so a
      renamed room invalidates the whole head. Ordering stable-first is free.
- [ ] Task 8. Separate the two motives inside `forget_after_action`
      (`crates/fono/src/assistant.rs:1341-1356`). The routing-quality half is
      real and must stay; the cache-protection half dissolves once Tasks 6–7
      land. Relaxing the clear is what actually enables multi-turn conversation.
- [ ] Task 9. Give the HTTP surface a warm path. It is the surface with the most
      conversations and the least cache help. At minimum, let a network turn
      pin the shared head even when history is non-empty.
- [ ] Task 10. Fix the dropped tool turns in the wire adapter
      (`crates/fono-net/src/llm_server/messages.rs:79-80`). A client replaying a
      tool-using thread builds a prompt that differs from what it believes it
      sent, which presents as unexplainable cache misses.
- [ ] Task 11. Add a radix/trie index over `prefix_tokens` to replace the linear
      scan in `find_longest_prefix`, once Task 1's raised cap makes entry counts
      large enough to matter.

### Stage 3 — The disk tier (only if Task 5's gate opens)

- [ ] Task 12. Write an ADR before any code. ADR 0040
      (`docs/decisions/0040-assistant-conversation-persistence.md`) set the
      precedent that a new durable store of conversation-derived content gets
      one, with a four-part posture: 0600 permissions, finite retention, an
      opt-out that creates no file at all, and an explicit delete control.
      A KV blob is a materialisation of the transcript and must clear that bar.
- [ ] Task 13. Introduce an explicit `STATE_FORMAT_VERSION` constant, bumped by
      hand, composed with the `llama-cpp-2` version — and **decouple the disk
      key from `CARGO_PKG_VERSION`**, which is folded in today
      (`crates/fono-assistant/src/llama_local.rs:1519`). As written, every Fono
      point release would discard the entire disk cache, which defeats the
      feature's whole purpose.
- [ ] Task 14. Replace model mtime in the runtime key
      (`crates/fono-assistant/src/llama_local.rs:1509-1516`) with the GGUF
      content hash for the persistent tier. mtime does not survive backup,
      restore, or package reinstall.
- [ ] Task 15. Define the on-disk format: a fixed header carrying magic,
      format version, runtime key, token count, blob length and a SHA-256 of the
      payload; then the payload; then the `prefix_tokens` vector. **The token
      vector is mandatory** — an entry that loses it silently drops out of
      longest-prefix matching entirely
      (`crates/fono-core/src/prompt_cache.rs:355-372`).
- [ ] Task 16. Store under `<cache_dir>/prompt-state/`
      (`crates/fono-core/src/paths.rs:37-54`), one content-addressed file per
      entry, mode 0600. Publish via `.part` + verify + rename, following
      `crates/fono-download/src/lib.rs:41-90`. Content addressing makes
      concurrent writers from the daemon and the separate MCP-server process
      harmless — same content, same bytes, last writer wins.
- [ ] Task 17. Maintain a small index file (key → path, bytes, tokens, layer,
      last-access) written atomically. Do **not** rely on filesystem atime;
      `relatime` and `noatime` are common. Reconcile index against directory on
      startup: delete orphan blobs, drop index rows with no file.
- [ ] Task 18. Adopt **write-back on eviction, not write-through on insert.**
      Most entries are killed by `prune_dominated_by`
      (`crates/fono-core/src/prompt_cache.rs:277-310`) within the same
      conversation and must never reach the disk. Skip the write when the
      content-addressed file already exists.
- [ ] Task 19. Perform all disk I/O off the model mutex. Reads resolve before
      the lock is taken; writes go to a niced background thread following
      `crates/fono-polish/src/llama_local.rs:1194-1229`. A multi-megabyte write
      under the model lock would stall every other conversation.
- [ ] Task 20. Prune by last-access against a configurable byte budget, swept at
      startup alongside the existing history purge
      (`crates/fono/src/session.rs:3549-3570`) — the established place, chosen
      deliberately over a background timer.
- [ ] Task 21. Quarantine on failure. Any header mismatch, checksum failure, or
      `set_state_data` rejection deletes the file and records the miss; never
      retry a bad blob. Wire the polish degenerate-output guard
      (`crates/fono-polish/src/llama_local.rs:1051-1060`) to **delete the
      backing disk entry**, not merely retry — otherwise a poisoned entry keeps
      poisoning across restarts.
- [ ] Task 22. Wire the disk cache into the existing privacy controls: the
      "clear history" action must wipe it, and the opt-out must prevent the
      directory from ever being created.
- [ ] Task 23. Promote `atomic_write`
      (`crates/fono-core/src/config.rs:2316-2338`) from `pub(crate)`, or place
      the disk tier inside `fono-core`. Keep `prompt_cache.rs` itself
      llama-agnostic (`crates/fono-core/src/prompt_cache.rs:5-10`) — validity
      checks stay key-based, never ABI-introspective.
- [ ] Task 24. Confirm no net-new dependency. `serde`, `serde_json`, `sha2`,
      `bincode`, `hex` and `tempfile` are all already linked. **Do not add a
      compression crate** — KV data compresses poorly, and
      `docs/binary-size.md:63-67` records the precedent of rejecting a runtime
      decompressor that cost more code than it saved.

### Stage 4 — Optional, and independently justified

- [ ] Task 25. Investigate `state_seq_get` / `state_seq_set` with
      `PARTIAL_ONLY`. This is not primarily an optimisation: the assistant's
      post-generation KV truncation
      (`crates/fono-assistant/src/llama_local.rs:767`) uses
      `clear_kv_cache_seq`, which the binding documents as *unable* to roll back
      partial recurrent state on Gated Delta Net models — i.e. on `gemma-4`,
      the default. Fix this **before** persisting completed-turn checkpoints.

---

## Verification Criteria

- Stage 1 ships with cache capacity user-configurable and the entry cap no
  longer the binding constraint; a week of traces attributes every wasted
  prefill token to exactly one cause.
- Stage 2 is verified by a test proving the cached prefix survives a history
  eviction — i.e. evicting turn 1 does not change the leading tokens.
- The disk tier restores a checkpoint written by a *previous process* and the
  turn records zero decoded prefix tokens.
- A corrupted, truncated, or wrong-version blob produces a clean miss and a
  deleted file, never a crash and never degraded output.
- Steady-state disk footprint stays under its configured budget across a
  multi-day run, with pruning driven by recorded access time.
- No new entry in `Cargo.lock`; `nice -n 10 ./tests/check.sh --size-budget`
  passes with the binary under the 25 MiB `cpu` row.
- Turning the feature off leaves no directory on disk; clearing history removes
  every blob.

## Potential Risks and Mitigations

1. **Format fragility — a persisted blob outlives the binary that wrote it.**
   The llama.cpp state layout is internal and version-coupled, and feeding a
   stale-format blob to `set_state_data` can abort the process.
   Mitigation: explicit `STATE_FORMAT_VERSION` composed with the binding
   version, a header magic, a length check and a payload checksum, all verified
   before the blob is handed to llama.cpp (Tasks 13, 15, 21).

2. **Keying too strictly makes the cache worthless; too loosely makes it
   dangerous.** `CARGO_PKG_VERSION` and model mtime in the key mean every
   release and every reinstall discards everything.
   Mitigation: GGUF content hash plus an explicit state-format version, and
   nothing else that changes for reasons unrelated to state layout (Tasks 13–14).

3. **Restore is already known not to be bit-exact,** and the default model is a
   recurrent/hybrid architecture whose partial state the current truncation API
   cannot roll back. Persistence turns a transient drift into a sticky one.
   Mitigation: fix the truncation with `state_seq_*` + `PARTIAL_ONLY` before
   persisting completed-turn checkpoints (Task 25); make the degenerate-output
   guard delete the disk entry (Task 21).

4. **Privacy — a KV blob is a transcript the user cannot read, search, or think
   to delete.** Today, closing the daemon erases it.
   Mitigation: ADR first, 0600, `cache_dir`, finite retention, opt-out that
   creates no file, and inclusion in the existing clear-history control
   (Tasks 12, 16, 22).

5. **Write amplification.** Two checkpoints per turn at ~28 MB each is ~56 MB
   per turn; a hundred turns a day is 5.6 GB/day. Tolerable on TLC, less so on
   a small QLC drive, and disastrous if done synchronously on the model mutex.
   Mitigation: write-back on eviction only, content-addressed skip-if-exists,
   niced background writer (Tasks 18–19).

6. **Redundancy.** Every snapshot is standalone and embeds the shared head, so
   fifty conversations over a 1000-token head duplicate ~900 MB of identical
   prefix. Compression will not rescue this and a compressor is a net-new
   dependency.
   Mitigation: accept it and size the budget accordingly — disk is cheap and
   the alternative (delta storage) is infeasible with this API. Revisit only if
   sequence-scoped state (Task 25) makes real sharing possible.

7. **Building the disk tier while the prefix keeps breaking.** The largest
   risk: shipping all of Stage 3 and observing no improvement, because misses
   were never about capacity.
   Mitigation: the Task 5 gate. Do not start Stage 3 until the data says
   capacity dominates.

8. **Head-of-line blocking is unaffected.** The LLM server spawns unbounded
   connection tasks (`crates/fono-net/src/llm_server/mod.rs:293`) that all queue
   on one model mutex (`crates/fono-assistant/src/llama_local.rs:903`). A
   persistent cache makes each request cheaper but does not parallelise
   anything.
   Mitigation: out of scope here, but a semaphore sized to 1 would at least make
   the queue explicit and let callers fail fast.

## Alternative Approaches

1. **Do Stage 1 only, and stop.** Raise the entry cap, expose the budget,
   instrument the misses. Captures most of the "more heads" ask for a fraction
   of the effort and no new durable state. This is the recommended starting
   point regardless of what follows.

2. **Stage 2 only — fix prefix stability, skip disk entirely.** If the miss
   taxonomy shows divergence dominating, this is strictly higher value: a stable
   prefix makes the *existing* in-memory cache work, and makes any future disk
   tier worth building.

3. **Delta tree with parent/child KV chains.** Would eliminate the head
   duplication. Rejected: `llama_copy_state_data` cannot emit a partial state,
   and chaining re-introduces the eviction-invalidates-descendants problem the
   current design deliberately avoided.

4. **Multi-sequence single context** (`with_n_seq_max` + per-conversation
   sequence ids). Real sharing of one KV arena across heads, and it is the
   supported path on the default model's architecture. Larger change; total live
   tokens across all heads must fit inside one `n_ctx`. Best considered after
   Task 25 establishes whether the sequence APIs behave.

5. **Server-side conversation ids.** Would give cheap lookup and per-conversation
   quotas. Rejected as the primary mechanism: content addressing survives
   client-side edits, works for the id-less HTTP surface, dedupes across
   conversations, and needs no protocol change. A `thread_id` already exists in
   SQLite (`crates/fono-core/src/conversations.rs:173`) and is deliberately
   walled off from the model; keep it that way, but consider it as an *eviction
   quota* hint only.
