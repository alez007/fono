# Voice-Triggered Actions — v4 (architecture-first)

**Date:** 2026-07-26
**Revised:** 2026-07-27 — F25–F27 added from a cloud-backend trace; §8, §9,
§13 and §15 changed as a result. F28–F30 added from a local-backend
(`gemma-4-e2b`) trace: the wording pass was cold-prefilling every tool turn.
**Status:** signed off. Phases 0, 0.5, 1 and 4b are **complete**; Phase 3 is
substantially complete (the §7 ladder ships, including partial-outcome
reporting) but still owes no-op detection (§7.2) and a permanent home for the
live house test (§12.1); Phases 2, 4, 5 and 6 are open.
**Supersedes:** `plans/2026-07-25-voice-actions-v3.md` decisions **D3** and
**D5**. Everything else in v3 (D1, D2, D4, D6–D8, and the crate/phase
skeleton) is **inherited unchanged** and not restated here — read v3 first.

Why a v4: v3 was written before Phase 0.5 produced numbers. The numbers
invalidate v3's assumption that prompt size is a *reporting* problem
(D3) and expose a whole latency class v3 never modelled. v4 is
deliberately **architecture-first and model-agnostic**: FunctionGemma,
the 26B/35B MoEs, and whatever ships next are interchangeable parts
behind a fixed contract. The contract is the deliverable; the model is a
config value.

---

## 1. The measured baseline (Phase 0.5, complete)

Rig: llama.cpp `llama-server` (the engine Fono embeds), CPU-only,
`--jinja`, 8 threads, 16k ctx, real HA catalogue (26 tools, 155
entities), 28 fixtures (16 positive / 12 negative), EN + RO.
Raw data: `tmp/ha-recon/bench/results/`, summary
`tmp/ha-recon/summarize.py`.

| model | arm | EN pass | RO pass | p50 |
|---|---|---|---|---|
| qwen3.6-35b-a3b | catalogue-only | **0.94** | 0.75 | 8.5 s |
| qwen3.6-35b-a3b | catalogue-inlined | 0.81 | 0.75 | 12.6 s |
| gemma-4-26b-a4b | catalogue-only | 0.81 | **0.83** | 5.9 s |
| gemma-4-26b-a4b | catalogue-inlined | 0.56 | 0.58 | **131 s** |
| gemma-4-e2b | catalogue-only | 0.31 | 0.25 | 2.2 s |
| qwen3.5-2b | catalogue-only | 0.31 | 0.17 | 3.6 s |
| qwen3.5-0.8b | catalogue-only | 0.44 | 0.17 | 3.5 s |

**F13 — inlining the inventory is strictly worse.** More context made
models *both* slower and less accurate (attention dilution across 155
mostly-irrelevant entities). catalogue-only is settled; D1 stands.

**F14 — models ≤ 2 B fail on capability**, 0.17–0.44. No prompt work
rescues them as *general* tool callers. They are only viable as
*specialists* (§4, Tier 1).

**F15 — the bench p50s are warm-cache numbers.** 27/28 fixtures hit the
prompt cache (median tokens actually evaluated: **20**) because all 28
shared one system prefix. Real first-use has no such luxury.

**F16 — live end-to-end, real house** (`tmp/ha-recon/live_light_test.py`,
qwen3.6-35b, real HA):

| command | outcome | wall clock |
|---|---|---|
| "turn on the kitchen lights" | 3 lights **ON** | **113 s** |
| "turn off the kitchen lights" | 3 lights **OFF** | 37 s |
| "aprinde luminile din bucătărie" | nothing happened | 53 s |
| "stinge luminile din bucătărie" | nothing happened | 62 s |

Peak RSS **12,257 MB**. English works end to end. The 113 s → 37 s drop
is the cold-vs-warm catalogue prefill, confirming F15 from the other
side. **12 GB / 7 tok-per-sec / 113 s is not a laptop profile** — this
is the gap v4 exists to close.

**F17 — the RO failure is ours, not the model's.** The model emitted
`area: "bucătărie"`; all 13 HA areas are English-named (`Kitchen`,
`Garage`, …). HA matched nothing. The model then reported the failure
honestly and guessed the cause. Fixture-level RO "failures" are the same
class: 2 of 3 were a *missing `domain` filter* (right tool, right area,
over-broad), 1 was verbosity on a correctly-untooled question. **Zero
wrong tools, zero false triggers in RO.** The strict conjunctive scorer
(`crates/fono-bench/src/assistant_tool_use.rs:491-595`) makes discipline
problems read as capability problems.

**F18 — and this is the load-bearing finding.** HA returned a
**success-shaped envelope** for the failed Romanian calls. Naive
"the RPC returned OK" success detection would have recorded two
successful runs and **promoted a command that never worked into a
deterministic shortcut** (v3 D4). Verification quality therefore *gates*
learning — see §6 and §7.

**F19 — reasoning mode is the single largest latency lever, and it makes
quality *worse*.** Measured on qwen3.5-2b, identical warm request
(`tmp/ha-recon/probe_grammar.py` plus follow-up probe):

| | wall clock | completion tokens | result |
|---|---|---|---|
| reasoning **on** (default) | 6.39 s | 173 | correct call |
| reasoning **off** | **1.59 s** | **30** | correct call |

**4× faster for the same decision.** ~85 % of output tokens were thinking
— exactly the §2 information argument. Worse: under a JSON-schema
constraint the reasoning actively *derailed* the model — it argued itself
out of the task ("I'm a text-based AI, I can't control lights") and
emitted garbage (`{"tool":"none","area":"text-based","domain":"assistant"}`).
Reasoning is a liability for a ~12-bit decision. **Fono must explicitly
disable it** (`enable_thinking: false` / `--reasoning-budget 0`), never
inherit the model default.

**F20 — grammar constraint does not make tokens cheaper.** Verified in the
llama.cpp source we link: no forced-token fast-forward exists in
`llama-grammar.cpp` / `llama-sampling.cpp` — the grammar masks logits
*after* a full forward pass. The batched-acceptance idea is therefore
**not available in our runtime**. Grammar still helps, but only by
reducing the token *count* (no preamble, no rambling), never the
per-token cost.

