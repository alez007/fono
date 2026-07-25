# Voice-Triggered Actions — v3

Supersedes `plans/2026-07-22-voice-actions-via-mcp-v2.md`, which supersedes
`plans/2026-05-22-voice-actions-via-mcp-v1.md`. The v2 spine survives
unchanged: **Alternative A** (the configured assistant LLM does its own
native function calling, no router), a `fono-action` crate owning
registry + dispatcher + policy, MCP as the connector, built-ins before
MCP, zero new external crates. One spine item is amended by measurement:
transport is **not** streamable-HTTP-first — HA 2026.7 serves HTTP+SSE, so
SSE is mandatory (see F8 below).

v3 changes four things, in order of importance:

1. **A caching correction.** v2's headline local-latency lever — prompt-cache
   the tool catalogue into the reserved `AssistantTools` checkpoint — cannot
   work as written. That slot is dead by construction. The catalogue must
   live inside the leading system block instead. There is also a
   pre-existing cache-invalidation defect upstream of it that must be fixed
   first or the lever measures as zero.
2. **Measurement moves before commitment.** Local tool calling is a release
   requirement gated on a bench that has already failed once. v3 runs that
   measurement as Phase 0.5, against the *real* Home Assistant catalogue,
   before `fono-action` exists.
3. **A user-owned shortcut layer replaces a project-owned fast path.** Fono
   cannot know its users' languages, phrasings, devices, or MCP servers.
   Shortcuts are therefore learned from successful turns and stored
   per-phrase, many-to-one against an action.
4. **Simplification.** One config block, not three. `[assistant.router]`
   removed. Needle demoted to a footnote. Vision reduced to a capability
   gate.

## Objective

Hold F8, say "turn on the kitchen lights" → the lights come on and Fono
confirms out loud. Say "start a 25-minute pomodoro" → a timer runs
in-process, shows in the tray, and notifies on expiry. Questions keep
streaming prose exactly as today.

Three properties are non-negotiable:

- **Local-first parity.** The same commands work with the embedded local
  model, entirely on the LAN, at perceived latency comparable to cloud.
- **Multilingual by construction.** Nothing in the design encodes a
  language list. A user typically mixes English with their native
  language; both must work equally well, and so must a language nobody on
  the project speaks.
- **Fast and reliable for the commands a user actually repeats.** Habitual
  commands should feel instant and never misfire, without the user
  configuring anything.

Home Assistant is the proving ground. Every user's device set differs, so
the design must not assume a catalogue shape — it must measure the one in
front of it.

## What already exists (do NOT rebuild)

Unchanged from v2 `:30-64`, which remains accurate:

- Tool-calling wire layer for the OpenAI-compat family: streamed
  `tool_calls` accumulation at
  `crates/fono-assistant/src/openai_compat_chat.rs:854-902`, descriptor
  builder at `crates/fono-assistant/src/openai_compat_chat.rs:300-318`,
  in-client round-trip at
  `crates/fono-assistant/src/openai_compat_chat.rs:522-617`.
- Trait-level types: `ToolCall` at
  `crates/fono-assistant/src/history.rs:46-51`, `TokenDelta.tool_event`
  and `ToolEvent::{Called, Result}` at
  `crates/fono-assistant/src/traits.rs:54-94`.
- Daemon orchestration and ToolEvent logging at
  `crates/fono/src/assistant.rs:641-648`, `:908-926`.
- JSON-RPC / MCP wire types and stdio framing in
  `crates/fono-mcp-server/src/protocol.rs` and
  `crates/fono-mcp-server/src/transport.rs:34-70`.
- `SseBuffer` at `crates/fono-assistant/src/sse.rs:28-97`.
- Secrets idiom: `*_ref` keys via `crates/fono-core/src/secrets.rs:54-59`.
- Notifications at `crates/fono-core/src/notify.rs:28-71`; tray dynamic
  labels at `crates/fono-tray/src/lib.rs:63-117`.

Additionally, and newly relevant:

- **SQLite is already unconditional.** `rusqlite` with `bundled` is a
  non-optional dependency of `fono-core` (`Cargo.toml:96-97`,
  `crates/fono-core/Cargo.toml:68`). A new store is net-zero on binary
  size and needs no sign-off. The bounded-bucket counter idiom to copy is
  documented at `crates/fono-core/src/api_keys.rs:19-23`.
- **Speaker identity is already resolved per turn** — it builds the
  identity prefix at `crates/fono/src/session.rs:3262-3265`.

## Decision deltas vs v2

### D1 — The tool catalogue goes in the system prefix, not `AssistantTools`

v2 `:267-269` proposes caching the catalogue into the "reserved"
`AssistantTools` checkpoint at
`crates/fono-assistant/src/llama_local.rs:1591-1594`. That layer is
**dead work today**: it is pinned and prefilled at startup, but it is
deliberately excluded from the live prefix-match candidate list — the
candidates are only `[F8ChatPrefix, F8System]` at
`crates/fono-assistant/src/llama_local.rs:474-478`, with the reason stated
at `crates/fono-assistant/src/llama_local.rs:466-467`: it is prefilled as
a bare unframed string from position 0, so it is not a token prefix of any
real prompt and can never be restored. A catalogue placed there inherits
that fate and the lever measures as zero.

For KV reuse the catalogue must be **textually contiguous with the system
prompt, inside the leading system block, before any history turn and
before the user text** — part of the string `assistant_base_prefix` wraps
at `crates/fono-assistant/src/llama_local.rs:1964-1974`:

- Gemma: `<start_of_turn>user\n{system}{TOOLS}`
- ChatML: `<|im_start|>system\n{system}{TOOLS}`

Four preconditions:

- **Identical string on both sides.** Warmup composes from
  `config.assistant.prompt_main` alone (`crates/fono/src/session.rs:214`,
  `:2853-2860`, `:2929-2936`) while the live turn uses
  `ctx.system_prompt`. If tools reach only one, `F8System` becomes
  unreachable and every first turn cold-prefills.
- **Deterministic serialization.** `serde_json` is BTreeMap-ordered (no
  `preserve_order`, `Cargo.toml:63`) so schemas are stable, but MCP
  `tools/list` returns server-defined order. The registry must sort by
  tool name before rendering.
- **Boundary on a newline or control token.** `assistant_base_prefix` ends
  mid-prose with no trailing newline
  (`crates/fono-assistant/src/llama_local.rs:1970-1972`) and
  `build_prompt_prefix_cache` applies `trim_end()`
  (`crates/fono-assistant/src/llama_local.rs:702`). A mid-word boundary
  lets BPE merge the last base token with the first catalogue token and
  silently fail `starts_with` — the class of bug documented at
  `crates/fono-assistant/src/llama_local.rs:601-618`.
- **Nothing volatile upstream** — see D2.

### D2 — Prerequisite: the speaker-prefix cache divergence

On any speaker-verified turn the daemon prepends
`"The current speaker is {name}.\n\n{prompt_main}"`
(`crates/fono/src/session.rs:3262-3265`), while both the startup warmup
(`crates/fono/src/session.rs:214`) and the hotkey-time prepare use bare
`prompt_main`. The pinned `F8System` base therefore diverges from the live
prompt at roughly token one and the whole turn cold-prefills — **today,
independent of voice actions**. A catalogue appended after the system
prompt sits downstream of that line and inherits the invalidation.

