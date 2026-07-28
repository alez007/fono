# Voice-action benchmark — grounded in Fono's own machinery

**Status:** proposal, ready for review. Not started.
**Relates to:** `plans/2026-07-28-voice-actions-universal-first-v5.md` Task 9 ("re-measure
the same four commands and gate Tier 2 on the result") — which today has no harness, and
is the immediate reason this plan exists.
**Extends:** `crates/fono-bench/src/assistant_tool_use.rs` and
`tests/fixtures/assistant_tool_use/homeassistant_lights.toml`.

---

## Objective

Answer three questions repeatably, for any model, in any of four languages, without a
human in the loop and without touching a real house:

1. **Routing** — did the model pick the right tool with the right arguments, first try?
2. **Recovery** — when it did not, did Fono's own ladder rescue it inside the same turn,
   with no second utterance from the user?
3. **Truthfulness and speed** — did the spoken reply honestly describe what happened, in
   the language it was asked in, and how long did each stage take?

The output is a versioned JSON report plus one Chrome-trace file per fixture run, so a
regression is attributable to a stage rather than to "it got worse".

---

## Assessment of the proposal as stated

### What is already built

The idea is largely implemented and this is the single most important thing to know
before writing any code:

- `crates/fono-bench/src/assistant_tool_use.rs:1-1069` is a multi-language, fixture-driven
  tool-use benchmark with per-language mean score, pass rate, and p50/p95 latency split
  across first and second turn.
- It already loads a **real** MCP `tools/list` dump and converts it to OpenAI shape,
  sorted for cache determinism (`crates/fono-bench/src/assistant_tool_use.rs:195-226`), and
  can inline a real `GetLiveContext` device dump as the inventory
  (`crates/fono-bench/src/assistant_tool_use.rs:174-189`).
- It already scores the real Home Assistant argument shape — `name`/`area`/`floor`/`domain`
  with set-aware array matching, which correctly rejects the over-broad
  `["light","shade","shutter"]` call (`crates/fono-bench/src/assistant_tool_use.rs:742-758`).
- It already measures the prompt-cache lever: reported vs *evaluated* prompt tokens, which
  is how KV prefix reuse is detected (`crates/fono-bench/src/assistant_tool_use.rs:481-489`).
- It already has negative fixtures — ambiguity, status questions, explanation requests —
  which are what stop a benchmark from rewarding trigger-happiness
  (`tests/fixtures/assistant_tool_use/homeassistant_lights.toml:103-141`).
- `--provider fake` is a genuine self-test of the harness in both fixture dialects
  (`crates/fono-bench/src/assistant_tool_use.rs:665-705`).

So the work is not "build a benchmark". It is "close the gap between that harness and the
real Fono path, and widen the fixture set to the failure taxonomy we already have evidence
for".

### The one structural flaw: it is not grounded in reality

This is the crux, and it is exactly the property the proposal asked for and the current
harness does not have. `call_first_turn` / `call_second_turn` / `post_chat`
(`crates/fono-bench/src/assistant_tool_use.rs:369-455`) hand-roll an OpenAI chat request
and a two-message tool replay. Everything Fono does between the model and the world is
bypassed:

| machinery | where it lives | bypassed today |
|---|---|---|
| system-prompt composition | `crates/fono/src/session.rs:239` | yes — bench writes its own two-line prompt at `crates/fono-bench/src/assistant_tool_use.rs:613-636` |
| room + device hint | `crates/fono/src/actions/mod.rs:186-233` | yes |
| blank-argument trimming | `crates/fono/src/actions/mod.rs:259-279` | yes |
| schema validation before send | `crates/fono/src/actions/mod.rs:304-328` | yes |
| retry after refusal / partial | `crates/fono/src/actions/mod.rs:361-374`, `:391-396`, `:469-473` | yes |
| vendor admission ladder | `crates/fono/src/actions/vendor.rs:40-122` | yes |
| readback verification | `crates/fono/src/actions/mod.rs:535-565` | yes |
| the real streaming pump | `crates/fono/src/assistant.rs:270` | yes |

Consequence: the benchmark grades **the model**, while the entire v5 Tier-1 plan is about
grading **Fono's recovery of a model that got it wrong**. A run of the current harness
cannot distinguish "the model routed correctly" from "the model routed badly and Fono
fixed it" — which is precisely the distinction Task 9 exists to measure.

### Reframing

Do not build a model benchmark that uses Fono's machinery. Build a **regression harness
for Fono's action pipeline** that is parameterised by model. The difference is not
cosmetic — it changes the defaults:

| | model benchmark | pipeline harness |
|---|---|---|
| default target | a real house | a deterministic fake house |
| default input | text | text (audio is a second tier) |
| default cadence | occasional, manual | every CI run |
| headline number | pass rate | **first-try** pass rate *and* **after-recovery** pass rate |
| model comparison | the point | one axis of many |

Model comparison then falls out as a special case, rather than the harness being bent
around it later.

---

## What the proposal is missing

Ordered by how much damage the omission causes.

### M1. "Success rate" is at least three numbers, not one

Collapsing them makes the benchmark unable to guide improvement:

- **First-try routing score** — tool name and arguments on the model's opening call.
- **After-recovery score** — the outcome once Fono's schema check, retry-with-server-error,
  and partial-retry ladder have run. The delta between the two *is the measured value of
  Tier 1*, and it is invisible unless both are reported.
- **Truthfulness score** — did the final reply match what actually happened? The v4/v5
  evidence is emphatic that the worst available failure is a fluent claim of success over
  a dark lamp (`crates/fono/src/actions/mod.rs:8-16`,
  `crates/fono-assistant/src/traits.rs:313-324`). A harness that only checks the world
  state rates an honest failure and a lying failure identically.

Add a fourth, cheap and load-bearing: **reply-language match**. F35 in the v5 plan is two
Romanian commands answered in English; the current scoring only greps for expected
substrings, which is a weak proxy.

### M2. A real house cannot be the default target

The proposal names real devices — office outdoor light, a dimmable living-room lamp, a
Google speaker, the hallway AC. Running those for real is valuable **and** it must not be
the default, for four reasons: it cannot run in CI; the house's state is not controlled,
so runs are not comparable; it is slow; and it switches real things, which under the
project's own rules is not something to automate casually.

More importantly, a real house **cannot provoke the interesting cases on demand**. The
failures worth regression-testing are refusals ("Input validation error: None is not of
type 'string'", "Received invalid slot info"), partial successes with a named `failed[]`
list, and unreachable servers. You cannot reliably make a working house produce those.

**Proposal: a stateful fake MCP server, seeded from a real capture.** In-process, speaks
the same `tools/list` / `tools/call` surface `crates/fono-assistant/src/mcp_client.rs:200`
and `:331` expect, seeded from a real `tools/list` + `GetLiveContext` dump so the tool
signatures and device names are genuine. It holds device state, so `HassTurnOn` followed
by `GetLiveContext` actually reflects the change and the readback ladder
(`crates/fono/src/actions/mod.rs:535-565`) is exercised rather than stubbed. It is
scriptable to fail in the exact recorded ways. Then a `--live` mode, opt-in and never
default, for the occasional real-house confirmation — patterned on the existing ignored
live tests (`crates/fono/src/actions/mod.rs:840-879`, `:917-1045`).

### M3. The layering problem — `fono-bench` cannot see the action machinery

`fono-bench` depends on `fono-stt`, `fono-polish`, `fono-assistant`, `fono-core`,
`fono-download` (`crates/fono-bench/Cargo.toml:33-41`). The executor, the hint and the
vendor ladder live in the **binary** crate at `crates/fono/src/actions/`. There is no edge
from the bench to them today, and this is the first decision the implementation has to
make. Three options:

- **(a) Hoist the action machinery into a crate both can see** — move
  `crates/fono/src/actions/` (executor + vendor ladder) down into `fono-assistant`, or into
  a new `fono-actions` crate. Net-zero on shipped binary size (it is a move of existing
  code, not a new dependency), and it also unblocks v5 Track B Task 10, which needs one
  composition function visible from both crates. **Recommended.**
- **(b) Put the harness inside `fono`** as a hidden subcommand. Cheapest to write, but it
  grows the shipped binary with benchmark code and orphans the existing fixture and report
  machinery in `fono-bench`.
- **(c) `fono-bench` gains a path dependency on `fono`.** Works, free on binary size
  (`fono-bench` is `publish = false` and never linked into `fono`), but it makes the bench
  build the whole desktop stack — tray, overlay, hotkeys — for a text-in benchmark.

Option (a) is the only one that leaves the codebase better than it found it.

### M4. The STT axis is absent, and it is a correctness axis, not just a latency one

"How fast Fono responds to a command" starts at the keypress, not at the text. And STT is
a *routing* input: a mangled Romanian room name or a case-folded "Master bedroom" fails at
the house for reasons the model never caused. Two tiers:

- **Text-in** (default): deterministic, fast, CI-able, the regression gate.
- **Audio-in** (periodic): WAV fixtures through the real STT. The repo already has WAV
  fixtures, WER/CER and a runner (`crates/fono-bench/src/{wav,fixtures,wer,runner}.rs`).

One trap to record: synthesising the audio fixtures with the TTS bench is cheap and
tempting, but TTS speech is markedly easier than real speech and will flatter the STT.
Use it as a smoke tier; do not report it as an STT result.

### M5. "Speed" needs decomposing into the numbers a user feels

The current report has total, first-turn and second-turn latency
(`crates/fono-bench/src/assistant_tool_use.rs:95-106`). The numbers that matter to someone
standing in a room are **time-to-first-audio** and **time-to-effect** (when the device
actually changed). Keep the existing prompt-cache fields — per the v5 evidence the head
pin took a turn from 39.4 s to 7.4 s, which dwarfs every other lever on a local model.
Add tokens in/out and, for cloud rows, **cents per command**: that is the axis that decides
whether a cloud model is viable for an always-on assistant.

### M6. Tracing already exists — wire it, do not invent it

`fono_core::turn_trace` writes Chrome Trace JSON with a dedicated `actions` lane carrying
`tool.execute`, `tool.verify` and `tool.rejected`
(`crates/fono-core/src/turn_trace.rs:120-130`, `crates/fono/src/actions/mod.rs:410-415`,
`:424-444`, `:546-563`), and `transcript_enabled()` records the verbatim prompt and reply
(`crates/fono-core/src/turn_trace.rs:45-64`). The harness should set
`FONO_ASSISTANT_TRACE` per fixture run and archive the trace beside the report row. That
answers "what exactly is it sending" for free, and in the same format used to debug
production.

**Warning to carry into the plan:** a trace file *is a transcript* — the whole system
prompt, every device name in the house, every utterance. Benchmark output directories are
sensitive by construction. Only fake-house runs are safe to publish or commit.

### M7. The fixture set needs the failure taxonomy, not the happy path

The proposal lists lights on/off per room, a specific named light, a dimmable light, music
on a speaker, and the hallway AC. Those are the right *devices*; they are almost all the
happy path. The recorded evidence says the interesting classes are:

| class | evidence | what it protects |
|---|---|---|
| wrong tool among near-identical signatures | v5 F33 — `HassLightSet` chosen for a plain switch-on | hint rule 4 |
| domain-less room command | v5 traces `…456`/`…468` — rollers moved, lights did not | hint rule 2 |
| invented optional argument | `brightness: 10`, `color: "#FFFFFF"` nobody asked for | Task 4 + hint rule 5 |
| device named after a room it is not in | `crates/fono/src/actions/mod.rs:158-165` | hint rule 3 |
| server refusal with a usable error | "invalid slot info" | Task 1 retry |
| partial success with named failures | `Admission::PartlyWorked` | Task 2 retry |
| non-idempotent command | "two degrees warmer" | Task 3 — must **not** retry |
| room named in another language | Romanian `bucătărie`, English house | the room hint itself |
| must-not-act | ambiguity, status query, explanation | already present, keep |

Note the AC is not merely another device: it is the *victim* of the domain-less room
command, so "turn on the office light" must assert the AC stayed **off**. That is a
negative assertion the current fixture schema cannot express and needs to gain.

### M8. The catalogue-size axis flips model rankings

A real Home Assistant exposes ~23 `Hass*` tools and can exceed 200 devices
(`crates/fono/src/actions/mod.rs:241`). The manifest already supports `tools_path` and
`inventory_path` for exactly this. Make it a **declared axis** with at least three rungs:
synthetic single tool / real 23-tool catalogue / real catalogue plus a 200-device list.
Small models degrade sharply across it while large ones barely move — which is precisely
why the v5 plan **rejected** trimming the catalogue by model class, and the harness should
be able to re-demonstrate that rejection rather than take it on faith.

### M9. Determinism and variance

`temperature: 0.0` is set, but tool-calling is not reproducible in practice. The report
carries `iterations` but no variance. Report n, pass count and a confidence interval, and
mark single-iteration runs as indicative. Without this, prompt tuning will chase noise.

### M10. The second tool family should prove vendor-neutrality, not add features

The proposal asks what other tools make sense. The instinct to add Fono's own MCP tools
(`crates/fono-mcp-server/src/tools/`) is understandable but points the wrong way: those are
tools an *agent* calls **into** Fono, a different direction from Fono calling **out**. The
useful second family is a deliberately boring generic MCP server — two or three unrelated
tools, unfamiliar naming, no Home Assistant vocabulary — whose only job is to demonstrate
that nothing in the pipeline branches on vendor. That is verification criterion 6 of the
v5 plan, currently checkable only by reading the diff.

### M11. The benchmark will become the target

Twelve fixtures reduced to one number, optimised against, gets gamed by prompt tweaks that
overfit. Mitigations: keep a held-out fixture set never used for tuning; keep
`suite_version` honest and bump it whenever fixtures change so old reports are not compared
to new ones; and require that a prompt change justified by this harness also moves a real
trace.

---

## Implementation Plan

Sliced so each slice is independently useful and independently revertible.

### Slice 0 — decide the layering

- [ ] Task 1. Choose between hoisting the action machinery into a shared crate, hosting the
      harness inside the `fono` binary, or giving `fono-bench` a path dependency on `fono`.
      Rationale: nothing else can start until the harness can call `run_one`, `room_hint`
      and `assistant_prompt_head`. Recommendation is the hoist; record the decision and its
      reasoning wherever the choice is made, because it also determines whether v5 Track B
      Task 10 gets easier or harder.
- [ ] Task 2. If hoisting: move `crates/fono/src/actions/` and its vendor ladder into a
      crate both `fono` and `fono-bench` can depend on, with no behaviour change and no new
      dependency. Rationale: a pure move keeps the diff reviewable and the binary size
      unchanged; behaviour changes in the same commit would make a size or latency
      regression unattributable.

### Slice 1 — a deterministic fake house

- [ ] Task 3. Capture a real `tools/list` and `GetLiveContext` dump from the reference
      Home Assistant and commit them as fixtures, with device and room names preserved
      verbatim. Rationale: genuine signatures and genuine naming quirks are the whole point;
      a hand-written catalogue would omit exactly the ambiguities that cause the failures.
- [ ] Task 4. Build an in-process fake MCP endpoint that serves those captures, holds
      device state, and answers `tools/call` by mutating it. Rationale: the readback
      verification rung and the `Admission` ladder are unreachable against a stub, and they
      are half of what the harness is meant to measure.
- [ ] Task 5. Make the fake scriptable into the recorded failure modes — hard refusal with
      the server's own error text, partial success with a named `failed[]` list, timeout,
      unreachable. Rationale: these are the inputs to every Tier-1 recovery path and cannot
      be provoked on demand against a working house.
- [ ] Task 6. Add a `--live` mode that points the same harness at a configured real server,
      off by default and never exercised by CI. Rationale: the fake is only trustworthy if
      it is periodically checked against the thing it imitates.

### Slice 2 — route the harness through Fono's own pipeline

- [ ] Task 7. Replace the hand-rolled chat request with the real path: the real
      system-prompt head, the real hint, the real `ActionTools`, the real assistant backend,
      the real streaming pump. Rationale: this is the single change that makes the number
      mean something about Fono rather than about the model, and it is what v5 Task 9 needs.
- [ ] Task 8. Keep the existing hand-rolled path as a named comparison mode. Rationale: the
      difference between "raw model" and "through Fono" is the measured value of the whole
      action stack, and it is the most persuasive number the harness can produce.
- [ ] Task 9. Set `FONO_ASSISTANT_TRACE` per fixture run and archive each trace beside its
      report row, keyed by fixture id and iteration. Rationale: reuses the production
      waterfall rather than inventing a second observability format; also the only way to
      attribute a latency regression to a stage.
- [ ] Task 10. Document, in the harness's own output and its README, that a trace directory
      is a transcript containing the whole device list and every utterance. Rationale: the
      warning already exists at `crates/fono-core/src/turn_trace.rs:45-64` and will be
      re-learned the hard way the first time someone attaches a run to an issue.

### Slice 3 — scoring that separates routing from recovery from honesty

- [ ] Task 11. Score the model's **first** tool call and the **final** outcome separately,
      and report both. Rationale: M1 — without the pair, the recovery ladder's value cannot
      be seen and Tier 2 cannot be gated on evidence.
- [ ] Task 12. Add a world-state assertion: after the turn, ask the fake house what changed
      and compare against a per-fixture expectation, including **negative** expectations
      ("the AC is still off"). Rationale: M7 — the domain-less room command is the recorded
      failure and cannot be caught by inspecting the tool call alone.
- [ ] Task 13. Add a truthfulness check comparing the spoken reply against the observed
      world state, at minimum catching a claim of success over a device that did not move.
      Rationale: this is the failure the project treats as worst, and it is currently
      unmeasured anywhere.
- [ ] Task 14. Add an explicit reply-language metric rather than inferring it from expected
      substrings. Rationale: F35; also makes adding French and Spanish fixtures cheap,
      because the language check stops depending on hand-picked keywords per language.
- [ ] Task 15. Report per-fixture n, pass count and a confidence interval, and flag
      single-iteration runs. Rationale: M9 — otherwise the first prompt tweak will be
      validated by noise.

### Slice 4 — the fixture set

- [ ] Task 16. Extend the fixture schema for the new assertions: expected world state,
      forbidden world state, expected reply language, and whether a retry is permitted.
      Rationale: Task 3 of the v5 plan makes "never retry a relative change" a rule, and a
      rule with no fixture is an accident waiting to be reintroduced.
- [ ] Task 17. Author fixtures for every class in the M7 table, in English and Romanian
      first. Rationale: those two are already covered by recorded traces, so expectations
      can be grounded rather than guessed.
- [ ] Task 18. Add French and Spanish, authored by someone fluent rather than machine
      translated, with the room and device names left in the house's own language.
      Rationale: translationese is not what users say, and the untranslated device names
      are the exact thing the room hint exists to protect.
- [ ] Task 19. Add the dimmable-light, media-playback and air-conditioning fixtures from the
      original proposal, with the AC appearing as both a target and a bystander.
      Rationale: they widen the tool surface beyond on/off, which is where the wrong-tool
      failure lives.
- [ ] Task 20. Add a small generic non-Home-Assistant MCP fixture family. Rationale: M10 —
      makes the vendor-neutrality criterion executable instead of reviewable.

### Slice 5 — axes, cost and reporting

- [ ] Task 21. Make catalogue size a declared axis with the three rungs named in M8.
      Rationale: model rankings invert across it, and two rejected proposals in the v5 plan
      depend on that claim staying true.
- [ ] Task 22. Record tokens in and out per turn and, for cloud rows, cost per command.
      Rationale: the usage figures are already parsed; the missing step is the arithmetic
      and a per-model price table.
- [ ] Task 23. Add time-to-first-audio and time-to-effect to the latency set, keeping the
      existing prompt-cache fields. Rationale: M5 — these are what a user feels, and the
      cache fields are the biggest measured local-model lever.
- [ ] Task 24. Bump the report schema version and write a one-screen comparison renderer.
      Rationale: a JSON blob nobody reads will not change a decision; the existing
      per-language table is the right shape to extend.

### Slice 6 — make it a gate

- [ ] Task 25. Run the fake-house, text-in, `en`+`ro` configuration in CI on every change to
      the action pipeline or the prompts. Rationale: the recorded regressions are all
      prompt- and executor-shaped, and all of them would have been caught by this
      configuration.
- [ ] Task 26. Commit a baseline report for that configuration and compare per-fixture
      verdicts rather than absolute timings. Rationale: the equivalence harness already
      established this pattern for a good reason — timings flap on shared runners and are
      not part of the contract.
- [ ] Task 27. Hold out a fixture subset that is never used for prompt tuning, and say so in
      the README. Rationale: M11.

### Slice 7 — audio-in tier (optional, after Slice 6 is green)

- [ ] Task 28. Add WAV fixtures through the real STT for a subset of commands, reported
      separately and never gating CI. Rationale: M4 — STT errors are a real routing input,
      but folding them into the main number would make every model comparison also a
      microphone comparison.

---

## Verification Criteria

- A single command runs the whole suite against the fake house with no network, no real
  devices and no API key, and finishes fast enough to sit in CI.
- The report distinguishes first-try routing failures from failures that survived recovery,
  and the difference between the two is visible per fixture and per language.
- Every failure class in the M7 table has at least one fixture, and each fixture fails when
  the corresponding Tier-1 protection is reverted.
- "Turn on the office light" asserts the lights changed **and** the air conditioning did
  not.
- A Romanian command is scored for a Romanian reply by an explicit metric, not by keyword.
- A run against the generic non-Home-Assistant server passes with no vendor-specific
  fixture edits, demonstrating the pipeline does not branch on vendor.
- Each fixture run leaves a trace file that shows the `actions` lane with `tool.execute`
  and, where applicable, `tool.verify`.
- v5 Task 9 can be answered by pointing at a report rather than by re-reading four traces
  by hand.
- No new dependency in the shipped binary graph; the size budget is unchanged; formatting,
  clippy and tests all clean.

---

## Potential Risks and Mitigations

1. **The fake house diverges from the real one, and the suite passes while the house
   fails.**
   Mitigation: seed the fake from a committed real capture; keep the `--live` mode and run
   it before any release that touches the action path; treat a divergence as a bug in the
   fake, not in the fixtures.
2. **Hoisting the action machinery destabilises the shipped binary.**
   Mitigation: land the move as a pure relocation with no behaviour change, verified by the
   existing action tests and the size-budget gate, before any harness code depends on it.
3. **The benchmark becomes the optimisation target and the prompts overfit.**
   Mitigation: a held-out fixture subset, an honest `suite_version`, and a standing rule
   that a prompt change must also move a real trace.
4. **Routing through the real pipeline makes runs slow and flaky, and CI starts getting
   ignored.**
   Mitigation: the CI configuration is fake house, text-in, two languages, one iteration,
   compared on verdicts and not timings; everything heavier is opt-in.
5. **Trace archives leak the contents of a real home into a bug report or a commit.**
   Mitigation: only the fake-house configuration produces publishable traces; the live mode
   writes to a directory that is documented as sensitive and ignored by version control.
6. **Four languages multiply the fixture-authoring cost and the suite rots.**
   Mitigation: land English and Romanian first, where recorded evidence grounds the
   expectations; add French and Spanish only once the schema has stopped moving, so they
   are authored once.
7. **Measuring the pipeline hides a model regression, or vice versa.**
   Mitigation: Task 8's raw-model comparison mode keeps both readings available from the
   same fixtures.

---

## Alternative Approaches

1. **Leave the harness where it is and only widen the fixtures.** Cheapest by far, and it
   would improve model selection. Rejected as the primary path because it cannot answer the
   question actually being asked — whether Fono's own recovery machinery works — and that
   is the question three plans of evidence have been accumulating toward.
2. **Test against the real house only, with a human confirming.** Highest fidelity and
   zero infrastructure. Rejected as the default because it cannot run in CI, cannot be
   compared across runs, and cannot provoke the refusal and partial-failure cases that the
   Tier-1 work exists to handle.
3. **Score with a model-as-judge instead of hard assertions.** Attractive for the
   truthfulness and language checks, which are genuinely fuzzy. Rejected for the routing and
   world-state checks, where assertions are exact and free; worth revisiting for the
   truthfulness metric alone if the mechanical version proves too blunt.
4. **Build it as a standalone tool outside the workspace.** Faster to start, avoids the
   layering decision entirely. Rejected because it would immediately duplicate the prompt
   composition and the executor, and the project has already been bitten twice by two
   renderings of the same prompt drifting apart.