**F21 — correction to F19's scope: the bench was already reasoning-off;
the live test was not.** `crates/fono-bench/src/assistant_tool_use.rs:391`
and `:432` already send `think: false` +
`chat_template_kwargs.enable_thinking: false` on both turns, matching what
Fono ships for local backends
(`crates/fono-assistant/src/openai_compat_chat.rs:706-709`). So the §1
matrix numbers are **already reasoning-off and stand as measured** — an
earlier claim that they were "4× pessimistic" was wrong. It was the
*first* live house run (`tmp/ha-recon/live_light_test.py`, hand-rolled,
no flags) that inherited the server default and paid the 4× penalty.
Lesson recorded as an invariant: **every** probe must send the production
request shape, or it measures the harness rather than Fono.

**F22 — reasoning off, measured on the real house.** Same four commands,
same model (qwen3.6-35b), same catalogue-only setup, one flag changed:

| command | reasoning ON | reasoning OFF |
|---|---|---|
| `turn on the kitchen lights` | 113 s | **7.9 s** |
| `turn off the kitchen lights` | 37 s | 20.8 s |

**14× on the cold first command.** Peak RSS unchanged at 12.3 GB.

**F23 — injecting the real area names fixes Romanian completely.** Adding
one line to the system prompt (`Areas in this home (use these exact
names): …`, 13 names ≈ 30 tokens) changed the model's emitted argument
from `area: "bucătărie"` (F17, matches nothing) to `area: "Kitchen"`.
Both Romanian commands then actuated the real lights: **RO 0/2 → 2/2,
live total 4/4.** This is the cheapest accuracy fix in the plan and it
generalises — the resolution table is a prepaid artefact (§6), not a
per-request cost, and it removes an entire failure class in every
language rather than translating names.

**F24 — with reasoning off the bottleneck flips from decode to prefill.**
Server log breakdown of one warm reasoning-off command:

| | prefill | decode | total |
|---|---|---|---|
| turn 1 (decide) | 15.2 s / 531 tok | 4.6 s / 46 tok | 19.7 s |
| turn 2 (confirm) | 4.0 s / 124 tok | 2.5 s / 25 tok | 6.5 s |

**Prefill is now ~76 % of the decision turn.** Two consequences that
reorder the plan: (a) reducing output token *count* (§2 consequence 2,
grammar) is no longer the main lever — it addresses the smaller half;
(b) **prompt-cache prepay (§6) and catalogue pruning (§5) are the top
levers**, because they attack prefill directly. Note the cache is already
partially working — only 531 of ~2 900 catalogue tokens were evaluated —
so the remaining win is in making the *uncached tail* (system prompt +
decoration + user text) small and stable, which is exactly the prefix
discipline the D2 fix established.

**F25 — the cloud path has a different bottleneck, and "end to end" was the
wrong metric.** One real assistant trace (`gpt-5.4-mini` over
`api.openai.com`, one Romanian light command), instrumented with the new
`actions` trace lane (`tool.execute` / `tool.verify`,
`crates/fono-core/src/turn_trace.rs`, `crates/fono/src/actions/mod.rs`) which
exists because the entire tool round trip was previously **invisible** — an
unexplained gap between two `llm` slices:

| stage | cost |
|---|---|
| STT | 1.07 s |
| **model decides the call** | **10.78 s** |
| tool executes (MCP round trip) | 0.59 s |
| model phrases the reply | 0.67 s |
| TTS | 0.58 s |
| playback | 2.87 s |

Two corrections follow. (a) **Prefill is not the cloud bottleneck.** The
second request carried the same prefix *plus* the whole tool result — a longer
prompt — and completed in 0.67 s. F24's prefill finding is real but
**local-only**; §6 and the payload trim are local levers and must be scoped as
such. (b) **The metric was wrong.** What the user waits for is not the turn:
for a command it is *stop speaking → the device moves* = **12.44 s**, of which
the model is **87 %**; playback and verification both fall *after* it and cost
the user nothing. For a query it is *stop speaking → the answer is audible* =
13.70 s. §13 now states both.

**F26 — a stored reply cannot be honest, which removes a whole planned
distinction.** A replayed shortcut was going to speak the sentence the model
produced when the phrase was learned, and this fails on the exact bug that
prompted the revision: the office command *partly* succeeded (air conditioning
in `success`, the wanted lamp in `failed`). A stored "I turned on the office
light" would be replayed over every future partial failure. Dropping stored
replies costs **nothing on the F25 command metric** — the phrasing call happens
after the device has already moved — and it deletes the need to classify tools
as commands vs queries at all (§8). One path: a shortcut caches *which tool
with which arguments*, never data, never wording.

**F27 — the GBNF grammars §6 specifies were never built, and the runtime
supports them.** `LlamaSampler::grammar_lazy` is present in the pinned
`llama-cpp-2` 0.1.150 (`sampling.rs:329`) — no new dependency, no binary
growth. Nothing in the workspace passes a grammar; the local path instead asks
for `<tool_call>{…}</tool_call>` in the prompt and parses forgivingly
(`crates/fono-assistant/src/local_tools.rs:101`, `:114`), whose own test
comments record the cost: *"the shapes models actually produce when they drift.
Each of these was a light that would otherwise have been read out loud instead
of switched."* Lazy triggering is what makes this shippable — prose generates
unconstrained and the grammar engages only once the model commits to a call, so
ordinary conversation is untouched. **Scope limit: constrain shape, not
values** — see the §14 risk.

**F28 — the wording pass on the local backend cold-prefilled every tool turn,
and the fix was two lines.** Measured on `gemma-4-e2b`, trace
`assistant-1785177665-0005.json`: STT 6.82 s, first model pass 2.0 s (prefill 0.3
s — a 1041-token checkpoint matched), tool 0.11 s, then the second pass spent
**21.65 s** re-reading 974 tokens plus 3.67 s of suffix, for 37.6 s total on one
light. Two independent causes, both in `crates/fono-assistant/src/llama_local.rs`:

1. The continuation re-serialised the parsed call as tidy JSON
   (`{"name": …, "arguments": …}`) while the completed-turn checkpoint was saved
   under the model's *raw* reply (`<tool_call>…</tool_call>`), so the checkpoint
   was not a token prefix of the very next prompt and could never match.
2. The second pass offered the **system** prefix to the cache, and
   `PromptStateCache::find_longest_prefix` only considers entries *shorter* than
   the prefix it is given (`crates/fono-core/src/prompt_cache.rs:354`). The
   1096-token checkpoint was therefore invisible — and worse, inserting it had
   pruned the 1041-token entry that used to match (subsumption,
   `prompt_cache.rs:264`), leaving the 72-token pinned system prompt as the only
   restorable state.