This is a standalone defect and should be fixed on its own merits; it is a
hard prerequisite for D1 being measurable. The trace tell is
`llm.prompt_cache_cold_prefill` with `reason="no_prefix_match"`
(`crates/fono-assistant/src/llama_local.rs:1635-1642`).

**FIXED (2026-07-25).** The composition moved into a single documented
helper, `assistant_system_prompt` (`crates/fono/src/session.rs:233-237`),
which **appends** the identity note — `{prompt_main}\n\nThe current
speaker is {name}.` — so `prompt_main` keeps leading and the pinned
`F8System` checkpoint stays a genuine token prefix. `prompt_main` is
`trim_end()`-ed before the join so a trailing newline in the configured
prompt cannot produce a double blank line and shift the boundary. This is
the same shape the F7 polish path already uses for its per-app context,
and the `\n\n` join preserves the newline boundary D1 requires.

Two regression guards, because the original defect was silent — nothing
errors when a pin stops matching, it just costs a full prefill:

- `crates/fono/src/session.rs:5961-5983` — asserts the composed prompt
  starts with `prompt_main` for the speaker case, and covers the
  trailing-whitespace case.
- `crates/fono-assistant/src/llama_local.rs:2329-2369` — asserts through
  the real `build_prompt_split` for both Gemma and ChatML that appended
  decoration keeps the pinned base leading, **and** asserts the negative:
  prepended decoration breaks it. The pre-existing invariant test
  (`:2286-2327`) only exercised an undecorated system prompt, which is
  exactly why the defect shipped unnoticed.

The same guard covers the tool catalogue, since D1 appends it in the same
position — a future refactor that reintroduces prepending fails the test
instead of silently regressing latency.

### D3 — Catalogue overflow is not retired, it moved

v2 `:76-80` retires the catalogue-overflow risk because HA exposes a
handful of Assist *intent* tools rather than one tool per entity. True of
tool count; the **entity inventory** still scales with the user's device
count, is unbounded, and `docs/bench/README.md:96-108` already warns that
prompt size — not model quality — is what makes local tool use appear
10–20× slower.

Primary control is **HA-side Assist exposure**, not a Fono-side filter:
that is where users already manage this, it applies to all their
assistants, and it is the safety boundary v2 already documents at
`:334-336`. Fono's v1 job is to *report* the counts and warn. The
client-side prefilter hook (v2 `:156`) stays a seam, populated only if
Phase 0.5 proves HA-side pruning insufficient.

### D4 — Shortcuts are user-owned data, learned per phrase

A project-owned deterministic command grammar would require owning every
language × every device naming convention × every unseen MCP server.
Instead: a shortcut maps a *phrase* to an *action* (a tool plus frozen
arguments), and phrases are learned from turns the assistant already
resolved correctly.

This dissolves the multilingual and word-order problem in the data model
rather than in an algorithm — "turn on kitchen lights", "turn on the
lights in the kitchen", and "pornește luminile în bucătărie" are three
phrase rows pointing at one action — which is precisely why **no fuzzy
matching is needed anywhere**.

Promotion is automatic and unconditional (no config knob), governed by two
rules:

- The same normalised phrase resolved to the same action, successfully,
  **at least twice**.
- That phrase has **never** resolved to a different action. If it ever
  does, it is marked ambiguous and permanently barred from automatic
  promotion (manual addition still allowed). This is the guardrail against
  frozen context — "turn on the light" meaning the office lamp in the
  office and the kitchen lamp in the kitchen.

`ToolCapability::Dangerous` is never promoted.

**Scope limit, measured (see F11).** Home Assistant's tool surface has no
`entity_id` — `HassTurnOn` takes only `name`/`area`/`floor`/`domain`, and
`GetLiveContext` never shows an ID. So a frozen argument set cannot single
out one of two entities that share both name and area. Shortcuts remove the
*model's* uncertainty (consistent, instant resolution of any phrasing in
any language); they do **not** disambiguate genuinely duplicate device
names. That fix is HA-side renaming or aliasing.

### D5 — One config block

```toml
[assistant.tools]
enabled = true
shortcuts = true

[[assistant.tools.mcp]]
name = "home_assistant"
transport = "http"
url = "..."
auth_token_ref = "..."
```

`[assistant.router]` (v2 `:331-333`) is **removed**. Its sole purpose was
parse-and-ignore forward-compatibility, but blocks with serde defaults are
additive by construction — that is already the house pattern
(`crates/fono-core/src/config.rs:1980-1991`). The seam bought nothing and
created a documented dead surface. Shortcut rules are learned state in
SQLite, not config, so they need no block of their own.

### D6 — Vision is a capability gate

`fono_screen` is registered only when the active assistant model
advertises vision. Otherwise it is absent from the catalogue and the model
cannot reach for something it cannot do. No local mmproj plumbing, no
escalation machinery — a `supports_vision` flag per backend/model plus a
`fono doctor` line.

### D7 — Needle is a footnote

v2 `:104-114` makes cactus-compute/needle the named local-latency
fallback. Upstream reality: a Simple Attention Network — 12-layer encoder
(GQA+RoPE, no FFN) plus an 8-layer decoder with cross-attention, JAX
`.pkl` checkpoints, production runtime is Cactus. Shipping it means a
hand-rolled JAX→ONNX export of an encoder-decoder graph with a decoder KV
cache, a probable minimal-runtime rebuild (ReDimNet2 precedent), and a new
autoregressive decode loop in Rust. Decisively, upstream's own guidance is
"at least 120 examples per tool" and the product surface is a finetuning
playground — it does not zero-shot generalise to catalogues we cannot see,
which is exactly this situation.

If a small dispatcher is ever needed, **llama.cpp-native 270M–600M
candidates cost nothing extra to try** (no export, no runtime rebuild, no
new decode loop, GBNF for free). Needle only earns its engineering delta
if those miss the bar, and even then only for a fixed built-in catalogue
Fono could finetune once.

### D8 — ACL: one signature now, zero implementation

`ConfirmationPolicy` (v2 `:158-159`) is already the right enforcement
point; an ACL is another implementation of it and `Decision::Deny` simply
stops being unreachable. But a policy can only enforce per-user rules if
it knows the user, so the trait takes the resolved speaker from day one:

```
ConfirmationPolicy::decide(&ToolCall, &ToolSpec, Option<&SpeakerId>) -> Decision
```

`NoOpPolicy` ignores it. Threading it now costs nothing; adding it later
is a trait-breaking change across every call site. Combined with
per-speaker invocation counters (Phase 1), the data an ACL needs
accumulates from the start.

## Phases

### Phase 0 — ADRs and bookkeeping

- [x] Resolve the ADR number collision: v2 `:139` asks for 0038 but
      `docs/decisions/0038-inbound-api-key-auth-and-usage.md` exists.
      Use **0029**, which is free and already cross-referenced by
      `docs/decisions/0030-fono-as-mcp-server-for-coding-agents.md:145-149`.
      Confirmed free against `docs/decisions/` — 0029 is the only gap in
      0001–0038.
