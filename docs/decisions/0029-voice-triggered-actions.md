# ADR 0029 — Voice-triggered actions

- **Status:** Accepted
- **Date:** 2026-07-25
- **Plan:** [`plans/2026-07-25-voice-actions-v3.md`](../../plans/2026-07-25-voice-actions-v3.md)
- **Supersedes plans:** [`plans/2026-05-22-voice-actions-via-mcp-v1.md`](../../plans/2026-05-22-voice-actions-via-mcp-v1.md),
  [`plans/2026-07-22-voice-actions-via-mcp-v2.md`](../../plans/2026-07-22-voice-actions-via-mcp-v2.md)
- **Inverse of:** [`ADR 0030`](0030-fono-as-mcp-server-for-coding-agents.md)
  (Fono as MCP *server* for coding agents)

## Context

Fono today converts voice to text. Voice *actions* let the same F8 turn
*do* something instead — turn on a light, start a Pomodoro, file an issue.
The connector is MCP, with Fono as the **client** calling whichever servers
the user configures; Home Assistant on the LAN is the proving ground.

The design pressure comes from five task classes the user expects to work
through one hotkey:

| Class | Example | Needs |
|---|---|---|
| Built-in command | "start a Pomodoro" | no model, instant, always right |
| Device control | "turn off the kitchen light" | small closed catalogue, fast |
| Knowledge Q&A | "why is the sky blue" | a conversational model |
| Visual reasoning | "what is the issue here" | a vision-capable model |
| Grounded lookup | "what is the weather" | network tools plus synthesis |

Two facts constrain any answer. First, **cloud and local are not the same
problem**: the 2026-07-04 bench run measured a Gemma-class local model at
50–67 % pass and 7.6–8.7 s p50 on the *simplest* class in that table, so a
design that works on cloud can still be unusable locally. Second, **local
capability varies enormously by machine**, from a 528 MB dispatcher on a
thin laptop to a 26–35 B asym MoE streamed from SSD on a workstation.

Reconnaissance against the real Home Assistant instance (HA 2026.7.3,
2317 entities) sharpened this further and is recorded in the plan: the
`mcp_server` integration was not even loaded, **zero** entities carried
aliases, and six actionable entities had names that collide
unresolvably. So the practical blocker is not model quality — it is
prompt size and naming ambiguity.

## Decision

### 1. One LLM with native function calling; `fono-action` as registry and dispatcher

Tools are advertised through the model's own function-calling API rather
than through a hand-rolled intent parser or a prompt-embedded DSL. A new
`fono-action` crate owns the tool registry and an `Arc<dyn Dispatcher>`
seam; built-ins land before MCP. No new external crates.

MCP transport: **both** streamable HTTP and HTTP+SSE are required, not
streamable-HTTP-only. Measured against Home Assistant 2026.7.3, the
`mcp_server` integration still serves the legacy HTTP+SSE transport —
`POST /mcp_server/messages` returns 405, and the session is established
by `GET /mcp_server/sse` followed by posts to the session endpoint the
stream advertises. Our proving-ground server therefore does not support
the transport a streamable-HTTP-only client would assume.

**Rejected:** a project-owned command grammar (the Rhasspy/Mycroft
tarpit, and the notion ADR 0011 already left dead). We would have to own
every language × every device-naming convention × every MCP server we
have never seen. Unbounded, and unnecessary once the model does the
resolving.

### 2. The tool catalogue lives inside the leading system block

This is the load-bearing latency decision. The catalogue must be
textually contiguous with the system prompt — inside the leading system
block, before any history turn and before the user text — so it is
covered by the existing `F8System` prompt-cache pin and prefill is paid
**once per catalogue version**, not per query.

**Rejected:** the reserved `AssistantTools` checkpoint that the v2 plan
targeted. That layer is dead by construction: it is prefilled as a bare
unframed string from position 0, so it can never be a token prefix of a
real prompt, and it is deliberately excluded from the live prefix-match
candidate list. Reusing it would have inherited that fate — silently,
because a missed pin does not error, it just costs a full prefill.

