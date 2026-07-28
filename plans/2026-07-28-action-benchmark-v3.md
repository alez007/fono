# Voice-action benchmark — real house, real path, discovered targets

**Status:** proposal, ready for review. Not started.
**Supersedes:** `plans/2026-07-28-action-benchmark-v1.md` (fake-house default — overruled) and
`plans/2026-07-28-action-benchmark-v2.md` (role map + allowlist — both dropped as
over-engineering). v1's failure taxonomy and scoring split survive and are referenced, not
repeated.
**Relates to:** `plans/2026-07-28-voice-actions-universal-first-v5.md` Task 9.

---

## Objective

Run real utterances, in four languages, through **the same code path Fono uses when the user
speaks to it**, against **the real MCP servers the user has connected**, and report:

1. **Routing** — right tool, right arguments, first try.
2. **Recovery** — when not, did Fono's own ladder rescue it inside the same turn.
3. **Truthfulness, language and speed.**

Constraints: the house is real; nothing about it reaches the repository; **any user can run
it without writing a configuration file first.**

---

## The two questions answered

### "Do we need a gitignored `house.toml`?" — No. Discover the house instead.

v2 proposed fixtures naming abstract roles plus a private map from roles to real devices.
Dropped, for a reason that also makes the benchmark *more* portable, not less.

The house already describes itself. `GetLiveContext` returns per-entity blocks carrying the
name, the **domain**, the **areas** and the **state**
(`crates/fono-assistant/src/mcp_client.rs:292-320`) — Fono parses exactly this today to build
the room and device lists in the prompt (`crates/fono/src/actions/mod.rs:186-233`).

So a fixture does not need to name a device. It states a **requirement**, and the harness
resolves it against whatever house it is pointed at:

- "any entity in domain `light`"
- "an area containing at least one `light` **and** at least one other switchable domain" —
  which is the whole domain-less-room-command fixture, resolved automatically
- "any `light` that reports a `brightness` attribute" — the dimmable case
- "any `media_player`", "any `climate`"

A fixture that cannot be satisfied is reported **`Skipped — no matching device`**, not failed.
That is honest, and it is what lets the same suite run against a one-lamp flat and a
two-hundred-entity house.

What this buys over the role map:

| | role map | discovery |
|---|---|---|
| new user's first run | write a config file first | works immediately |
| private nouns in git | none | none |
| stays correct when the house changes | no — map goes stale silently | yes |
| tests the real naming quirks | only the ones you mapped | whatever the house actually has |

The privacy requirement is met by **where output goes**, which was always the real mechanism:
resolved names live only in the run directory, never in a fixture. Two report layers, exactly
as v2 had them — a **safe layer** (verdicts, latencies, tokens, cost, keyed by requirement id)
that is shareable and is what regression comparison reads, and a **detail layer** (resolved
names, literal arguments, reply text) that stays local alongside the traces and the cassette.
Comparison therefore works on a machine that has never seen the house.

Resolution must be **deterministic** — sort candidates and take the first — or two runs pick
different lamps and nothing is comparable. The chosen entity is recorded in the detail layer
so a failure can be investigated, and an override flag can pin a specific device when someone
is chasing one.

### "Do we need a role allowlist?" — No. It guards nothing.