- [x] ADR 0029 "Voice-triggered actions" —
      `docs/decisions/0029-voice-triggered-actions.md`. Records Alternative
      A, the catalogue-in-system-prefix caching decision including the
      rejection of the dead `AssistantTools` slot (D1), phrase-centric
      user-owned shortcuts with the ambiguity guardrail (D4), the single
      config block (D5), vision as a capability gate (D6), speaker-aware
      policy signature and `Dangerous` deny-by-default (D8), the
      HA-exposure-as-boundary stance (D3), templated confirmations as a
      correctness fix (D7), measure-before-commit, and the Needle
      rejection. Also folds in the recon findings (2317 entities, zero
      aliases, six unresolvable collisions) as the justification for D4
      being a mechanism rather than an optimisation.
- [x] Fix the stale ROADMAP pointer (was `plans/2026-05-22-…-v1.md`) →
      now points at v3 plus the ADR. Corrected the provider claim: the old
      text asserted tool calling "works on OpenAI, Anthropic, Groq,
      Cerebras, and Gemini" with local "later". Verified false —
      `crates/fono-assistant/src/openai_compat_chat.rs:504-509` sends a
      `tools` array **only** for `fono_screen` under `prefer_vision`, and
      `crates/fono-assistant/src/anthropic_chat.rs` never sends one at all
      (it only *reads* `tool_calls` back out of rolling history at
      `:147-151`). Rewritten to promise cloud and on-device together, with
      measurement first. Also added the learn-by-promotion shortcut story
      in user-facing language.
- [x] CHANGELOG `[Unreleased]` entry for the D2 prompt-cache fix, written
      as a user-visible behaviour change (first reply no longer slower
      when voice recognition identifies the speaker) rather than as cache
      mechanics.

### Phase 0.5 — Measure before committing (blocking gate on architecture)

The riskiest item in the plan is local tool calling, it is a release
requirement, and it has already failed once (50–67% pass, 7.6–8.7 s p50 on
gemma-4-12b, 2026-07-04, v2 `:98-103`). The rig to measure it exists
today: the bench (`crates/fono-bench/src/assistant_tool_use.rs`) plus the
local OpenAI-compatible server (ADR 0036). This phase costs days and
determines whether the single-model architecture holds.

**Retrieve the real catalogue first.** No Fono code required — a
throwaway script suffices.

- [x] Confirm the Home Assistant version and which MCP endpoint it
      serves. **Done 2026-07-25: HA 2026.7.3, 436 components.** The
      `mcp_server` integration was NOT enabled; it has since been enabled
      via the config-flow API (`llm_hass_api = ["assist"]`, entry
      `01KYC7R8VXX85JZ6J0KHV8Q6ZG`, additive and reversible from Settings
      → Devices & services). Recon scripts: `tmp/ha-recon/recon.py`,
      `mcp_probe.py`, `enable_mcp.py`.
- [x] Mint a long-lived access token; perform `initialize` → `tools/list`
      by hand and dump the raw JSON. Record tool count and total schema
      byte size. **Done — see F8/F9 below.** 26 tools, 9,835 B sorted
      compact ≈ 2,459 tokens. Raw dump: `tmp/ha-recon/tools_list.json`,
      script `tmp/ha-recon/mcp_tools.py`.
- [x] Call `GetLiveContext`; dump it. Record entity count and inventory
      byte size. **Done twice, by two independent routes.** The websocket
      exposure list (`homeassistant/expose_entity/list`) gave the exposed
      set before `mcp_server` existed; the real `GetLiveContext` call now
      confirms what the model is actually shown: 155 entities, 18,980 B
      ≈ 4,745 tokens. Dump: `tmp/ha-recon/live_context.txt`, analysis
      `tmp/ha-recon/analyse_context.py`.
- [ ] **Exposure hygiene pass, in HA:** Settings → Voice assistants →
      Expose. Concrete list in the findings below; `tmp/ha-recon/
      reliability-audit.json` field `recommend_unexpose` is the actionable
      set. Also set **aliases** on the survivors — currently **zero**
      entities have any alias, so a Romanian phrase has no HA-side hook at
      all and must be resolved from the English `friendly_name`.
- [ ] **Fix the duplicate spoken names in HA** (findings F3 below). This is
      an accuracy ceiling no model, constraint, or prompt can beat, and it
      includes two locks that both say "FRONT DOOR".
- [ ] Re-dump and record the deltas after pruning. Keep both the pruned and
      unpruned catalogues — the unpruned one is the pathological case the
      measurement needs. Re-run `tmp/ha-recon/mcp_tools.py` and
      `analyse_context.py`; the F8/F9 numbers are the "before" baseline.

#### The MCP transport finding (changes Phase 3 scope)

**HA 2026.7 serves the SSE MCP transport, not streamable HTTP.** Measured:
`POST /mcp_server/mcp` → 404, `POST /mcp_server/sse` → 405. The working
handshake is `GET /mcp_server/sse`, which emits an `event: endpoint`
carrying `/mcp_server/messages/<session>`; JSON-RPC requests are POSTed
there and **the responses arrive on the SSE stream**, not on the POST
reply. Server identifies as `home-assistant 1.26.0`, protocol
`2025-03-26`.

This contradicts v2 `:68-75`, which assumed streamable HTTP for HA
2025.2+, and it changes Phase 3: **SSE is not optional**. Fono's MCP
client needs SSE as a first-class transport or the proving ground does not
work at all. Reference implementation to port from:
`tmp/ha-recon/mcp_tools.py` (~110 lines of client).

#### Findings, 2026-07-25 (live instance, 2317 entities)

Raw dumps and scripts in `tmp/ha-recon/` (gitignored). Numbers below are
the durable record.

**F1 — The inventory is the cost, and it is large.** 2317 entities exist;
181 are exposed to Assist. Rendered as `entity_id(name,area)` that is
10,775 chars ≈ **3,078 tokens on every single prompt**, before any tool
schema. Pruning to the recommended set gives 4,127 chars ≈ **1,179
tokens, a 62% reduction**. This confirms D3 empirically: tool count is
irrelevant, entity count is everything.

| tier | n | chars | ~tokens |
|---|---:|---:|---:|
| current exposed | 181 | 10,775 | 3,078 |
| actionable only (drop sensors) | 101 | 5,465 | 1,561 |
| recommended (also drop machinery) | 76 | 4,127 | 1,179 |
| lights + covers + climate only | 54 | 2,981 | 851 |

**F2 — 44% of the exposed set is read-only telemetry.** 64 `sensor` + 16
`binary_sensor` = 80 entities, 5,309 chars ≈ 1,516 tokens per prompt.
Dominated by per-plant soil/CO2/humidity sensors (Alocasia, Areca, Citrus,
Anthurium, Hedera, Money tree, …) and PIR temperature readings. These are
never voice-controlled and only rarely voice-queried; `GetLiveContext`
exists precisely so they need not be resident.