Four preconditions follow, each a way this decision fails quietly:
identical catalogue string in the warmup and live paths; tools sorted by
name so the string is stable across runs (MCP `tools/list` order is
server-defined); the checkpoint boundary landing on a newline so BPE
cannot merge across it; and nothing volatile upstream of it.

That last precondition exposed a **pre-existing defect**, fixed as part
of this work: the speaker-identity note was *prepended* to the system
prompt, diverging from the pin at roughly token one and cold-prefilling
every speaker-verified turn. Per-turn decoration is now appended, in one
documented helper, guarded by regression tests in both crates.

### 3. Home Assistant exposure is the boundary; Fono reports, it does not filter

With 2317 entities, inlining an inventory would dominate prefill. The
primary control is HA-side Assist exposure — that is where users already
manage this, and it applies to all their assistants. Fono's job in v1 is
to *report* tool and entity counts and warn above a threshold. The
client-side prefilter stays an unpopulated seam, used only if
measurement proves HA-side pruning insufficient.

Where disambiguation is genuinely needed, the model calls
`GetLiveContext` rather than carrying the inventory resident.

### 4. Shortcuts are user-owned data, learned per phrase

A shortcut maps a *phrase* to an *action* (a tool plus frozen arguments).
Phrases are many-to-one against an action, so "turn on kitchen lights",
"turn on the lights in the kitchen", and "pornește luminile în bucătărie"
are three rows pointing at one action.

This dissolves the multilingual and word-order problem **in the data
model rather than in an algorithm**, which is why no fuzzy matching is
needed — and fuzzy matching is how the wrong light gets turned on.
Matching is exact on the normalised phrase (lowercased, whitespace
collapsed, trailing punctuation stripped, diacritics folded for matching
while the raw form is stored). Diacritic folding is required, not
cosmetic: STT output for Romanian and similar languages varies on
diacritics between models and settings.

Phrases are learned by promotion — a phrase becomes a shortcut once the
assistant has resolved it identically and successfully twice. There is no
`auto_accept` knob. The guardrail that makes unattended promotion safe is
**ambiguity disqualification**: if a phrase is ever observed resolving to
a different tool or different arguments, it is permanently disqualified
from promotion. That is exactly the set of context-dependent phrases
("turn on the light", meaning whichever room I am in), caught without
modelling context.

Storage is five bounded pre-aggregated tables in a separate
`actions.sqlite` — no event log anywhere, so the database is bounded by
user behaviour rather than by time. `rusqlite` is already an
unconditional dependency, so this costs no binary size.

The reconnaissance strengthened this from an optimisation into a
*mechanism*: with zero HA aliases and six unresolvable name collisions,
learned per-phrase shortcuts are how an utterance in any phrasing or
language resolves deterministically — the user disambiguates once, and the
resolved arguments are frozen.

One measured limit: Home Assistant's tool surface exposes **no entity ID**
(`HassTurnOn` takes `name`/`area`/`floor`/`domain`, and the live-context
listing shows names only). A frozen argument set therefore cannot single
out one of two devices sharing both name and area. Shortcuts remove the
*model's* uncertainty; genuinely duplicate device names must be fixed by
renaming or aliasing in Home Assistant.

### 5. One config block

```toml
[assistant.tools]
enabled = true
shortcuts = true
```

**Rejected:** `[assistant.router]` as a parse-and-ignore
forward-compatibility seam. New blocks with serde defaults are additive
by construction — that is already the house pattern — so the seam buys
nothing and creates a documented-but-dead surface users will ask about.
Also rejected: a tiered `[assistant.fastpath]` / `[assistant.dispatcher]`
config surface. Shortcuts are learned state in SQLite, not config.

### 6. Vision is a capability gate

`fono_screen` is registered only when the active model advertises vision.
No local mmproj plumbing, no escalation machinery — a model is simply
never offered a tool it cannot fulfil, and `fono doctor` explains why
screenshots are absent.