The reasoning that produced it does not survive contact with what the harness actually does.
The benchmark hands the model **the same tool catalogue the assistant is handed every day**
(`crates/fono/src/actions/mod.rs:50-113`, reading the user's own active tools). A model that
could switch off something unexpected here could do it in ordinary conversation. An allowlist
would be a guard on the benchmark that the thing being benchmarked does not have — which
also makes it a *fidelity* bug: constraining the reachable set changes the routing problem,
and the benchmark would stop measuring the real one.

What is genuinely different about a benchmark run is that it is **unattended and repetitive**.
That needs two things, and neither is configuration:

- **Restore after every fixture** — needed anyway for repeatability (R1 below), and it is what
  makes an unattended run safe to leave alone.
- **A hardcoded refusal to target safety-relevant domains** — `lock`, `alarm_control_panel`,
  garage covers. Not user-facing, not configurable, about five lines: the harness never
  *selects* those as fixture targets. It says nothing about what the model may call, so
  fidelity is untouched.

Note `lock` is in `ACTIONABLE` (`crates/fono-assistant/src/mcp_client.rs:275-288`), so it *is*
offered to the model as it should be — the exclusion belongs in fixture target selection only.

Quiet hours survives, shrunk to a default-on volume clamp plus skipping media fixtures at
night, because a benchmark that wakes the house at 3 a.m. gets deleted rather than fixed.

---

## Design, in full

### D1. Text in, silent out — the path already supports it

The most useful finding here: almost nothing needs building.
`AssistantTurnInputs.pre_transcribed` (`crates/fono/src/assistant.rs:193-198`) makes
`run_assistant_turn` (`crates/fono/src/assistant.rs:276`) skip STT and treat the string as the
user's turn — that branch is `crates/fono/src/assistant.rs:318-343`. `tts` is already `Option`
and documented as "text-only turn" (`crates/fono/src/assistant.rs:151-154`); `overlay` is
already `Option` (`crates/fono/src/assistant.rs:192`).

A headless text turn is therefore `pcm: vec![]`, `pre_transcribed: Some(utterance)`,
`tts: None`, `overlay: None`, a drained `action_tx`, and a real `Arc<dyn Assistant>`.
Everything else — prompt composition, the room hint, blank-argument trimming
(`crates/fono/src/actions/mod.rs:259-279`), the schema check (`:304-328`), the retry ladder
(`:361-374`, `:469-473`), the vendor admission ladder, the readback (`:535-565`) — is the
production path because it *is* the production path.

**Rule: never add a second text-in path.** If a text turn needs behaviour that does not exist,
add it to `run_assistant_turn` so the voice path gets it too.

### D2. Layering — a feature-gated subcommand inside `fono`

The harness needs the user's **actual** `Config`, `Paths`, `Secrets` and tool-catalogue
database, because `actions::build` reads all four (`crates/fono/src/actions/mod.rs:50-113`).
That is binary-crate territory; pulling it into `fono-bench` would mean reimplementing config
and secrets resolution.

**`fono bench actions`, behind a non-default `bench-actions` feature**, with `fono-bench` as an
optional dependency so the existing fixture, statistics and report machinery in
`crates/fono-bench/src/assistant_tool_use.rs` is reused. The size gate builds default features
(`crates/fono/Cargo.toml:36`), so a non-default feature costs the shipped artefact nothing.

v1's hoist of `crates/fono/src/actions/` into a shared crate may still be worth doing for v5
Track B Task 10, but it is not a prerequisite.

### D3. Repeatability on a real house — three mechanisms

- **R1. Precondition and restore, per fixture, bypassing the model.** Drive the target into
  the fixture's declared starting state with direct `mcp_client::call_tool`
  (`crates/fono-assistant/src/mcp_client.rs:331`), never the assistant; restore after
  asserting. Without this, "turn off the lights" scores differently depending on prior state.
  A failed restore **aborts the suite loudly**.
- **R2. Snapshot before and after, and a `Drifted` verdict.** A real house has other actors —
  schedules, other people, a thermostat with opinions. If something changed that no tool in
  this turn touched, the verdict is `Drifted`, not `Failed`. Collapsing the two is how a
  real-house suite becomes noise nobody trusts.
- **R3. Cassette at the MCP boundary.** A recorder around `discover`
  (`crates/fono-assistant/src/mcp_client.rs:200`) and `call_tool` (`:331`) captures verbatim
  requests and responses. Three modes: **`live`** (default, records), **`replay`** (cassette
  only — no network, no devices, deterministic), **`live-verify`** (replay against the live
  house and report divergence, which is what keeps a cassette honest). Replay is the
  regression ratchet: change a prompt, replay yesterday's cassette, see which fixtures moved.
  Cassettes are as sensitive as traces and live in the gitignored run directory.

Plus **fault injection at the same boundary** — swap one real response for a recorded refusal
("Received invalid slot info"), a partial success with a named `failed[]` list, a timeout, or a
connection error. Real house, injected fault, real recovery path. The Tier-1 ladder is
unreachable otherwise, because a working house will not refuse on demand.

### D4. What still needs saying

- **The language matrix is two-dimensional** — *house language* × *speaker language*, and the
  cells are not equally interesting. An English-named house addressed in Romanian is the
  recorded failure the room hint exists to fix (`crates/fono/src/actions/mod.rs:145-153`); the
  same house in English is trivial. Weight fixtures toward the mismatched cells.
- **Model overrides layer over the loaded config** — comparing five models must not mean
  editing the user's configuration five times, and everything else about the turn must stay
  exactly as the user has it.
- **Budget the run before starting it.** Fixtures × languages × models × iterations multiplies
  fast, every cell moves a real device, and cloud rows cost money. A dry run printing the cell
  count, the estimated cost, and the resolved devices that will be touched — confirmed once.
  This is also where discovery gets checked before anything moves.
- **Idempotency is a first-class self-check.** Two consecutive runs must produce identical
  verdicts. If they do not, restore is broken. That is the only proof R1 works.
- **CI needs something, and the notes server is it** — see below. A benchmark with no ratchet
  decays.

---

## Which second and third MCP servers

Criteria: most people can run it, it stresses a **different** failure mode, it is free and
local where possible, it is safe to write to.

### Second: a filesystem-backed notes / list server — the highest-value addition

- It tests an axis nothing else does: **verbatim fidelity of a free-text argument**. Home
  Assistant tests picking the right name from a *closed set*. A note server tests whether the
  model writes down **what you actually said** rather than a tidy paraphrase — which for a
  dictation-adjacent tool matters a great deal.
- It is where **diacritic and encoding damage** gets caught end to end: a Romanian or Spanish
  sentence in, byte-identical out.
- Verification is an exact string compare. No vendor knowledge needed.
- It is safe, deterministic, needs no discovery, and contains **no private nouns at all** — so
  it is the one family that can gate CI end to end against a temp directory.
- It doubles as the **vendor-neutrality control**: no Home Assistant vocabulary anywhere,
  proving nothing in the pipeline branches on vendor (v5 verification criterion 6, currently
  checkable only by reading the diff).

### Third: a calendar server, against a dedicated test calendar

- **Relative temporal reasoning across languages** — "next Tuesday at half three",
  "poimâine la zece", "dans deux semaines". Models produce confidently wrong ISO timestamps
  and nothing currently catches it.
- It has a natural readback, so `VerifyClass::PostCondition` and `confirm`
  (`crates/fono/src/actions/mod.rs:535-565`) get exercised against a **second vendor** — that
  rung is presently proven only by Home Assistant.
- It surfaces timezone and locale handling, which nothing else touches.
- Caveat: it writes to a real calendar, so it needs a dedicated one and the same restore
  discipline as the house.

### Considered and not recommended

- **A separate media server (MPD, Spotify)** — redundant; the speaker case is already reached
  through Home Assistant's media player, and it adds surface without a new failure mode.
- **Web search / fetch** — genuinely interesting as the canonical "tool returning far more text
  than you wanted", which stresses context growth and summary honesty, but non-deterministic
  and network-bound. An axis later, not a family now.
- **Fono's own MCP server** (`crates/fono-mcp-server/src/tools/`) — points inward: those are
  tools an agent calls *into* Fono. One exception worth remembering: `fono.confirm` as a tool
  the assistant may call would make multi-turn clarification testable. Later.

---

## Implementation Plan

### Slice 0 — the seam

- [ ] Task 1. Add a non-default `bench-actions` feature to the `fono` crate and a hidden
      `fono bench actions` subcommand behind it, with `fono-bench` as an optional dependency.
      Rationale: the harness needs the user's real config, secrets and tool catalogue, which
      only the binary crate resolves; a non-default feature leaves the shipped artefact and the
      size gate untouched.
- [ ] Task 2. Build a headless text-in turn driver calling `run_assistant_turn` with
      `pre_transcribed` set, no PCM, no TTS, no overlay, returning reply text, tool calls and
      stage timings. Rationale: this is the entire "normal Fono path" requirement and the
      fields already exist; anything missing belongs in the shared pump so the voice path
      benefits too.
- [ ] Task 3. Add backend and model overrides layered on top of the loaded config, leaving
      tools, servers and secrets untouched. Rationale: comparing models must not mutate the
      user's configuration.

### Slice 1 — discovery and output hygiene

- [ ] Task 4. Implement fixture target requirements resolved against the live house from the
      context dump — by domain, by co-located domains within one area, by attribute — with
      deterministic selection, a `Skipped — no matching device` verdict, and an override flag
      to pin a specific entity. Rationale: this is what makes the suite runnable by another
      user on the first attempt and keeps it correct when the house changes.
- [ ] Task 5. Exclude safety-relevant domains from **target selection only** — `lock`,
      `alarm_control_panel`, garage covers — hardcoded, not configurable, with the tool
      catalogue offered to the model left exactly as the assistant sees it. Rationale: an
      unattended repetitive run should not choose the front door; constraining what the model
      may *call* would change the routing problem and break fidelity.
- [ ] Task 6. Split the report into a safe layer keyed by requirement id and a detail layer
      carrying resolved names, literal arguments and reply text, and write every run into a
      gitignored run directory under the Fono state dir. Rationale: regression comparison must
      work on the safe layer alone, on a machine that has never seen the house.
- [ ] Task 7. State in the harness output that the run directory contains the whole device
      list and every utterance. Rationale: the warning at
      `crates/fono-core/src/turn_trace.rs:45-64` will otherwise be relearned the first time a
      run is attached to an issue.

### Slice 2 — making a real house repeatable

- [ ] Task 8. Per-fixture precondition and restore via direct MCP calls that bypass the
      assistant, aborting the suite loudly on a failed restore. Rationale: without a known
      starting state the same fixture scores differently between runs, and an unrestored house
      is a real-world harm.
- [ ] Task 9. Snapshot world state before and after each turn and add a `Drifted` verdict
      distinct from `Failed`. Rationale: other actors in the house will perturb runs, and
      mislabelling that as failure destroys trust in the suite.
- [ ] Task 10. Add a default-on volume clamp and skip media fixtures outside a configured
      window. Rationale: a suite that wakes the house gets deleted rather than fixed.
- [ ] Task 11. Add a dry run printing cells, estimated cost and the resolved devices that will
      be touched, confirmed once. Rationale: it prevents the run nobody meant to start, and it
      is where discovery is checked before anything moves.
- [ ] Task 12. Add a suite-idempotency self-check running the suite twice and requiring
      identical verdicts. Rationale: the only proof Task 8 works.

### Slice 3 — record and replay

- [ ] Task 13. Record verbatim requests and responses at the `discover` / `call_tool`
      boundary into a cassette in the run directory. Rationale: that boundary is the only place
      the house's full behaviour is observable in one stream.
- [ ] Task 14. Add `replay` mode driving the same pipeline from a cassette with no network and
      no devices. Rationale: this is the regression ratchet, and the only way to evaluate a
      prompt change without re-running the house.
- [ ] Task 15. Add `live-verify` mode replaying a cassette against the live house and
      reporting divergence. Rationale: a cassette silently drifting from the house would make
      replay results actively misleading.
- [ ] Task 16. Add boundary fault injection for the recorded failure classes — hard refusal
      with the server's own text, partial success with a named `failed[]` list, timeout,
      unreachable. Rationale: the Tier-1 retry ladder is otherwise never exercised, and a
      working house cannot be made to refuse on demand.

### Slice 4 — scoring

- [ ] Task 17. Score the model's first tool call and the final outcome separately and report
      both. Rationale: the delta between them *is* the measured value of Tier 1, and it is what
      v5 Task 9 needs.
- [ ] Task 18. Assert world state after the turn, including negative assertions — the
      co-located non-light entity in the same area must not have moved. Rationale: the
      domain-less room command cannot be caught by inspecting the tool call alone, and
      discovery now supplies the bystander automatically.
- [ ] Task 19. Add a truthfulness check comparing the reply against the observed world state,
      at minimum catching a claim of success over a device that did not move. Rationale: the
      project treats this as the worst available failure and it is currently unmeasured.
- [ ] Task 20. Add an explicit reply-language metric rather than inferring it from expected
      substrings. Rationale: v5 F35; it also makes French and Spanish cheap to add.
- [ ] Task 21. Report per-fixture n, pass count and a confidence interval, and flag
      single-iteration runs as indicative. Rationale: otherwise the first prompt tweak is
      validated by noise.
- [ ] Task 22. Add time-to-first-reply and time-to-effect to the latency set, keep the
      existing prompt-cache fields, and add tokens and cost per command. Rationale: these are
      what a user feels, and the cache fields remain the largest measured local-model lever.

### Slice 5 — fixtures

- [ ] Task 23. Extend the fixture schema for target requirements, preconditions, expected and
      forbidden world state, expected reply language, and whether a retry is permitted.
      Rationale: v5 Task 3's "never retry a relative change" rule has no fixture, and a rule
      without one gets reintroduced by accident.
- [ ] Task 24. Author v1's failure taxonomy as requirement-based fixtures in English and
      Romanian. Rationale: those two are grounded in recorded traces, so expectations are
      evidence rather than guesswork.
- [ ] Task 25. Add the dimmable-light, media-playback and climate fixtures, with the climate
      entity appearing as both target and bystander. Rationale: they widen the surface past
      on/off, which is where the wrong-tool failure lives (v5 F33).
- [ ] Task 26. Add French and Spanish once the schema has stopped moving, authored by someone
      fluent, with house names left in the house's own language. Rationale: translationese is
      not what users say, and untranslated names are exactly what the room hint protects.
- [ ] Task 27. Declare the house-language × speaker-language matrix and weight fixtures toward
      the mismatched cells. Rationale: the matched cells are trivial and the mismatched ones
      are the recorded failures.

### Slice 6 — the second and third servers

- [ ] Task 28. Add the filesystem-backed notes/list family with verbatim-fidelity and
      diacritic round-trip fixtures. Rationale: it tests an axis nothing else does, it is the
      vendor-neutrality control, and it needs no house.
- [ ] Task 29. Wire the notes family into CI end to end against a temp directory. Rationale: a
      benchmark with no ratchet decays, and this is the part that can run without a house.
- [ ] Task 30. Add the calendar family against a dedicated test calendar, with relative
      temporal expressions in all four languages and readback verification. Rationale: it is
      the only family exercising the verify ladder against a second vendor.

### Slice 7 — reporting and cadence

- [ ] Task 31. Write a one-screen comparison renderer over the safe layer and bump the report
      schema version. Rationale: a JSON blob nobody reads changes no decisions.
- [ ] Task 32. Record the cadence in the docs — notes family every CI run, replay on every
      prompt or executor change, full live run before any release touching the action path.
      Rationale: the three modes have very different costs and only the cheapest can be
      automatic.
- [ ] Task 33. Hold out a fixture subset never used for prompt tuning and say so. Rationale:
      twelve fixtures reduced to one number gets gamed by prompt tweaks that overfit.

### Slice 8 — audio-in tier, after Slice 7 is green

- [ ] Task 34. Add WAV fixtures through the real STT for a subset of commands, reported
      separately and never gating. Rationale: STT is a routing input and not merely a latency
      one, but folding it into the headline would make every model comparison also a microphone
      comparison. Do not synthesise the fixtures with TTS and report the result as an STT
      number — TTS speech flatters STT badly.

---

## Verification Criteria

- A single command runs the suite against the real connected servers with **no configuration
  file written first**, restores the house afterwards, and writes a run directory containing a
  safe report, a detail file, a cassette and one trace per fixture.
- The same command run on a different house either passes or reports
  `Skipped — no matching device` per fixture, and never fails for want of a mapping.
- `git status` is clean after a full live run, and no committed file contains a real device or
  room name.
- Two consecutive runs yield identical verdicts.
- The report distinguishes first-try routing failures from failures that survived recovery,
  per fixture and per language.
- Every failure class in v1's taxonomy has a fixture, and each fails when the corresponding
  Tier-1 protection is reverted.
- The room-command fixture asserts the lights changed **and** the co-located non-light entity
  did not.
- A Romanian command in an English-named house is scored for a Romanian reply by an explicit
  metric.
- Replaying a cassette reproduces the live run's verdicts with no network access.
- The notes family passes in CI with no house, no secrets and no private fixtures.
- v5 Task 9 is answerable by pointing at a report rather than re-reading four traces.
- The default build is unchanged: no new dependency in the shipped graph, size budget unmoved,
  fmt / clippy / tests clean.

---

## Potential Risks and Mitigations

1. **Discovery picks an unsuitable device — a light in a cupboard, a media player that is
   someone's TV mid-film.**
   Mitigation: deterministic selection recorded in the detail layer, an override flag to pin a
   device, safety domains excluded from selection, and the dry run naming every resolved
   target before anything moves.
2. **The suite leaves the house in a wrong state after a crash.**
   Mitigation: restore is a verified step, a failed restore aborts loudly, and the idempotency
   self-check proves restore works before the suite is trusted.
3. **A private noun reaches the repository.**
   Mitigation: fixtures name requirements and never devices, so there is no noun to leak; the
   run directory lives outside the tree; the safe/detail split keeps resolved names out of
   anything shareable.
4. **Real-house results are not comparable between runs, so no regression is detectable.**
   Mitigation: preconditions, the drift verdict, and replay — replay is where the ratchet
   actually lives.
5. **The cassette drifts from the house and replay becomes misleading.**
   Mitigation: `live-verify` before any release touching the action path; treat divergence as a
   stale cassette, not a fixture failure.
6. **No CI gate, so the suite rots.**
   Mitigation: the notes family gates CI end to end with no house data at all.
7. **A run is disruptive or expensive — music at night, climate cycling.**
   Mitigation: volume clamp, media fixtures skipped outside the window, short dwell with
   verified restore, and a dry run that names every device first.
8. **Four languages multiply authoring cost and the suite rots.**
   Mitigation: English and Romanian first, where traces ground the expectations; the other two
   only once the schema has settled.
9. **The benchmark becomes the optimisation target.**
   Mitigation: a held-out subset, an honest `suite_version`, and a standing rule that a prompt
   change justified here must also move a real trace.
10. **Measuring the pipeline hides a model regression.**
    Mitigation: keep the existing raw-model harness in
    `crates/fono-bench/src/assistant_tool_use.rs` as a named comparison mode; the difference
    between the two readings is the measured value of the action stack.

---

## Alternative Approaches

1. **Fixtures naming real devices via a private map** — v2's proposal. Rejected: it makes
   every new user write a configuration file before their first run, it goes stale silently
   when the house changes, and it tests only the naming quirks someone remembered to map.
   Discovery is strictly better on all three counts and leaks no more.
2. **Fixtures naming real devices directly, with the fixture files gitignored.** Simplest of
   all and perfectly private. Rejected because the fixture *logic* — the taxonomy, the
   assertions, the language matrix — is the part with lasting value, and gitignoring it means
   nobody else ever benefits from it and it is never reviewed.
3. **Fake house as default** — v1's recommendation. Cheaper and CI-able. Rejected on the
   stated requirement for real complexity; record–replay recovers most of the determinism and
   boundary fault injection recovers the provoked-failure cases.
4. **Real house with neither preconditions nor cassette.** Rejected: without a known starting
   state the same fixture scores differently between runs, which makes the suite useless for
   the regression question it exists to answer.
5. **Model-as-judge for all scoring.** Attractive for truthfulness and language, which are
   genuinely fuzzy. Rejected for routing and world state, where assertions are exact and free;
   worth revisiting for truthfulness alone if the mechanical check proves too blunt.