**F3 — Duplicate spoken names are the real accuracy ceiling.** 7 colliding
names cover 16 actionable entities, and **6 of those groups are
unresolvable even with area scoping** — same spoken name, same area:

| spoken name | area | entities |
|---|---|---|
| kitchen lights | Kitchen | `light.kitchen_counter`, `light.kitchen_table`, `light.kitchen_top_light` |
| dark room lights | Basement | `light.dark_room_lights`, `switch.dark_room_lights_2`, `switch.dark_room_lights_3` (cross-domain) |
| couch | Living room | `light.couch`, `light.couch_2` |
| guest bedroom lights | Guest bedroom | `light.guest_bedroom_lights` ("…lights 2"), `light.guest_bedroom_lights_2` ("…lights 1") — inverted |
| living square | Living room | `light.living_square`, `light.living_square_2` |
| basement lights | Basement | `switch.basement_lights`, `switch.basement_lights_2` |
| front door | Entrance / Hallway | `lock.front_door`, `lock.front_door_2` — **both named "FRONT DOOR"** |

Two consequences for this plan:

- **The canonical example phrase in the objective — "turn on the kitchen
  lights" — is ambiguous on the real instance.** Three entities, one name,
  one area. The correct resolution is an *area-level* `HassTurnOn`
  (`area: Kitchen`, `domain: light`), not an entity pick. Area-level
  targeting is therefore the **primary** path and entity-level the
  exception, which inverts the emphasis in the v2 bench fixtures where
  both area fixtures also pin an entity id.
- **`fono doctor` must detect and report this** (new Phase 4 task).
  Reporting "6 exposed groups share a spoken name, including 2 locks" turns
  an unexplainable accuracy problem into a 10-minute rename. No amount of
  GBNF, model size, or prompt engineering substitutes.

**F4 — 25 exposed entities are machinery, not voice targets.** Camera
`_autofocus` / `_ir_lamp` / `_wiper` toggles, `switch.rfid`,
`switch.transmission_*`, `switch.adguard_home_query_log`, PIR occupancy
entities masquerading as `light.*_pir_basic` and `fan.*_pir`, and a
`light.life_matrix_status_led`. Fourteen of them have no `friendly_name`
at all, so the model sees a raw entity id.

**F5 — The four scenes need names, not unexposing.** `scene.good_morning`,
`good_night`, `goodbye`, `i_m_back` are ideal voice targets but have no
`friendly_name` and no area. Rename rather than prune.

**F6 — Zero aliases, 16 areas.** No entity has an alias, so HA offers the
model no multilingual hook whatsoever. The 16 areas are well populated and
are the strongest disambiguation tool available. Separately, 11 actionable
entities have no area (all 4 scenes, the 3 kids-bedroom lights, the
vacuum, the todo list) and cannot be reached by area-level commands.

**F7 — This materially strengthens D4, with one limit.** With zero HA
aliases, learned per-phrase shortcuts are the mechanism by which "pornește
luminile în bucătărie" ever resolves deterministically to a specific
action: the user disambiguates a phrase *once* and the resolved arguments
are frozen, so the model never has to re-resolve it. The limit is F11 —
for the six *unresolvable* collisions the frozen arguments are themselves
ambiguous, because HA's tool surface has no entity ID to freeze. Shortcuts
fix phrasing variance; renaming fixes collisions.

#### Findings from the real MCP catalogue (after enabling `mcp_server`)

**F8 — The catalogue is 26 tools / ~2,459 tokens, and it is worth
caching.** Sorted by name and serialized compact (exactly the D1 shape),
`tools/list` is **9,835 B ≈ 2,459 tokens**. That is small enough to keep
resident and far too large to re-prefill per query on a CPU-bound local
model — which is precisely the D1 thesis, now with a number attached.

| tool group | n | bytes | share |
|---|---:|---:|---:|
| media / volume | 10 | 3,790 | 39 % |
| on/off + light + climate + cover | 6 | 2,601 | 26 % |
| `GetLiveContext` | 1 | 964 | 10 % |
| lists / todo | 4 | 990 | 10 % |
| vacuum | 3 | 484 | 5 % |
| timers / broadcast / datetime | 2 | 163 | 2 % |

The largest single entry is `GetLiveContext` at 964 B (10 %), which is the
one tool that pays for itself. **Media and volume tools are 39 % of the
catalogue for 4 media players** — the first thing to try if the catalogue
ever needs trimming, and a concrete use for the client-side prefilter seam
if Phase 0.5 shows it is needed.

**F9 — Inlining the inventory would triple the cached prefix.** The real
`GetLiveContext` returns **155 entities, 18,980 B ≈ 4,745 tokens** — 1.9×
the catalogue itself. Resident cost would be:

| resident content | bytes | ~tokens | vs catalogue |
|---|---:|---:|---:|
| catalogue only | 9,835 | 2,459 | 1.0× |
| catalogue + inventory | 28,815 | 7,204 | **2.9×** |

So **D3's on-demand decision is confirmed by measurement, not taste**:
fetching the inventory through `GetLiveContext` when the model actually
needs to disambiguate keeps the cached prefix at a third of the size it
would otherwise be. Two-thirds of a naive resident prompt would be device
inventory.

Note the inventory is *already* the pruned view — 155 entities out of
2317, so exposure hygiene is doing most of the work before Fono sees
anything. The domain split confirms F2 on live data: **74 of 155 (48 %)
are read-only `sensor` / `binary_sensor`**, still the largest single
reduction available.

| domain | n |
|---|---:|
| sensor | 58 |
| light | 42 |
| switch | 20 |
| binary_sensor | 16 |
| cover | 6 |
| climate | 5 |
| media_player | 4 |
| lock | 2 |
| todo / vacuum | 2 |

**F10 — The collisions are real in what the model actually sees.** Nine
duplicate display names survive into `GetLiveContext`, including
`'Dark room lights'` **three times** and `'FRONT DOOR'` / `'FRONT DOOR
Door'` twice each. This is F3 reconfirmed from the model's own vantage
point rather than inferred from the registry, and it is the accuracy
ceiling that no sampling constraint can lift.

**F11 — There are no entity IDs anywhere in the HA tool surface. This
partially invalidates a D4 claim.** Verified two ways:

- `GetLiveContext` emits only `names` / `domain` / `state` / `areas` /
  `attributes` per entity. **Zero** occurrences of `entity_id`, and zero
  dotted identifiers like `light.kitchen_counter`.
- `HassTurnOn` / `HassTurnOff` accept exactly `name`, `area`, `floor`,
  `domain`, `device_class`. **There is no `entity_id` property.**

Two consequences, both material:

1. **The existing bench fixture models the wrong contract.**
   `tests/fixtures/assistant_tool_use/homeassistant_lights.toml` asserts
   `expected_entity_ids = ["light.kitchen_ceiling"]`, but a model driving
   real HA can never emit an entity ID — it emits `{name: "Kitchen
   ceiling", area: "kitchen"}`. Any real-catalogue fixture must assert on
   **name/area/domain arguments**, and the harness needs to score that
   shape. This is now a Phase 9 bench task, not a cosmetic one.