Fixed by continuing from the reply as written and offering *this turn's completed
exchange* as the cached prefix, which makes the checkpoint an exact-key hit.
Pinned by `the_wording_pass_starts_where_the_finished_turn_was_saved`. The
generalisable lesson for Phase 2: **a checkpoint is only reusable if the next
prompt is rendered by the same code path that saved it**, and offering a shallow
prefix can *hide* a deeper one. Also exposes a latent trap worth fixing if it
recurs — pruning a shallower entry is only safe while the deeper entry actually
matches.

**F29 — a failed command left no evidence.** The same trace shows
`tool.execute` with `server_error: true` and nothing else: no arguments, no
server message. The model's spoken excuse (*"it is not a supported device
type"*) was the only clue, and it is the least trustworthy witness available.
`tool.execute` now records the arguments and the server's own words (capped at
300 characters), and refusals are logged at `warn`. Diagnosis before
optimisation: Phase 5 cannot promote what it cannot explain.

**F30 — the static head was read from scratch once per conversation, and the
first turn of a conversation was never checkpointed at all.** Two traces on
`gemma-4-e2b` taken a minute apart, after the F28 fix landed:

- `assistant-1785178455-0002.json` (cold, command refused) — pass 1 prefilled
  **894 tokens in 13.22 s**, pass 2 prefilled **958 tokens in 23.79 s**; 60.8 s
  total. `cold_prefills: 0` and `cache_hits: 2` in the summary, because both
  passes "matched" the **72-token** `f8_system` pin the daemon warms at startup
  and called it a hit.
- `assistant-1785178534-0003.json` (warm, command succeeded) — pass 2 matched a
  1065-token `f8_chat_prefix` checkpoint and prefilled **0.042 s** (F28 confirmed
  fixed in the field), but pass 1 still prefilled **944 tokens in 16.51 s**;
  28.4 s total.

Two distinct defects, both in `crates/fono-assistant/src/llama_local.rs`:

1. **The head was never checkpointed after being read.** The static head —
   system prompt + rooms + devices + tool catalogue, 966 tokens — costs ~13 s to
   read and was then discarded. The only pinned entry was the bare 72-token
   system prompt, pinned at daemon startup *before* the device list and the tool
   catalogue exist. Fix: when a turn pays to prefill its prefix and that prefix
   is the static head (empty history), checkpoint it and **pin** it under
   `F8System`, replacing the useless 72-token pin. The price is then paid once
   per system prompt instead of once per conversation. Carried by
   `GenParams::pin_prefix`, false for the wording pass (whose prefix contains
   that turn's own words and must never displace the head pin).
2. **The first turn of a conversation stored no completed-turn checkpoint.** The
   store was gated on non-empty history, to stop a `fono.summarize` checkpoint
   from pruning the shared prefix. That gate also removed the checkpoint the
   wording pass needs seconds later — which is exactly the 23.79 s in the cold
   trace, and why the warm trace (turn 2, history non-empty) paid 0.042 s for the
   same step. The gate's original justification is void now that the head is
   pinned: `prune_dominated_by` skips pinned entries
   (`crates/fono-core/src/prompt_cache.rs:275`). Fix: store on every turn.

Lesson for Phase 2, and the reason `cold_prefills: 0` must never again be read
as "the cache is working": **the metric that matters is decoded prefix tokens,
not hit count.** A hit on a 72-token pin ahead of a 966-token prefix is a cold
prefill wearing a hit's clothing. Phase 2's warm-path assertion should count
`decoded_prefix_tokens`, not `cold_prefills`.

**F31 — a turn that calls a tool poisons its own checkpoint for the next turn,
and the head fix did not reach turn two.** Two more `gemma-4-e2b` traces, taken
a minute apart, with F30 already landed:

- `assistant-1785180569-0002.json` — pass 1 prefilled **1562 tokens in 28.6 s**;
  47.3 s total. The command was refused by Home Assistant.
- `assistant-1785180630-0003.json` — the *next* turn in the same conversation,
  whose prompt is an **exact character prefix** of nothing less than the
  previous prompt plus one reply. It still prefilled **1599 tokens in 37.6 s**;
  47.5 s total. Both summaries again said `cold_prefills: 0`, both matching only
  the 72-token pin.

The second one is the interesting one: everything the turn needed had been read
and checkpointed one minute earlier, and none of it could be used. Two causes,
compounding, both now fixed in `crates/fono-assistant/src/llama_local.rs` and
`crates/fono-core/src/prompt_cache.rs`:

1. **A completed-turn checkpoint is unusable by the next turn whenever a tool
   was called.** It covers prompt + tool call + tool result + reply, but the
   next turn's history keeps only the *spoken reply* — the call and the result
   are never rendered again. So the checkpoint diverges mid-sequence and can
   never win a prefix match, however deep it is. In trace 0002 the survivors
   were a 1742-token completed-turn entry (divergent) and the 72-token pin.
2. **The prefix a turn reads was pruned inside that same turn.** F30 stored it,
   but under `F8ChatPrefix` — the same layer as the completed-turn checkpoint,
   which is a strict superset of it, so `prune_dominated_by` dropped it seconds
   later. Visible as the `llm.prompt_cache_pruned` at 37.383 in trace 0002. The
   fix that would have saved 1599 tokens deleted itself.

Fix: store the read prefix under a **new `HistoryPrefix` layer** so it is out of
the chat layer's pruning fight, and add that layer to the lookup set. It is the
one checkpoint guaranteed to be a genuine prefix of the next turn, because it
contains nothing of this turn's own output. The layer still self-prunes across
turns, so it holds one entry, not one per turn.

Generalised lesson, and a rule for Phase 2: **a checkpoint is only worth storing
if what follows it in the next prompt is an append.** Anything a turn emits that
history will later drop — tool calls, tool results, preambles — makes every
checkpoint taken after it dead weight. Checkpoint *before* the divergence, not
after.

**F32 — a `null` argument made Home Assistant refuse the command, twice.** Same
trace 0002, now legible thanks to F29's instrumentation:
`HassTurnOff {"area": "Kitchen", "domain": ["light"], "floor": null, "name":
"Kitchen lights"}` → `Input validation error: None is not of type 'string'`.
The model filled in every field the tool advertises, two of them with
placeholders. Nothing was broken and the command was one `null` away from
working; the user repeated themselves and the model apologised each time.

Fix: drop `null`, empty-string and empty-list arguments before the call leaves
Fono (`crates/fono/src/actions/mod.rs`). A key the caller did not mean to set and
a key it left blank are the same request. This never changes what was asked for
— it stops us asking for it badly. Note this is a *shape* error of exactly the
kind F27's grammar work would prevent at the source, and is more evidence for
scoping that to shape rather than values.

---

## 2. Fundamentals: is there a reason this must be slow?

Decompose "user speaks → light changes":

| stage | cost | irreducible? |
|---|---|---|
| STT | existing, optimised | yes |
| **decide** (tool + args) | **6–113 s** | **no** |
| **think about it** (reasoning tokens) | **~4× the decision** | **no — pure waste (F19)** |
| execute (MCP round trip) | 50–200 ms | yes |
| confirm to user | 3.5 s median | **not on the critical path — §8** |

From the server logs: decode runs at **~143–170 ms/token (~7 tok/s)**;
prefill at 68–90 ms/token. So:

> **latency ≈ output_tokens × ~150 ms**

Now the information argument. For "turn on the kitchen lights" the actual
*decision* is: which tool (1 of 26 ≈ 4.7 bits), which area (1 of 13 ≈
3.7 bits), which domain (≈ 3.3 bits) — **~12 bits, under 2 bytes**. We
spend ~30–40 decoded tokens (≈ 5–6 s) emitting that, because the JSON
skeleton (`{"area":"…","domain":["…"]}`) is decoded token-by-token at
full cost. **~85–90 % of decode time carries zero information** — it is
fully determined by the tool's JSON schema, which we already have.

Three consequences, each a fundamental (not incidental) saving:

1. **Do not think about a 12-bit decision.** Reasoning tokens are the
   largest single cost on the action path and buy nothing here (F19/F22):
   4–14× faster with it off, same answer, and materially *better*
   behaviour under constraint. This is a flag, not a research project —
   the cheapest win in the plan, and available today.
   **Scope limit:** this applies to the *action* path only. Fono's own
   OpenAI-compatible server surface (`crates/fono-net/src/llm_server/`)
   is a general-purpose endpoint for third-party clients and MUST NOT
   force reasoning off — it currently does not mention reasoning at all,
   which is the correct behaviour: the client stays in control.
2. **Emit only the decision, not the prose.** Constraint cannot make a
   token cheaper in our runtime (F20), but it caps the token *count*.
   When cost is per-token-linear, count is the only knob that matters.
3. **A repeated command has zero decision entropy.** If we already know
   the answer, the correct number of model calls is **zero**. Floor
   becomes STT + network ≈ 200 ms.

**Conclusion: there is no fundamental reason a repeated light command
should cost more than ~200 ms, or a novel one more than ~1–2 s.** The
current 37–113 s is architecture, not physics.

---

## 3. Design axioms

- **A0 — Prepay everything.** Any work whose inputs are known before the
  user speaks MUST happen before the user speaks. Nothing that can be
  precomputed is allowed on the critical path.
- **A1 — Models are replaceable, the contract is not.** No decision may
  depend on a specific model family. Model choice is config.
- **A2 — Generic over tools.** We do not know what tools users will
  have. No HA-specific logic above the MCP boundary; HA is one source.
- **A3 — Determinism must be earned, and only verified truth counts**
  (F18).
- **A4 — Default on, user deselects.** Discovery must never require
  configuration to work.

---

## 4. The latency ladder (core architecture)

Every request enters at the cheapest tier that can serve it and falls
through only when it cannot. This is the whole design in one table:

| tier | when | model cost | target latency |
|---|---|---|---|
| **0 — replay** | phrase is a learned, verified shortcut | **none** | **~200 ms** (network-bound) |
| **1 — constrained** | novel phrase, warm catalogue, reasoning off, output capped by schema | decision tokens only | **1–2 s** |
| **2 — reasoning** | ambiguous / multi-step / unknown target | full decode (thinking allowed here, and only here) | 5–10 s |

Tier 2 is not a failure mode — it is the **teacher**. Every Tier 2
success that passes verification feeds the shortcut store, so the
long-tail command becomes a Tier 0 command on its third use. The system
gets faster with use, per user, in whatever language they speak.

Tier 0 dissolves the multilingual problem in the data model (v3 D4): the
Romanian phrasing and the English phrasing are two rows pointing at one
action. No fuzzy matching, no translation, no grammar engineering.

---

## 5. A1 — The tool catalogue store (supersedes v3 D3)

v3 D3 held that prompt-size control belongs to the upstream server
(HA-side exposure) and Fono's job is to *report*. F13 kills that: fewer
tools is not only cheaper but **measurably more accurate**, and users
cannot be expected to curate an upstream they may not administer. Fono
owns a persisted, user-curated catalogue.

**Storage: SQLite via `rusqlite`, already in the dependency graph**
(`Cargo.toml:97`, `crates/fono-core/Cargo.toml:68`) — **zero new
dependency, zero binary-size impact**, no sign-off needed. Mirror the
house pattern from `crates/fono-core/src/speakers.rs:177-241` /
`history.rs:76-113`: `open` / `open_in_memory` / `migrate`, WAL,
`CREATE TABLE IF NOT EXISTS`, additive columns probed with
`PRAGMA table_info`.

```sql
CREATE TABLE IF NOT EXISTS tool_source(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,      -- "home_assistant", "builtin"
  transport TEXT NOT NULL,
  last_seen INTEGER
);
CREATE TABLE IF NOT EXISTS tool(
  id INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES tool_source(id),
  name TEXT NOT NULL,
  schema_json TEXT NOT NULL,
  schema_hash TEXT NOT NULL,
  capability TEXT NOT NULL,        -- safe | dangerous
  verify_class TEXT NOT NULL,      -- post_condition | result_contract | none (§7.1)
  readback_tool TEXT,              -- name of the tool that observes this one's effect
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  available INTEGER NOT NULL DEFAULT 1,
  enabled INTEGER NOT NULL DEFAULT 1,   -- A4: default on
  user_touched INTEGER NOT NULL DEFAULT 0,
  UNIQUE(source_id, name)
);
```

**Lifecycle (reconcile, never truncate):**

- new tool → insert `enabled = 1` (A4).
- missing on this sync → `available = 0`. **Never delete, never reset
  `enabled`** — a server restart or a network blip must not silently
  re-enable something the user switched off.
- returns → `available = 1`, `enabled` preserved.
- `schema_hash` changed → update schema, keep the user's `enabled`,
  invalidate the prompt cache (§6).
- `user_touched` separates "on because default" from "on because the
  user said so", so defaults can evolve later without stomping intent.

**Only `enabled AND available` tools are rendered into the prompt.** The
render is canonical — sorted by `(source, name)`, `serde_json`
BTreeMap-ordered — satisfying D1's determinism precondition. Its hash is
the prompt-cache key.

Deselection is therefore simultaneously a **latency** lever (fewer
prefill tokens) and an **accuracy** lever (F13). That is the honest
framing to give users in the UI.

---

## 6. A2 — Prompt-cache contract and the prepay discipline

Layer the system prefix so the longest stable run is pinnable (D1):

```
[ static rules ] [ canonical enabled-tool catalogue ] | [ per-turn: speaker note, user text ]
                 ^-------- pinned KV prefix ---------^
```

The boundary must fall on a newline (D1) and the pinned string must be
byte-identical between warmup and the live turn — the D2 defect class,
now fixed and guarded (`crates/fono/src/session.rs:233-237`,
`crates/fono-assistant/src/llama_local.rs:2329-2369`).

**Prepay triggers (A0).** The catalogue KV prefix is warmed, in the
background, on: daemon start; catalogue sync producing any change;
user enable/disable; model or context-size change. **Never on the
request path.** F16's 113 s → 37 s is exactly the cost of getting this
wrong, and it is a one-time cost we are choosing to pay at the wrong
moment.

Also precomputed at sync time, not request time:
- **GBNF grammars per enabled tool**, compiled from `schema_json`.
- **Target-name resolution tables** (areas, entity names + aliases) —
  the F17 fix. Cheap, and it also lets us inject the 13 real area names
  (~30 tokens) so the model stops inventing translated targets.
- Response templates per locale (§7).

**Cache health must be observable**, not assumed: `fono doctor` reports
pin state, and the existing trace tell
(`llm.prompt_cache_cold_prefill`, `reason="no_prefix_match"`,
`crates/fono-assistant/src/llama_local.rs:1635-1642`) must be zero on a
warm second turn. A silent cache miss is a 3× latency regression with no
error (D2's original sin).

---

## 7. A3 — How we know a function was actually executed

F18 makes this load-bearing, not hygiene. Generic ladder, weakest to
strongest; a tool declares (or we infer) its strategy:

| strategy | evidence | counts as proof? |
|---|---|---|
| `None` | fire-and-forget (e.g. broadcast) | no |
| `Transport` | JSON-RPC returned a result, no error | **no** — F18 |
| `ResultContract` | parsed `isError` / structured failure / known failure text in the payload | yes |
| `PostCondition` | re-read observable state, assert the expected change | **yes, definitive** |

### 7.1 What about tools that are not Home Assistant?

This is the question that decides whether verification is architecture or
a Home-Assistant special case. We do not know users' tools (A2), so
verification must be a *property discovered per tool*, not an assumption.

Two of the three usable rungs are **already generic**:

- `ResultContract` is generic **by protocol**. MCP defines `isError` on
  every tool result, so every MCP server — HA or otherwise — gives us
  this rung for free. It is not a Home Assistant feature.
- `PostCondition` is generic **only when a readback exists**. It requires
  a second tool in the same catalogue that reports the state the action
  changed (`GetLiveContext` for HA; a `read_file` for a filesystem
  server; a `get_*`/`list_*` for many others). Sometimes there is none —
  `send_email`, `broadcast` — and then no amount of engineering makes the
  result observable.

So at **discovery time** (§5) each tool is classified and the class is
stored in the catalogue store alongside `schema_hash`:

| class | when | promotion (§9) | UI wording |
|---|---|---|---|
| `PostCondition` | a readback tool covering this tool's effect was found | auto-promote after 2 verified runs | "done" (proven) |
| `ResultContract` | no readback, but the server reports structured failure | promote only with a higher bar (more verified runs, no failures) | "accepted" |
| `None` | fire-and-forget, no failure signal | **never auto-promote**; explicit user opt-in only | "sent" — never "done" |

Three rules follow, and they are the honest part:

- **Never claim more than the evidence supports.** For a `None`-class
  tool we genuinely do not know it worked. The reply must say "sent",
  not "delivered" — the UI must not launder transport success into
  outcome success. This is the F18 lesson generalised beyond HA.
- **Unverifiable + irreversible must not be silently replayed.** Tier 0
  replay is for actions that are verifiable *or* reversible. An
  unverifiable irreversible action (unlock, send, pay, delete) always
  goes through the normal path, regardless of how often it has been used.
- **Classification is data, not code.** A server that later gains a
  readback tool upgrades on the next reconcile; nothing is hardcoded per
  vendor.

### 7.2 Rules

- `Transport` alone is **never** sufficient. HA proved it returns
  success-shaped envelopes for calls that did nothing (F17/F18).
- `PostCondition` costs one extra round trip (~100 ms) and is the only
  definitive proof for state-changing tools. At Tier 0/1 latencies that
  is affordable; use it for `enabled` state-changing tools.
- **No-op must be distinguished from success.** "Turn on" a light that
  is already on yields no state change: it is *not* a failure, but it is
  *not evidence* that targeting was correct, so it must not count toward
  promotion (§8). **Still owed — and it gates Phase 5.** `confirms`
  deliberately returns `Confirmed` for an already-correct state
  (`crates/fono/src/actions/vendor.rs:189-210`), which is right for *wording*
  (the user asked for a state and it holds) and wrong as *promotion
  evidence*. The two uses must be separated before anything can be promoted.
  Cheapest shape: read the pre-state only when a phrase is already a
  promotion candidate, so the extra round trip is paid once per phrase
  learned rather than on every command.
- Failures must surface the *reason* to the user, not a generic error.
  The model diagnosed F17 better than our code would have; a
  target-not-found error should say so and offer the real names.

---

## 8. A4 — Response: build nothing (dropped)

v4 first proposed three response modes (`off` / `template` / `llm`) with a
selection rule. **Dropped — YAGNI.** Three reasons, in order of weight:

- **The response is not on the critical path.** The tool call fires and
  the light changes *before* the first reply token is decoded. Optimising
  the sentence optimises nothing the user waits for. The latency problem
  is entirely in *deciding* — §2 owns that, and F19 alone beats anything
  response modes could have won.
- **A config knob cannot know.** We do not know users' tools (A2).
  "Is this reply redundant?" depends on the tool's payload, not on a
  global setting — so the setting is wrong for half the catalogue on day
  one, and wronger with every new MCP server. A temperature query must be
  spoken; a light toggle need not be. That is a property of the result,
  not a preference.
- **The existing reply path already works.** Modes would mean a template
  engine, per-locale strings, and slot-filling for payloads we have never
  seen — to save time that was never blocking.

**Decision: the action fires, then the assistant replies exactly as it
does today.** No modes, no templates, no config, no code.

Revisit only on evidence. One plausible trigger exists: the trailing reply
keeps the model busy, so a rapid second command queues behind it. If that
is measured as a real annoyance, the minimal fix is **one** flag to skip
the reply for verified state-changing actions — one boolean, not three
modes and a rule. Not before.

**Reaffirmed 2026-07-27, and extended to Tier 0 replay.** A stored reply —
replaying the sentence the model produced when a shortcut was learned — was
considered and **rejected on correctness, not cost** (F26). It would speak a
success sentence over a later partial failure, which is the failure this plan
exists to prevent. Two consequences:

- **A replay is phrased by the model, from the real result**, exactly as a
  novel command is. Costs nothing the user waits for (F25: the device has
  already moved), and a partial failure narrates itself with no special case.
- **There is therefore no command/query distinction anywhere in the design.**
  An earlier revision split tools into "commands, replay the stored sentence"
  and "queries, must be fresh"; that split existed *only* to decide when a
  stored sentence was safe, and dies with it. Shortcuts cache **routing**
  (which tool, which arguments). Never data, never wording.

---

## 9. A5 — Promotion gated on verification (refines v3 D4)

v3 D4's promotion rule (same normalised phrase → same action,
successfully, ≥ 2 times; never if it ever resolved differently; never
for `Dangerous`) is kept **and tightened**:

- "successfully" now means **`ResultContract` or `PostCondition`
  verified** (§7). `Transport`-only success MUST NOT count. Without this
  clause, F18 shows we would have learned to replay a broken command.
- A verified **no-op** does not count (§7).
- Promotion is invalidated when the action's `schema_hash` changes or its
  tool becomes `available = 0` / `enabled = 0` (§5).
- Tier 0 replay still executes the *real* tool call and still verifies.
  Replay skips the *decision*, never the *verification*.
- **A replay that fails verification hands the turn to the model rather than
  the user.** The failed result is already a sentence the model can act on
  (`crates/fono/src/actions/mod.rs:282-285`), so the fallback is the ordinary
  path with one tool result already in history — no new mechanism. Three
  constraints:
  - **Tools are offered again exactly once**, and only after a *replay*. The
    second turn is deliberately tool-less so a model cannot chain actions
    (`crates/fono-assistant/src/openai_compat_chat.rs:603-608`); this is a
    narrow exception to that rule, not its removal.
  - **Only when the intent is an absolute end state.** Re-running "turn the
    light on" is harmless; re-running "raise the temperature by two degrees"
    is four degrees. Today this holds by accident — verification only exists
    where `desired_state` is `Some` (`crates/fono/src/actions/vendor.rs:218-224`),
    which is absolute by construction — so it is written down here as a rule
    before a vendor with relative commands arrives. Relative intents get the
    honest failure sentence instead.
  - **Execution failure falls through unconditionally.** If the server was
    unreachable or nothing was touched, no effect happened, so there is
    nothing to double.

  Cost when it fires: ~13.0 s to the device moving, against 12.44 s with no
  shortcut at all (F25) — ~0.6 s worse, once, because the same failure demotes
  the shortcut (§9.1). The user never has to repeat themselves.

### 9.1 Demotion — a shortcut is a standing hypothesis, not a fact

Promotion is not permanent. The world changes underneath a learned
shortcut: a device is renamed, an area is restructured, an entity is
removed from exposure, an integration breaks. A shortcut that keeps
firing blind would be *fast and confidently wrong* — strictly worse than
being slow.

So **every replay re-verifies at its tool's class (§7), and the outcome
feeds back into the store**:

- **`PostCondition` class — demote on the first verified failure.** No
  three-strikes grace. A wrong action has already happened in the
  physical world; the correct response is to stop trusting the shortcut
  immediately and fall back to Tier 1/2, which can re-resolve the target
  and tell the user what changed.
- **`ResultContract` class — demote on a reported error.** Same logic,
  weaker evidence.
- **A no-op neither confirms nor demotes** (§7) — it is not evidence in
  either direction.
- **Demotion is a reset, not a ban.** The row keeps its history and drops
  to unpromoted; if the next Tier 1/2 resolution verifies twice again
  (possibly to a *different* action, e.g. after a rename), it re-promotes.
  The system self-heals without user intervention.
- **Structural invalidation is immediate and needs no failure**:
  `schema_hash` change, `available = 0`, or `enabled = 0` demotes on the
  spot (§5).
- **Demotion is observable.** It is a trace event, not a silent state
  flip — the same discipline as the prompt-cache health rule (§6), and
  for the same reason: a silent regression is the one we do not fix.

The payoff is that verification does double duty. The same readback that
gates *learning* also gates *continued trust*, so accuracy does not decay
as the house (or the tool catalogue) evolves.

---

## 10. A6 — The model-agnostic boundary

Everything above is expressed in terms of: a catalogue, a grammar, a
decision, a verification, a response. None of it names a model. What a
model must supply to participate:

| capability | required for | fallback if absent |
|---|---|---|
| OpenAI-style `tool_calls` | Tier 1/2 | adapter (e.g. FunctionGemma's `<start_function_call>` format) |
| grammar/GBNF constraint | Tier 1 fast path | Tier 2 only |
| stable pinnable KV prefix | prepay (§6) | cold prefill, degraded |
| vision | `fono_screen` | tool not registered (v3 D6) |

**FunctionGemma note.** `google/functiongemma-270m-it` is real and
attractive on paper — 551 MB peak RSS, ~126 tok/s decode, 0.3 s TTFT vs
our 12,257 MB / ~7 tok/s — i.e. the laptop profile F16 says we lack. But
it (a) is licensed **"gemma", not OSI**, so per
`docs/decisions/0004-default-models.md` it may be *supported* but
**never a default**; (b) is explicitly a **fine-tuning base**, not a
finished model (BFCL-Live-Simple 36.2; Google's own domain fine-tune
moved a comparable task 58 % → 85 %); (c) emits a **custom call format**
needing an adapter. It is therefore a Tier 1 *candidate*, evaluated
behind the A6 boundary — not an architectural assumption. Same treatment
for the next model that appears.

---

## 11. Config (supersedes v3 D5)

```toml
[assistant.tools]
enabled = true
shortcuts = true            # Tier 0 learning
verify = "postcondition"    # none | transport | result | postcondition
reasoning = false           # F19: 4x faster, better quality. Do not flip.

[[assistant.tools.mcp]]
name = "home_assistant"
transport = "http"
url = "..."
auth_token_ref = "..."
```

Per-tool `enabled` is **not** config — it is user state in SQLite (§5),
because it is discovered data with a lifecycle, not authored
configuration. Same reasoning v3 D5 applied to shortcuts.

---

## 12. Phases (revised)

Phase 0 / 0.5 are **complete** (§1). v3's Phases 1–4 are inherited; the
ordering below reflects that latency architecture now precedes breadth.

- **Phase 1 — catalogue store + reconcile** (§5). `fono-core` SQLite
  store, canonical render, hash, lifecycle. Unit-testable with zero
  model involvement.
- **Phase 2 — prepay + cache contract** (§6). Layered prefix, background
  warm on the four triggers, `fono doctor` pin visibility, assert zero
  cold prefill on warm turns. **Scoped local-only by F25** — a cloud
  backend's prefix is already effectively cached, so this buys nothing
  there. Add one adjacent local lever while here: **trim the tool-result
  payload** to what a reply needs, since the whole house dump is re-prefilled
  on the second turn (`brief()` currently passes 2000 chars through
  verbatim, `crates/fono/src/actions/mod.rs:332-342`). Correctness benefit
  too — less irrelevant JSON is less to misread.
- **Phase 3 — execution verification** (§7). Strategy ladder, no-op
  detection, real error surfacing. Re-run the F16 live test: the RO rows
  must fail *loudly* with the real reason.
- **Phase 4 — Tier 1: reasoning off, then shrink the uncached prefix**
  (§2, §4). Explicitly disable thinking per backend on the action path
  (F19/F22, 4–14×) — already the shipped behaviour for local backends,
  so the work is asserting it and covering the remaining backends with a
  regression test. Then attack prefill, which F24 shows is now ~76 % of
  the decision turn: prune the catalogue (§5) and keep the uncached tail
  small and prefix-stable (§6). Output-count capping via JSON-schema /
  GBNF (F20) comes last — it addresses the smaller half. Target 1–2 s.
  **Grammar scope, per F27:** use `grammar_lazy` triggered on `<tool_call>`
  so prose is unconstrained, and constrain **shape only** — JSON validity,
  the enabled tool names as literals, required arguments present. This
  removes the drift class the `local_tools` tests enumerate. Do **not**
  constrain argument *values* without measuring first (§14).
- **Phase 4b — prepaid target-name resolution** (F23, §6). Inject the
  real area/target names into the warmed prefix. Measured: RO 0/2 → 2/2
  live, ~30 tokens, no per-request cost. Highest accuracy-per-token in
  the plan.
- **Phase 5 — Tier 0 replay, with demotion** (§9, §9.1). Verified
  promotion, continuous re-verification on replay, immediate demotion on
  verified failure, structural invalidation. Target ~200 ms on the third
  utterance. **Depends on Phase 3's no-op detection** (§7.2) — a light that
  was already on must not count as promotion evidence — so that lands first.
  Sliced so each step is testable with no model involvement: (0) no-op
  detection; (1) shortcut store, additive tables in the existing
  `tools.sqlite`, no new database and no new dependency; (2) learning —
  phrase → (tool, arguments, verified outcome); (3) replay — normalise,
  look up, execute, verify, let the model phrase it; (4) demotion and the
  §9 fallthrough; (5) the §11 config keys, which were never added; (6) the
  UI (§15 Q3).
- **Phase 6 — Tier 1 specialist bake-off** (§10). FunctionGemma-class
  candidates behind the A6 boundary, adapter included. The §1 matrix is
  already reasoning-off (F21) and stands as the shortlist baseline.
- Then v3's MCP-breadth, UX, realtime, and release phases.

Each phase is independently shippable and independently measurable
against §13. No phase depends on a specific model.

---

## 12.1 How each phase is checked

Three things, run at every phase boundary. Two of them already exist.

1. **The pre-commit gate** — `cargo fmt`, `cargo clippy -D warnings`,
   `cargo test`, and `./tests/check.sh --size-budget`. Already a hard
   rule in `AGENTS.md`; the only addition here is that the size gate also
   serves as the standing proof that the catalogue store stayed
   zero-dependency (§5).
2. **The live house test** — `tmp/ha-recon/live_light_test.py`, **4/4 or
   it did not work**. It is the only check that covers the whole chain,
   and it already forces a known pre-state so a command that changes
   nothing cannot score a false pass. **Phase 3 owes it a permanent home
   under `tests/`** so it stops being a personal artefact — still outstanding,
   and `tmp/` being pruned would lose it outright.
3. **Say one command out loud.** Everything else bypasses STT. Thirty
   seconds, and it is the only check that the *user's* path works.

Model-in-the-loop assertions (prompt-cache restore actually happening,
bench pass rate vs the §1 baseline) go in `#[ignore]`d tests pointed at a
real GGUF via `FONO_TEST_ASSISTANT_GGUF` — the pattern
`appended_speaker_note_restores_pinned_f8_system` already establishes
(`crates/fono-assistant/src/llama_local.rs`). Too slow for CI, one
command locally, run before a tag.

Two behaviours are worth naming as tests because they are the ones that
fail *silently*, and silence is what makes them expensive:

- **A tool you switched off must not come back on** after the server
  disappears and returns (covered, `tool_catalog.rs`).
- **A promoted shortcut whose target was renamed must demote** rather
  than keep firing fast and wrong (§9.1, owed by Phase 5).

Beyond that: write the tests the code needs. That is not a policy.

---

## 13. Verification criteria

**How latency is measured (F25).** Two numbers, both from the moment the user
stops speaking — not from turn start, and not to turn end:

- **Command latency** — stop speaking → **the device moves**. Verification,
  reply generation, TTS and playback all happen *after* this and are excluded.
  Baseline 12.44 s.
- **Query latency** — stop speaking → **the answer is audible** (playback
  starts). Baseline 13.70 s.

Every target below is one of these two. "End to end" is not used again: it
flattered playback into the budget and hid that the model is 87 % of what the
user actually waits for.

- **Tier 0**: repeated verified command ≤ **2 s** command latency, zero
  model calls before the device moves, still verified. (The old ≤ 300 ms
  "end to end" figure was never achievable — STT alone is 1.07 s.)
- **Tier 1**: novel command p50 ≤ **2.5 s** on CPU, laptop-class RAM.
- **Cache**: zero `prompt_cache_cold_prefill` on any warm turn; first
  post-start turn pays no catalogue prefill (§6).
- **Accuracy**: ≥ 0.90 pass EN **and** RO on the real HA suite, with the
  F17 area-name fix and the `domain`-filter discipline fixed.
- **Live**: all four F16 rows pass against the real house, RO included.
  *(Met once, 4/4, with reasoning off + the F23 area hint — must stay
  green as a regression, at Tier 1 latency rather than 8–30 s.)*
- **Trust decay**: a promoted shortcut whose target is renamed or removed
  demotes on its first verified failure and re-promotes after two fresh
  verified runs, with both transitions visible in the trace (§9.1).
- **Verification**: a call that changes nothing is reported as a failure
  with its real reason, and is never promoted (F18).
- **Fallthrough**: a replay that fails verification finishes the job in the
  same turn without the user speaking again, and demotes the shortcut (§9).
- **Honesty**: a partially-successful command names the devices that did not
  respond, on the replay path as well as the novel path (F26).
- **Memory**: peak RSS within laptop budget for the shipped default
  tier.
- **Size**: `./tests/check.sh --size-budget` green — no new dependency
  (§5).

---

## 14. Risks

- **Tier 0 over-reach.** Replaying a context-dependent phrase ("turn it
  off") would be wrong. Mitigation: v3 D4's ambiguity bar, plus
  never promoting phrases whose resolution varied, plus verification on
  every replay (§9).
- **Constrained decoding fights the chat template.** GBNF plus `--jinja`
  tool syntax may conflict per model family. Mitigation: Tier 1 is
  optional per A6; Tier 2 always works.
- **A value-constrained grammar turns a clean failure into a confident wrong
  action.** Enumerating the real area names in the grammar would make F17's
  `area: "bucătărie"` unemittable — but the model must then pick *some*
  allowed literal, and picking the wrong room switches a real light in it.
  Today an unknown room matches nothing and Fono says so, which is the better
  failure. Mitigation: constrain **shape** only (F27); treat value enumeration
  as a separate, measured decision, and keep the §6 prepaid name list — which
  *informs* the model without removing its ability to decline — as the
  preferred fix for the same problem (F23, already 2/2 on the real house).
- **PostCondition unavailable** for many third-party tools. Mitigation:
  the ladder degrades explicitly, and promotion simply never happens for
  unverifiable tools — correct, if slower.
- **Catalogue churn thrashing the cache.** A flapping MCP server could
  re-warm repeatedly. Mitigation: reconcile on hash, debounce, and
  `available=0` does not change `enabled`.
- **Batched forced-token acceptance is not exposed** by the runtime we
  link (F20, confirmed in source). Mitigation: Tier 1's budget comes from
  reasoning-off, prefill reduction, and token count instead — all
  independent of it.
- **Prefill dominates on CPU** (F24), so a large or unstable prompt tail
  is now the main latency risk. Mitigation: the §6 prefix discipline plus
  catalogue pruning; regression-guard with the same trace assertion the
  D2 fix uses.

---

## 15. Open questions

1. ~~Does the runtime expose batched acceptance of grammar-forced
   tokens?~~ **Answered: no** (F20). Tier 1's win comes from
   reasoning-off plus a smaller token count, not cheaper tokens.
2. `PostCondition` for HA: read back via `GetLiveContext` (simple, ~100 ms,
   coarse) or a narrower state read? Prefer narrower if MCP exposes one.
3. ~~Do we expose Tier 0 shortcuts in the UI as editable rows, or keep them
   invisible learned state with a "forget" button?~~ **Answered
   2026-07-27: visible, on a new `#/actions` route.** Learned behaviour that
   fires real-world devices is not hidden. The route carries the retrieved
   tool list *and* the shortcut rows, including **actions that have worked
   only once and are not yet on the fast path** — each showing last run, who
   said it (speaker identification is already in the pipeline), the
   `tool.execute` / `tool.verify` timings (F25), and verification strength.
   Many phrases may point at one action (different word order, different
   languages) and **user-authored phrases are allowed**, marked as such and
   still executed and verified like any other — a hand-written phrase is not
   trusted more than a learned one. "Tools & actions" in settings keeps the
   servers, the enabled/disabled counts, and a link here.
   Editing is limited to adding phrases and forgetting rows; the
   phrase→action mapping itself is earned by verification, not authored,
   or the verification gate means nothing.
4. Does disabling reasoning cost accuracy on genuinely ambiguous
   multi-step requests? F19 shows no loss on simple commands. If a loss
   appears, reasoning becomes a **Tier 2-only escalation** — off by
   default, enabled only after a Tier 1 failure. Never on by default.
5. ~~How is a readback tool matched to an action tool at discovery time
   (§7.1)?~~ **Answered: naming heuristic, with the result stored as data** —
   shipped as `pick_readback_tool` / `classify`
   (`crates/fono-core/src/tool_catalog.rs:577`, `:601`). An explicit
   per-server mapping would be exact but does not scale to unknown servers;
   storing the guess means a wrong one is a row to correct rather than a code
   change, which is what makes the fuzzy default acceptable.
6. **Promotion on servers we cannot verify.** `for_result` knows one vendor
   (`crates/fono/src/actions/vendor.rs:108-117`); everything else is
   `Unknown`, which by design claims nothing — so on a non-Home-Assistant
   server *nothing would ever be promoted*. Either promote on weaker evidence
   ("no error, N runs") or promote routing only and label those rows
   unverified in the UI. Leaning to the second: it keeps the honesty ladder
   intact, and a fast wrong answer is what this plan buys insurance against.