### 7. Confirmations are templated by default, cloud included

A deterministic confirmation string is not only a latency win (it removes
the second model turn); it is a **correctness** win, because a template
cannot claim "I turned off the lights" when the call returned
`entity_unavailable`.

### 8. `ConfirmationPolicy` takes the speaker; `Dangerous` denies by default

ACL is not built. But the enforcement point — `ConfirmationPolicy` with
`Decision::{Allow, Deny, RequireConfirmation}` — only *can* enforce
per-user rules if it knows the user, so the speaker is threaded into the
signature now:

```rust
fn decide(&self, call: &ToolCall, spec: &ToolSpec, speaker: Option<&SpeakerId>) -> Decision;
```

`NoOpPolicy` ignores it. This costs nothing today and avoids a
trait-breaking change across every call site later; combined with
per-speaker invocation counts, the data an ACL needs accumulates from day
one.

Separately, `ToolCapability::Dangerous` **denies unless explicitly
enabled**. Fono-as-server already forbids voice-authorising destructive
actions for coding agents; Fono-as-client must not be laxer.

### 9. Measure before committing

Phase 0.5 is a blocking gate: run the existing bench against the real
pruned and unpruned HA catalogues, across the candidate model range
(0.8–2 B local, the current Gemma default, a 270M–600M dispatcher probe,
and the large asym MoE builds), with the levers on and off — *before*
`fono-action` is built. It records whether the `F8System` pin hits or
cold-prefills, which is the number that says whether decision 2 is real.

Capability tiering is treated as a structural certainty implied by the
task table, not as a latency contingency; what measurement decides is
which tiers get populated, not whether tiering exists.

**Rejected as a lead option:** Needle. It is JAX-native with an
encoder-decoder cross-attention graph, requiring a hand-rolled ONNX
export, a new autoregressive decode loop in Rust, and probably a
minimal-runtime rebuild. More decisively, upstream's own guidance is
"at least 120 examples per tool" — it is a finetune-per-toolset model,
not a zero-shot generaliser, and Fono cannot see its users' catalogues.
Demoted to a research footnote behind llama.cpp-native candidates, which
need no export and get GBNF constraint support for free.

## Consequences

- Prefill is paid once per catalogue version rather than per query — the
  decision that matters most for the large MoE models, which stream from
  SSD and are precisely where a cold prefill hurts worst.
- A pre-existing prompt-cache defect is fixed, benefiting every
  speaker-verified turn whether or not voice actions are enabled.
- Frequently used commands become instant and model-independent, in any
  language, without the user writing configuration.
- GBNF-constrained sampling needs a small Rust JSON-schema→GBNF
  converter, because Fono deliberately drops llama.cpp's `common/` for
  binary size and that is where `json-schema-to-grammar` lives. Grammar
  sampling itself is in core, so this is bounded but must be planned.
- Per-speaker attribution records which identified human triggered which
  action. That is a step beyond transcript history, so it needs an
  explicit off switch and a `docs/providers.md` note, consistent with how
  `speakers.sqlite` is already treated.
- Users with large Home Assistant instances must curate Assist exposure
  and are told so, rather than silently getting a slow assistant.

## Relationship to other ADRs

- **ADR 0030** (Fono as MCP server) — inverse direction; shared wire
  types extracted when both sides need them.
- **ADR 0011** (voice commands) — keeps its stance that command grammars
  are not the mechanism; actions ride the assistant turn instead.
- **ADR 0024** (assistant multimodal and search) and **ADR 0031**
  (visual context capture) — the vision capability gate builds on both.
- **ADR 0036** (local LLM server) — the rig Phase 0.5 measures against.
- **ADR 0022** (binary size budget) — zero new external crates;
  `rusqlite` and `serde_json` are already linked.
- **ADR 0004** (default models) — the asym MoE candidates are opt-in
  downloads, not defaults.