2. **D4's "the ambiguity stops mattering" claim is too strong.** A
   shortcut freezes *arguments*, and the argument vocabulary itself cannot
   express "this Couch, not that one" when two `light.couch*` entities
   share the name **and** the area. Freezing the arguments makes the
   phrase resolve *consistently* and *instantly* — a real gain — but it
   resolves to whatever set HA matches, which for the six unresolvable
   groups in F3 is still more than one entity. Shortcuts therefore remove
   the *model's* uncertainty, not HA's.

   The only true fix is HA-side renaming or aliasing, which is why the
   exposure-hygiene task above is a correctness prerequisite and not
   merely a latency optimisation. D4 stands as a mechanism for
   multilingual and word-order variation; it does **not** stand as a fix
   for duplicate names.

**Fix the prerequisite.**

- [x] Fix the speaker-prefix divergence (D2) — done by appending rather
      than prepending the identity note, in the new
      `assistant_system_prompt` helper, with regression guards in both
      crates. See D2 for detail. Static invariant proven by test.
- [x] **Runtime trace confirmation (done 2026-07-25).** New live test
      `appended_speaker_note_restores_pinned_f8_system`
      (`crates/fono-assistant/src/llama_local.rs`, `#[ignore]`, gated on
      `FONO_TEST_ASSISTANT_GGUF`) pins `F8System` from bare `prompt_main`
      via the real `prewarm_prompt_caches`, then runs a decorated
      (speaker-note appended) `reply_stream` and asserts through the turn
      trace that the pin is **restored**, not cold-prefilled. Ran against
      `gemma-4-26B-A4B-it-asym` (ctx 8192): `prompt_cache_prefix_match`
      `matched_layer=f8_system`, **20 of 38 tokens restored** (the full
      `prompt_main` base, ~4.5 MB KV, restore 17 ms); only the 8-token
      appended note + 10-token user text prefilled; **zero
      `cold_prefill`**. Confirms the caching fix is real at runtime, not
      just as the string invariant.

**Run the matrix.** Axes: model × catalogue size × levers.

- [ ] Models, embedded local, from the registry
      (`crates/fono-polish/src/registry.rs:21-72`):
      `gemma-4-e2b` (current default, 3.2 GB), `qwen3.5-0.8b` (528 MB),
      `qwen3.5-2b` (1.27 GB); one llama.cpp-native tool-tuned small
      candidate in the 270M–600M class (FunctionGemma-class) as the
      dispatcher probe.
- [ ] Models, large asym MoE (streamed from SSD, `n_gpu_layers = 0`, mmap
      on, `GGML_CPU_REPACK=OFF` per `.cargo/config.toml:193`):
      `gemma-4-26b-a4b-it-asym`
      (`bogdan-radulescu/gemma-4-26B-A4B-it-asym-GGUF`, 9.6 GB) and
      `qwen3.6-35b-a3b-asym`
      (`bogdan-radulescu/qwen3.6-35B-A3B-asym-GGUF`, 11.7 GB). These are
      the "capable local" tier and the case where the caching lever
      matters most — paying prefill on a streamed MoE is the worst
      possible outcome.
- [ ] One cloud row (Groq or Cerebras) as the accuracy/latency ceiling.
- [ ] Catalogue axis: pruned real catalogue, unpruned real catalogue.
- [ ] Lever axis: GBNF constrained sampling on/off; catalogue-in-system-
      prefix on/off; templated confirmation on/off.
- [ ] Record per row: pass rate, first-turn latency, confirmation-turn
      latency, prefill token count, **and whether the `F8System` pin hit
      or cold-prefilled**. The last is the number that says whether the
      caching design is real.
- [ ] Record the **checkpoint byte size per model**. The prompt cache
      defaults to 8 entries / 256 MiB
      (`crates/fono-core/src/prompt_cache.rs:205-209`) and pinned entries
      are skipped by eviction (`:372-392`) — for the large MoE models at
      ctx 8192 the pins alone may approach or exceed the budget, which
      would need the budget to scale with KV size.
- [ ] Record the hardware tier alongside each run (RAM, core count, disk
      type), since local capability varies by machine and the wizard's
      recommendations depend on it. `machine_label` exists in the report
      envelope but is free text.
- [ ] **Memo with a go/no-go:** does a general local path reach the gate
      on at least one model per hardware tier? If yes, the single-model
      architecture holds and Phase 6 is unnecessary. If no, Phase 6 opens
      as a bake-off.

Verification: the memo exists, is committed, and names a target model per
hardware tier.

### Phase 1 — `fono-action` crate: types, registry, built-ins, shortcuts store

- [ ] Create `crates/fono-action` (SPDX headers; `deny.toml` unchanged;
      **zero new external crates** — serde, serde_json, tokio, reqwest,
      rusqlite are all in-graph).
- [ ] `ToolSpec { name, description, input_schema, capability, provider }`
      with `ToolCapability::{StateRead, StateWrite, Action, Dangerous}`;
      `ToolResult`; `ToolError`. Reuse `fono_assistant::ToolCall`, moving
      it into `fono-action` and re-exporting if the dependency direction
      demands it.
- [ ] `ToolRegistry` with `register` / `list_for_turn`, a no-op prefilter
      hook (D3), and **deterministic ordering — sorted by tool name** on
      render, since MCP servers return arbitrary order and the KV cache
      key is the rendered string (D1).
- [ ] `Dispatcher` trait + `DefaultDispatcher::execute(&ToolCall)`.
- [ ] `ConfirmationPolicy::decide(&ToolCall, &ToolSpec, Option<&SpeakerId>)`
      (D8) with `Decision::{Allow, Deny, RequireConfirmation}`.
      `NoOpPolicy` allows `StateRead`/`StateWrite`/`Action` and
      **denies `Dangerous` unless explicitly enabled** — cheap, and it
      keeps Fono-as-client consistent with the rule Fono-as-server already
      imposes on coding agents.
- [ ] Built-ins: `timer_start`, `pomodoro_start`, `timer_cancel`,
      `timer_status`. Tokio task in the daemon, expiry via
      `notify::send`, state queryable over a new IPC `Request` variant.
- [ ] Tray countdown row via the existing provider-closure pattern
      ("Pomodoro — 17:32 · click to cancel"; ksni has no badge). Escape
      cancels an active timer only when the overlay/assistant is idle.
- [ ] **Shortcuts store** — new `actions.sqlite` under `data_dir`, its own
      DB following the store-separation posture documented at
      `crates/fono-core/src/paths.rs:130-149`; declare the path accessor
      first, as the reserved `notes.sqlite` does. Copy `ApiKeyStore`'s
      `busy_timeout(5 s)` (`crates/fono-core/src/api_keys.rs:89-90`) since
      daemon, CLI, and web settings all touch it. Schema:

      ```
      actions(id PK, tool_name, args_json, label, enabled, created_at)
      phrases(id PK, action_id FK, phrase_norm UNIQUE, phrase_raw,
              lang_hint, source, enabled, hits, last_hit_at,
              consecutive_failures, created_at)
      phrase_invocations(phrase_id, speaker_id, count, last_at)  -- composite PK
      candidates(phrase_norm PK, action_key, seen_count, last_seen_at, ambiguous)
      tool_stats(tool_name PK, calls, failures, last_used_at, mean_latency_ms)
      ```

      Bounded pre-aggregation only — **no event log anywhere**. `candidates`
      capped at ~50 rows LRU by `last_seen_at`; `phrases` soft-capped at
      ~500 with LRU eviction of never-reused rows as a runaway guard.
- [ ] Phrase normalisation: lowercase, collapse whitespace, strip trailing
      punctuation, and **fold diacritics for matching while storing raw**.
      Folding is close to mandatory rather than cosmetic — STT output for
      Romanian and similar languages varies on diacritics between models,
      and "pornește" / "porneste" must hit the same row.
- [ ] Promotion logic (D4): observe successful single-tool turns into
      `candidates`; promote at `seen_count >= 2`; mark `ambiguous` and bar
      automatic promotion permanently if the same phrase ever resolves to a
      different action; never promote `Dangerous`; never promote
      multi-call turns or turns involving a screenshot.
- [ ] Lookup: exact match on `phrase_norm`. Hit → dispatch directly, speak
      the templated confirmation, no model involved. Miss → silent
      fall-through to the LLM. The fast path can never make a turn worse,
      only faster.
- [ ] Staleness and self-healing: if a phrase's tool leaves the catalogue,
      mark stale and skip; after N consecutive dispatch failures,
      auto-disable and surface it.
- [ ] Unit tests: registry routing and deterministic ordering, dispatch
      errors, missing tool, timer lifecycle with `tokio::time::pause`,
      promotion at threshold, ambiguity disqualification, diacritic
      folding, stale-tool skip, failure auto-disable, cap/eviction.

Verification: `cargo test -p fono-action` green.

### Phase 2 — Generalize the assistant tool loop (OpenAI-compat)

- [ ] `AssistantContext` gains `tools: Vec<ToolSpec>` and
      `tool_executor: Option<Arc<dyn ToolExecutor>>`, mirroring
      `screen_capture: ScreenCaptureFn`
      (`crates/fono-assistant/src/traits.rs:100-138`).
- [ ] Replace `build_screen_tool()` special-casing: serialize the full
      sorted tool list; `fono_screen` becomes a registry entry whose
      executor closes over the capture fn, registered **only when the
      active model advertises vision** (D6). Image-content continuation
      stays the one special content shape.
- [ ] Scalar accumulator → `Vec<ToolCallAccumulator>`
      (`crates/fono-assistant/src/openai_compat_chat.rs:785-787`); bounded
      multi-iteration loop with tools attached on continuations, hard cap
      4, then a forced text turn.
- [ ] Emit `ToolEvent::Called/Result` per call; extend the history
      write-back triplet to N calls.
- [ ] Cancellation: `select!` the executor future against the existing
      cancel channel; Escape/barge-in aborts an in-flight dispatch.
- [ ] **Templated confirmations for `Action` tools become the default**,
      cloud included, with a knob to force full model phrasing. This is a
      correctness fix as much as a latency one: a deterministic string
      cannot claim success when the call returned `entity_unavailable`.
      It is also what makes shortcut hits speak without a model.
- [ ] Record `tool_stats` and feed `candidates` on every successful
      dispatch.
- [ ] Daemon wiring: build registry + dispatcher + policy at session
      construction; attach to F8 turns only; F7 dictation never sees
      tools; `[assistant.tools].enabled` master toggle (default off until
      Phase 4, flipped on at release).
- [ ] Integration tests: mock executor → call → result → spoken
      continuation; multi-call turn; iteration cap; shortcut hit bypasses
      the model entirely.

Verification: manual on Groq/Cerebras — "start a five minute timer" works
end-to-end with no MCP configured, and repeating it twice produces a
learned shortcut that fires instantly on the third.

### Phase 3 — MCP client (HTTP+SSE, streamable HTTP, stdio)

- [ ] Widen derives on `crates/fono-mcp-server/src/protocol.rs` types
      (Serialize on `ClientMessage`/`InitializeParams`/`ToolCallParams`;
      Deserialize on `InitializeResult`/`ToolsListResult`/`ToolDef`/
      `ToolCallResult`/`ContentBlock`); round-trip tests. Extract
      `fono-mcp-protocol` only if a dependency cycle forces it.
- [ ] `fono-action/src/mcp/http.rs` — streamable HTTP: POST-per-message,
      `Accept: application/json, text/event-stream`, Bearer via
      `auth_token_ref` + `Secrets::resolve`, SSE responses by promoting
      `SseBuffer` to `pub` (or moving it to `fono-http`). Stateless first;
      `Mcp-Session-Id` seam stubbed.
- [ ] `fono-action/src/mcp/sse.rs` — **legacy HTTP+SSE transport, required
      for the proving ground** (F8): `GET <base>/sse` held open for the
      session lifetime, read the `endpoint` event to learn the
      session-scoped POST URL, POST requests there, correlate responses
      arriving on the *stream* (not the POST body, which is 202). Auto-detect
      per server: try streamable HTTP, fall back to SSE on 405.
- [ ] `fono-action/src/mcp/stdio.rs` — `tokio::process` child, piped
      stdio, stderr to `tracing::warn!`, reap on drop with 2 s kill
      escalation.
- [ ] `McpClient`: initialize handshake advertising protocol `2025-06-18`,
      `tools/list` import into the registry (namespaced
      `home_assistant.HassTurnOn` on collision), `tools/call` with
      per-call timeout, pending-id correlation map.
- [ ] Catalogue versioning: hash the rendered catalogue string; changing
      it invalidates the cached system-prefix checkpoint rather than
      silently mismatching (D1).
- [ ] Config: repeatable `[[assistant.tools.mcp]]` following the
      `#[serde(default)]` + skip-if-default pattern in
      `crates/fono-core/src/config.rs`.
- [ ] Failure isolation: a dead server logs, marks itself down, and never
      poisons built-ins, shortcuts, or other servers.
- [ ] Integration tests: Python-stub MCP server over stdio (skipped
      without `python3`); HTTP against an in-process hyper stub.

Verification: against the live HA instance — `fono doctor` shows the
server, tool count, entity count, and "turn off the desk lamp"
round-trips.

### Phase 4 — UX: shortcuts UI, wizard, doctor, tray, CLI, docs

- [ ] **Shortcuts page in web settings, grouped by action** — this is the
      primary UX surface and it must be excellent:

      ```
      Kitchen lights on          home_assistant.HassTurnOn
        → light.kitchen_ceiling                   [disable] [delete]
        Invoked 47 times · last 2 hours ago
        Bogdan 41 · Ana 6

        Phrases
          "turn on kitchen lights"            EN  31  2h ago     [x]
          "turn on the lights in the kitchen" EN   9  yesterday  [x]
          "pornește luminile în bucătărie"    RO   7  3d ago     [x]
          + add phrase

      Recently learned (2)                              [undo all]
      Stale (1) — the tool this points to no longer exists
      ```

      Per-phrase counts and delete, per-speaker attribution, manual
      "add phrase" so a second household member can register their own
      wording or language against an existing action without waiting to be
      learned, and a "recently learned" section with one-click undo.
- [ ] `fono doctor` "Actions:" section: enabled flag, per-server status,
      **tool count and entity count with a warning above a threshold plus
      a pointer to HA's exposure page** (D3), shortcut count, last
      handshake error, and whether vision is available.
- [ ] **`fono doctor` ambiguity check (finding F3).** For the exposed
      catalogue, detect and report: (a) entities sharing a normalised
      spoken name — flagged **critical** when they also share an area,
      since nothing can disambiguate them; (b) actionable entities with no
      area, which area-level commands cannot reach; (c) entities with no
      friendly name, where the model sees a raw entity id. On the live test
      instance this finds 6 critical groups including two locks both named
      "FRONT DOOR". Prior art for the detection logic: `tmp/ha-recon/
      audit.py`. This is the highest-value diagnostic in the whole feature
      — it converts "the assistant is unreliable" into a short rename list.
- [ ] `fono use actions on|off`; tray "Actions" submenu (master toggle,
      per-server status rows).
- [ ] CLI: `fono shortcuts list|delete` for headless users.
- [ ] Wizard step, cloud and local assistant lanes: "Enable voice actions
      via Home Assistant?" → URL + long-lived token, handshake validated
      before save, token to `secrets.toml`. Recommend a model per hardware
      tier per the Phase 0.5 memo.
- [ ] `docs/providers.md` "Voice actions": HA `mcp_server` setup, **the
      exposure-pruning recommendation and why it matters for latency**,
      alias advice, token generation, and two privacy notes — tool and
      entity names travel to a cloud assistant, and per-speaker
      attribution records which identified human triggered which action
      (with the off switch).

### Phase 5 — Embedded local model tool calling (LAN-only, gated)

Reuses the identical `AssistantContext` tools/executor seam. Still a
release requirement; its content is now shaped by the Phase 0.5 memo.

- [ ] Per-family tool prompt templates in
      `crates/fono-assistant/src/llama_local.rs` (Qwen `<tool_call>`,
      Gemma JSON) fed from the sorted `ToolSpec` list.
- [ ] Stop dropping `ChatRole::Tool` turns
      (`crates/fono-assistant/src/llama_local.rs:1913`, `:1941`) — a local
      model currently cannot observe tool outcomes at all.
- [ ] **Catalogue in the system prefix** per D1: render into the string
      `assistant_base_prefix` wraps, use the identical composition in
      warmup and live paths, land the checkpoint boundary on a newline or
      control token, and verify via trace that `F8System` hits on a
      tools-attached turn.
- [ ] Add a deterministic tie-break to `find_longest_prefix`
      (`crates/fono-core/src/prompt_cache.rs:357`) — prefer pinned, then a
      stable key order. Equal-length candidates currently resolve by
      HashMap iteration order, which becomes a coin flip once the
      catalogue makes ties likely.
- [ ] Scale the prompt-cache byte budget with model KV size if Phase 0.5
      showed the large MoE pins approaching the 256 MiB default.
- [ ] **GBNF / JSON-schema-constrained sampling** in
      `crates/fono-core/src/llama_gen.rs:85-89` when tools are attached.
      **Named risk:** Fono links `llama-cpp-2` with
      `default-features = false` to drop `common/` (`Cargo.toml:119`).
      Grammar *sampling* is in core `llama.h`, but
      `json-schema-to-grammar` lives in `common/` — so this needs a small
      Rust JSON-schema→GBNF converter. Bounded (flat objects, string /
      number / enum) but it is a task, not a footnote.
- [ ] Gate, committing the bench run alongside: `fono-bench
      assistant-tool-use` against the **real pruned catalogue**, at least
      two languages, **≥ 90% pass and p50 end-to-end ≤ ~2.5 s** on the
      reference machine for the Phase 0.5 target model, beating the
      50–67% / 7.6–8.7 s baseline. Report the pruned-catalogue result as
      the gate and the unpruned one as documented degradation.

Verification: gate met; a full "turn on the kitchen light" turn
round-trips with the embedded model and no packet leaves the LAN, verified
by capture.

### Phase 6 — Small-dispatcher bake-off (only if Phase 5 misses the gate)

Opens only on a Phase 0.5 or Phase 5 miss. Measurement-only; memo before
any shipping code.

- [ ] Evaluate **llama.cpp-native candidates first** (FunctionGemma-270m
      class, Qwen3-0.6B class): zero export work, zero runtime rebuild,
      GBNF for free, a registry entry away from shippable. Measure
      zero-shot accuracy on the real catalogue plus the built-in timer
      tools, and tool-decision latency.
- [ ] Design the escalate-vs-dispatch convention: an utterance that is not
      a tool call must hand off cleanly to the full assistant.
- [ ] Only if all llama.cpp candidates miss: probe Needle (D7) — ONNX
      exportability of the encoder-decoder graph via the pinned
      `tmp/venv`, op-union diff against
      `../fono-voice/onnxruntime/ops.config`, and an honest estimate of
      the Rust decode loop plus the per-catalogue finetuning pipeline it
      implies.
- [ ] Memo: ship a dispatcher tier (introducing its config block then, not
      speculatively now), or reject.

### Phase 7 — Realtime (Gemini Live) parity

- [ ] `RealtimeEvent::ToolCallRequested` plus a tool-result submission
      channel on the session; extend the existing `functionDeclarations`
      setup at `crates/fono-assistant/src/gemini_live.rs:188-200` to
      include registry tools alongside `end_conversation`. The seam is
      already reserved at `crates/fono-assistant/src/traits.rs:200-204`.
- [ ] Daemon realtime loop dispatches through the same
      `Arc<dyn Dispatcher>`; policy and shortcuts shared.
- [ ] May ship one release after Phases 1–4 if verification drags.

### Phase 8 — Anthropic native tool_use (droppable to fast-follow)

- [ ] Serialize `ToolSpec` to the Anthropic `tools` shape; parse
      `content_block` `tool_use` and `input_json_delta` accumulation in
      `crates/fono-assistant/src/anthropic_chat.rs:242-252`, which ignores
      it today; `tool_result` continuations; same executor and dispatcher.

### Phase 9 — Benchmark and release engineering

- [ ] Bench: generate `homeassistant` fixtures from the **real exposed
      entity list** rather than the 5 synthetic lights in
      `tests/fixtures/assistant_tool_use/homeassistant_lights.toml`, and
      support an inventory-size axis.
- [ ] Bench: **make area-level targeting the primary expectation** per
      finding F3. The real instance has three entities named "Kitchen
      lights" in the Kitchen area, so the canonical phrase resolves
      correctly only as an area-level `HassTurnOn`. Add fixtures whose
      `expected_area` is set with **no** `expected_entity_ids`, which also
      closes the untested area-only branch of `correct_target` at
      `crates/fono-bench/src/assistant_tool_use.rs:385-393`.
- [ ] Bench: add genuinely-ambiguous fixtures drawn from the real
      collisions ("turn on the couch", "unlock the front door") where the
      correct behaviour is to **ask**, not to guess — and note that
      `lock.front_door` / `lock.front_door_2` is why `Dangerous` defaults
      to deny.
- [ ] Bench: **expand the negative set well beyond 4 of 12** — knowledge
      questions, dictation-like utterances, screenshot requests. False
      activation already carries the heaviest penalty (0.55); the fixture
      mix should reflect that.
- [ ] Bench: add timer/pomodoro fixtures and a shortcut-hit-rate metric.
- [ ] Bench: fix the zero-filled `second_turn_latency_ms` rows — the timer
      starts unconditionally at
      `crates/fono-bench/src/assistant_tool_use.rs:219` even when no
      second HTTP call happens, so confirmation percentiles currently mix
      real turns with no-ops. Phase 5's central claim is about removing
      that turn, so the metric must be interpretable.
- [ ] Bench: report cache hit/cold-prefill per row, and treat the language
      axis as open-ended rather than an EN/RO pair.
- [ ] Pre-commit gate plus `./tests/check.sh --size-budget` (expected
      growth ≈ tens of KB; SQLite already paid for).
- [ ] CHANGELOG section, ROADMAP move to Shipped, version bump.

## Verification criteria

- "Turn on/off <light>" works against live HA via the negotiated MCP
  endpoint with a long-lived token, spoken confirmation included.
- "Start a pomodoro / 5-minute timer" works with **no** MCP server
  configured; tray countdown visible; notification on expiry; Escape or
  tray click cancels.
- **Shortcut learning:** repeating a successful command twice produces a
  shortcut with no user action; the third invocation dispatches without
  the model and speaks the templated confirmation. A phrase that has
  resolved to two different targets is never auto-promoted.
- **Multilingual:** three phrases in two languages pointing at one action
  all fire, and the settings page shows them grouped with independent
  counts and speaker attribution.
- **Local/LAN parity:** the Phase 5 gate is met on the real pruned
  catalogue with zero WAN packets.
- Q&A turns are byte-identical in behaviour with `[assistant.tools]`
  disabled; F7 dictation never sees tools.
- `fono doctor` reports server health, tool count, entity count with a
  warning when large, and vision availability; a dead server degrades
  rather than crashing.
- `Dangerous`-capability tools are refused unless explicitly enabled.
- Size budget green; zero new external crates.

## Risks

| Risk | Mitigation |
|---|---|
| The catalogue-in-system-prefix cache silently fails to match (boundary merge, string divergence, unsorted tools) | Three named preconditions in D1; trace-verified `F8System` hit as an explicit Phase 5 checkbox; deterministic sort in the registry; catalogue hash for invalidation. |
| The speaker-prefix divergence is not fixed and every measurement reads "cold" | D2 is a blocking Phase 0.5 prerequisite, verified by trace before the matrix runs. |
| Entity-inventory size dominates local prefill and is user-dependent and unbounded | HA-side exposure as the primary control; `fono doctor` reports counts and warns; both pruned and unpruned catalogues measured; prefilter hook available if measurement demands it. |
| A learned shortcut freezes context-dependent arguments | Ambiguity disqualification (D4); wrong shortcuts are audible because the confirmation is spoken; "recently learned" with one-click undo; failure auto-disable. |
| Shortcuts table grows unbounded | Bounded pre-aggregation only, no event log; `candidates` capped at ~50 LRU; `phrases` soft-capped at ~500 with LRU eviction of never-reused rows. |
| Small local models emit malformed or wrong tool calls | GBNF-constrained sampling with the `json-schema-to-grammar` gap named; the bench gate on the real catalogue; Phase 6 bake-off if it misses. |
| Local confirmation turn dominates latency | Templated confirmations default on for `Action` tools; shortcut hits skip the model entirely. |
| Large MoE checkpoints exceed the prompt-cache budget and pins cannot be evicted | Phase 0.5 records checkpoint byte size per model; budget scales with KV size if needed. |
| Generalizing the screen-tool loop regresses `fono_screen` | It becomes a registry tool covered by the existing bench and integration tests before any MCP code lands; vision gating means it is simply absent where unsupported. |
| Multi-iteration loop runaway | Hard cap 4, tools stripped on the forced final turn, per-call timeout, Escape cancels in-flight dispatch. |
| HA MCP spec still evolving | Version confirmed empirically in Phase 0.5; pinned protocol rev in the handshake; graceful degradation; doctor surfaces the negotiated version. |
| Per-speaker attribution is a privacy escalation | Documented in `docs/providers.md` with an off switch; own DB at `0600`, consistent with `speakers.sqlite`. |
| Cloud assistant sees entity names | Documented; exposure list is the control; wizard shows a one-line privacy note. |

## Alternative approaches considered and rejected

1. **Project-owned deterministic command grammar** (a phrase→intent matcher
   shipped by Fono). Rejected: requires owning every language × device
   naming convention × unseen MCP server. The learned per-phrase store
   achieves the same latency with none of the maintenance and works in
   languages nobody on the project speaks.
2. **A separate small dispatcher model as the default architecture** (two
   models: tiny router plus conversational). Rejected as a default because
   it doubles RAM, download, and failure modes for a benefit that is
   unmeasured. Kept as Phase 6, opened only by a measured miss.
3. **Needle as the named fallback** (v2's position). Rejected as a lead
   option per D7 — llama.cpp-native small models cost nothing to try and
   Needle's own docs make clear it expects per-toolset finetuning.
4. **Fono-side entity filtering as the primary control.** Rejected for v1
   per D3 — HA's exposure page is where users already manage this and it
   applies to all their assistants. Fono reports; HA controls.
5. **Local vision plumbing** so screenshot analysis works with embedded
   models. Rejected as scope: a `supports_vision` capability gate is one
   flag and makes the limitation legible instead of mysterious.
6. **`[assistant.router]` as a parse-and-ignore seam** (v2's position).
   Rejected per D5 — serde-default blocks are additive by construction, so
   the seam bought nothing and created a dead documented surface.

## Out of scope

- ACL rules themselves. Only the speaker-aware policy signature (D8) and
  the per-speaker counters land now.
- Confirmation UX beyond `NoOpPolicy` plus `Dangerous` deny-by-default.
- OAuth to Home Assistant; long-lived tokens only.
- Fuzzy or semantic phrase matching. Exact normalised match is sufficient
  precisely because phrases are many-to-one against an action.
- Persisting assistant conversation history — `ChatTurn::at` is an
  `Instant` (`crates/fono-assistant/src/history.rs:57-59`) and changing it
  ripples through every provider serialiser.
- GitHub / calendar / other MCP servers: they *work* through the generic
  transports but get no wizard lane or docs walkthrough.

## Status

- **2026-07-25** — v3 drafted. Awaiting sign-off before code lands.
  Supersedes v2 with the caching correction (D1), its prerequisite fix
  (D2), measurement-before-commitment (Phase 0.5, including real-catalogue
  retrieval and the large asym MoE tier), user-owned phrase-centric
  shortcuts with unconditional promotion (D4), single config block (D5),
  vision as a capability gate (D6), Needle demoted (D7), and the
  speaker-aware policy signature (D8).
