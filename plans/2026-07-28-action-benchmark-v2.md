# Voice-action benchmark — real house, real path, no leaks

**Status:** proposal, ready for review. Not started.
**Supersedes:** `plans/2026-07-28-action-benchmark-v1.md`, whose fake-house default was
overruled. v1's failure taxonomy (M7) and its scoring split (M1) survive intact and are
referenced rather than repeated.
**Relates to:** `plans/2026-07-28-voice-actions-universal-first-v5.md` Task 9.

---

## Objective

Run real utterances, in four languages, through **the same code path Fono uses when the
user speaks to it**, against **the real MCP servers the user has connected**, and report:

1. **Routing** — right tool, right arguments, first try.
2. **Recovery** — when not, did Fono's own ladder rescue it inside the same turn.
3. **Truthfulness, language and speed** — did the reply honestly describe what happened,
   in the language asked, and how long did each stage take.

Subject to two hard constraints the design must satisfy, not work around:

- **The house is real.** All its ambiguity, its 200 devices, its odd naming, its latency.
- **Nothing about the house reaches the repository.** Not device names, not room names, not
  traces, not reports.

---

## The three decisions that follow from the constraints

### D1. Text in, voice out optional — the path already supports it

This needs almost no new machinery, which is the single most useful finding here.
`AssistantTurnInputs` already carries `pre_transcribed`
(`crates/fono/src/assistant.rs:193-198`): when set, `run_assistant_turn`
(`crates/fono/src/assistant.rs:276`) skips STT entirely and treats the string as the user's
turn — that branch is at `crates/fono/src/assistant.rs:318-343`. `tts` is already
`Option<Arc<dyn TextToSpeech>>` and documented as "text-only turn"
(`crates/fono/src/assistant.rs:151-154`). `overlay` is already `Option`
(`crates/fono/src/assistant.rs:192`).

So a text-in, silent, headless turn is: `pcm: vec![]`, `pre_transcribed: Some(utterance)`,
`tts: None`, `overlay: None`, a drained `action_tx`, and a real `Arc<dyn Assistant>`.
Everything else — the system prompt, the hint, the executor, the vendor ladder, the
readback — is the production path, byte for byte, because it *is* the production path.

The amendments needed are three, and all are small: a way to observe the reply text (the
pump already accumulates it for the `assistant:` summary line), a way to observe the tool
calls (the trace already records them on the `actions` lane —
`crates/fono/src/actions/mod.rs:410-415`, `:424-444`, `:546-563`), and a way to override
the model without editing the user's config.

**Do not add a second text-in path.** If a text-only turn needs a behaviour that does not
exist, add it to `run_assistant_turn` so the real voice path gets it too.

### D2. Layering — a feature-gated subcommand inside `fono`, not a hoist

v1 recommended hoisting `crates/fono/src/actions/` into a shared crate so `fono-bench`
could see it. The real-house decision changes the calculus. The harness now needs the
user's **actual** `Config`, `Paths`, `Secrets` and tool-catalogue database, because
`actions::build` (`crates/fono/src/actions/mod.rs:50-113`) reads all four. That is
binary-crate territory, and dragging it into `fono-bench` would mean reimplementing the
config and secrets resolution — the exact duplication this project has been bitten by
twice.

**Recommendation: `fono bench actions`, behind a non-default `bench-actions` feature.**

- The size budget is unaffected: `tests/check.sh --size-budget` builds default features
  (`crates/fono/Cargo.toml:36`), so a non-default feature costs the shipped artefact
  nothing. This is the same pattern the accel features already use.
- `fono-bench` becomes an *optional* dependency of `fono`, activated by that feature, so
  the fixture-loading, statistics and report-rendering code in
  `crates/fono-bench/src/assistant_tool_use.rs` is reused rather than reinvented.
- No hoist, no relocation diff, no risk to the shipped binary.

The hoist may still be worth doing later for v5 Track B Task 10, but it should not be a
prerequisite for this.

### D3. Privacy — fixtures name **roles**, a gitignored map names devices

This is the crux of "test the real house, leak nothing", and it is a schema decision, not
a redaction afterthought.

A committed fixture says:

- target `role:outdoor_light_secondary`, utterance template *"turn on the {outdoor_light_secondary}"*
- expected: that role on, `role:hallway_climate` unchanged

A gitignored `house.toml` on the user's machine maps each role to the real name, the real
room, and the real entity, and is the **only** file that contains a real noun. The
utterance is rendered at run time by substituting the map.

What this buys:

- The fixture *logic* — the failure taxonomy, the negative assertions, the language
  matrix — is committable, reviewable and shared. That is the part with lasting value.
- Another user with a different house writes their own map and runs the same suite.
- The suite is portable, which is the difference between a benchmark and one person's
  script.

Four artefact classes, with different rules:

| artefact | contains | where it goes |
|---|---|---|
| fixture files | roles, languages, assertions | committed |
| `house.toml` role map | real device and room names | gitignored, under the Fono state dir |
| traces | the entire system prompt, every device name, every utterance (`crates/fono-core/src/turn_trace.rs:45-64`) | gitignored run directory, never attached to an issue unredacted |
| report | two layers — see below | safe layer shareable |

The report splits in two. The **safe layer** is verdicts, per-fixture pass/fail by role id,
latencies, token counts, cost, language match — all of it publishable, none of it naming
anything. The **detail layer** is the model's literal arguments and the reply text, written
to a separate file in the run directory and never merged into the safe layer. Only the safe
layer is what a regression comparison reads, which means comparison works on a machine that
has never seen the house.

A redactor keyed off the role map can rewrite a trace or a detail file into shareable form
by substituting real names back into role tokens — useful when a failure needs discussing,
and deliberately a manual step so it is never the default.

---

## Repeatability without a fake house

The user overruled the fake house, and the reasons v1 gave for it do not vanish — they have
to be paid for differently. Four mechanisms, in order of importance:

### R1. Setup and restore, per fixture, bypassing the model

Before each utterance the harness drives the world into the fixture's declared precondition
using **direct MCP calls** — `mcp_client::call_tool`
(`crates/fono-assistant/src/mcp_client.rs:331`), never the assistant. After the assertion it
restores. This is what makes two runs comparable without a simulator, and it is the single
highest-value item in the plan: without it, "turn off the kitchen lights" scores differently
depending on whether they were already off.

A restore that fails must **abort the suite**, loudly. Leaving someone's house in a wrong
state because a benchmark crashed is not an acceptable failure mode.

### R2. World snapshot before and after — the "drifted" verdict

A real house has other actors: schedules, other people, a thermostat with opinions. Read the
world state immediately before and after the turn. If something changed that no tool in this
turn touched, the fixture's verdict is **`Drifted`**, not `Failed`. Collapsing drift into
failure is how a real-house suite becomes noise nobody trusts within a month.

### R3. The cassette — record at the MCP boundary, replay later

A thin recorder around `mcp_client::discover` (`crates/fono-assistant/src/mcp_client.rs:200`)
and `call_tool` (`:331`) writes every request and every verbatim response to a cassette in
the run directory. Three modes:

- **`live`** (default) — real servers, real devices, records a cassette.
- **`replay`** — cassette only, no network, no devices touched. Deterministic, fast, and
  runnable on a laptop in a different country.
- **`live-verify`** — replay a cassette against the live house and report where the house
  now answers differently. This is what keeps the cassette honest.

Replay is what gives regression testing its ratchet: change a prompt, replay yesterday's
cassette, see exactly which fixtures moved. Cassettes are as sensitive as traces — the
gitignored run directory, always.

### R4. Fault injection at the boundary, not a fake server

v1 wanted a fake house partly because a working house cannot be made to refuse on demand.
That capability is still needed — the Tier-1 retry ladder
(`crates/fono/src/actions/mod.rs:361-374`, `:391-396`, `:469-473`) is unreachable otherwise.
The cheap version keeps the real server and corrupts exactly one response: replace it with a
recorded refusal ("Received invalid slot info"), a partial success with a named `failed[]`
list, a timeout, or a connection error. Real house, injected fault, real recovery path.

---

## What is still missing from the proposal as stated

v1's list stands; these are new, or changed by the pivot to a real house.

### N1. Physical safety and courtesy are now part of the design

A suite that switches real things needs rules, and they belong in code, not in the operator's
memory:

- A **role allowlist** — only roles named in `house.toml` may be touched. A model that
  invents an entity cannot reach anything undeclared.
- **Quiet hours** — the media-playback fixtures refuse to run outside a configured window,
  and clamp volume regardless.
- **Cost-bearing devices** — air conditioning and heating get a short dwell and an
  unconditional restore, with the restore verified before the next fixture starts.
- **A permanent denylist** — locks, garage doors, alarms, anything safety-relevant is never
  a benchmark target, whatever the map says.

The hallway AC is the interesting case for exactly the reason v1 identified: it is the
*bystander* of the domain-less room command (v5 traces `…456`/`…468`), so the fixture that
turns on the office light must assert the AC stayed off — and must therefore be able to
observe it without ever being allowed to switch it.

### N2. The language matrix is two-dimensional, not one

"Four languages" is really *house language* × *speaker language*, and the axes are not
symmetric. An English-named house addressed in Romanian is the recorded failure the room
hint exists to fix (`crates/fono/src/actions/mod.rs:145-153`). The same house addressed in
English is trivial. Declare both dimensions; the interesting cells are the mismatched ones,
and the fixture set should be weighted toward them rather than spread evenly.

### N3. Model overrides must layer over the real config, not replace it

Comparing five models must not mean editing the user's config five times. The harness takes
backend and model overrides applied on top of the loaded `Config`, leaving tools, servers
and secrets untouched. Everything else about the turn stays exactly as the user has it —
which is the whole point of using the real path.

### N4. Budget the run before starting it

Fixtures × languages × models × iterations multiplies fast, and every cell is a real device
movement and, on cloud rows, real money. A dry run that prints the cell count, the estimated
cost and the list of devices that will be touched — and asks for confirmation once — is
cheap to build and prevents the run nobody meant to start.

### N5. The suite must be idempotent, and that must be tested

Running the suite twice in a row must produce the same verdicts. If it does not, the restore
step is broken. Make that a first-class self-check rather than something discovered three
weeks in.

### N6. CI still needs something, and the boring server is it

Real-house-default means no CI gate, and a benchmark with no ratchet decays. Two answers,
both partial and both worth having: **replay** of a committed *synthetic* cassette, and the
generic filesystem/notes server below, whose fixtures contain no private nouns at all and
can run end-to-end in CI against a temp directory.

---

## Which second and third MCP servers — the direct answer

Criteria: most people can actually run it, it stresses a **different** failure mode than
Home Assistant, it is free and local where possible, and it is safe to write to.

### Second: a notes / list server backed by the filesystem — **the highest-value addition**

A local MCP server that appends to and reads back a text file or a note store. Why it earns
its place:

- It tests a completely different axis: **verbatim fidelity of a free-text argument**. Home
  Assistant tests whether the model picks the right *name from a closed set*. A note server
  tests whether the model writes down *what you actually said* instead of a tidy paraphrase.
  Nothing in the suite tests that today, and for a dictation-adjacent tool it matters.
- It is the natural place to catch **diacritic and encoding damage** end to end — a Romanian
  or Spanish sentence going in and coming back byte-identical.
- Verification is exact and free: read the file, compare strings. No vendor knowledge needed.
- It is **safe, deterministic, and contains no private nouns**, so its fixtures are
  committable and it can gate CI. That makes it the answer to N6 as well.
- It doubles as the **vendor-neutrality control** v1 asked for (M10): no Home Assistant
  vocabulary anywhere, proving nothing in the pipeline branches on vendor.

### Third: a calendar server — **the second-vendor verification test**

CalDAV, or any calendar MCP, pointed at a dedicated test calendar. Why:

- It introduces **relative temporal reasoning across languages** — "next Tuesday at half
  three", "poimâine la zece", "dans deux semaines". Models produce confidently wrong ISO
  timestamps here, and the error is invisible without a check.
- It has a natural readback (list events in a window), so `VerifyClass::PostCondition` and
  the `confirm` ladder (`crates/fono/src/actions/mod.rs:535-565`) get exercised against a
  **second vendor** — currently that whole rung is proven only by Home Assistant.
- It surfaces timezone and locale handling, which nothing else in the suite touches.

Caveat: it writes to a real calendar, so it needs a dedicated one and the same restore
discipline as the house.

### Considered and not recommended

- **A separate media server (MPD, Spotify).** Redundant — the Google-speaker case is already
  reached through Home Assistant's media player, and a second one adds surface without a new
  failure mode.
- **Web search / fetch.** Genuinely interesting because it is the canonical "tool that
  returns far more text than you wanted", which stresses context growth and summary
  honesty — but it is non-deterministic and network-bound. Worth an axis later, not a
  fixture family now.
- **Fono's own MCP server** (`crates/fono-mcp-server/src/tools/`). Points inward: those are
  tools an agent calls *into* Fono, the opposite direction. One exception worth remembering —
  `fono.confirm` as a tool the assistant may call would let the suite test multi-turn
  clarification, which is otherwise untestable. Later.

---

## Implementation Plan

### Slice 0 — the seam

- [ ] Task 1. Add a non-default `bench-actions` feature to the `fono` crate and a hidden
      `fono bench actions` subcommand behind it, with `fono-bench` as an optional dependency.
      Rationale: the harness needs the user's real config, secrets and tool catalogue, which
      only the binary crate can resolve; a non-default feature keeps the shipped artefact and
      the size gate untouched.
- [ ] Task 2. Build a headless text-in turn driver that calls `run_assistant_turn` with
      `pre_transcribed` set, no PCM, no TTS and no overlay, and returns the reply text, the
      tool calls, and the stage timings. Rationale: this is the whole "normal Fono path"
      requirement, and the fields already exist — anything that has to be added should be
      added to the shared pump so the voice path benefits too.
- [ ] Task 3. Add backend and model overrides layered on top of the loaded config.
      Rationale: N3 — comparing models must not mean mutating the user's configuration.

### Slice 1 — privacy scaffolding, before any real device is touched

- [ ] Task 4. Define the role vocabulary and the gitignored `house.toml` role map, and render
      fixture utterances by substitution at run time. Rationale: D3 — this is what makes real
      fixtures committable and portable, and it must exist before the first fixture is
      written or the private nouns will be baked in.
- [ ] Task 5. Split the report into a safe layer and a detail layer, and write every run into
      a gitignored run directory under the Fono state dir. Rationale: regression comparison
      must work on the safe layer alone, on a machine that has never seen the house.
- [ ] Task 6. Add a redactor that rewrites a trace, cassette or detail file into shareable
      form using the role map, as an explicit manual command. Rationale: failures need
      discussing; the substitution must be deliberate rather than a default anyone forgets.
- [ ] Task 7. Document in the harness output that the run directory is a full transcript of
      the home. Rationale: the warning at `crates/fono-core/src/turn_trace.rs:45-64` will
      otherwise be relearned the first time a run is attached to an issue.

### Slice 2 — making a real house repeatable

- [ ] Task 8. Implement per-fixture setup and restore via direct MCP calls that bypass the
      assistant, with an abort-the-suite-loudly failure mode on a failed restore. Rationale:
      R1 — without a known precondition the same fixture scores differently run to run, and
      an unrestored house is a real-world harm.
- [ ] Task 9. Snapshot world state before and after each turn and introduce a `Drifted`
      verdict distinct from `Failed`. Rationale: R2 — other actors in the house will perturb
      runs, and mislabelling that as failure destroys trust in the suite.
- [ ] Task 10. Add the role allowlist, the quiet-hours gate, the dwell-and-restore rule for
      cost-bearing devices, and the permanent safety denylist. Rationale: N1.
- [ ] Task 11. Add a dry-run mode that prints cells, estimated cost and the devices that will
      be touched, and confirms once. Rationale: N4.
- [ ] Task 12. Add a suite-idempotency self-check that runs the suite twice and requires
      identical verdicts. Rationale: N5 — this is the only thing that proves Task 8 works.

### Slice 3 — record and replay

- [ ] Task 13. Add a recorder at the `discover` / `call_tool` boundary writing a cassette of
      verbatim requests and responses. Rationale: R3 — the boundary is the only place where
      the house's full behaviour is observable in one stream.
- [ ] Task 14. Add `replay` mode driving the same pipeline from a cassette with no network
      and no devices. Rationale: this is the regression ratchet and the only way a prompt
      change can be evaluated without re-running the house.
- [ ] Task 15. Add `live-verify` mode that replays a cassette against the live house and
      reports divergence. Rationale: a cassette silently drifting from the house is the
      failure mode that would make replay results actively misleading.
- [ ] Task 16. Add boundary fault injection for the recorded failure classes — hard refusal
      with the server's own text, partial success with a named `failed[]` list, timeout,
      unreachable. Rationale: R4 — the Tier-1 retry ladder is otherwise never exercised.

### Slice 4 — scoring

- [ ] Task 17. Score the model's first tool call and the final outcome separately and report
      both. Rationale: v1 M1 — the delta between them *is* the measured value of Tier 1, and
      it is what v5 Task 9 needs.
- [ ] Task 18. Assert world state after the turn, including negative assertions such as the
      climate role staying untouched. Rationale: the domain-less room command cannot be
      caught by inspecting the tool call alone.
- [ ] Task 19. Add a truthfulness check comparing the reply against the observed world state,
      at minimum catching a claim of success over a device that did not move. Rationale: the
      project treats this as the worst available failure and it is currently unmeasured.
- [ ] Task 20. Add an explicit reply-language metric rather than inferring it from expected
      substrings. Rationale: v5 F35; it also makes adding French and Spanish cheap.
- [ ] Task 21. Report per-fixture n, pass count and a confidence interval, and flag
      single-iteration runs as indicative. Rationale: v1 M9 — otherwise the first prompt
      tweak is validated by noise.
- [ ] Task 22. Add time-to-first-audio-equivalent and time-to-effect to the latency set,
      keep the existing prompt-cache fields, and add tokens and cost per command. Rationale:
      v1 M5 — and the cache fields remain the largest measured local-model lever.

### Slice 5 — fixtures

- [ ] Task 23. Extend the fixture schema for roles, preconditions, expected and forbidden
      world state, expected reply language, and whether a retry is permitted. Rationale: the
      "never retry a relative change" rule from v5 Task 3 has no fixture, and a rule without
      one gets reintroduced by accident.
- [ ] Task 24. Author the v1 M7 failure taxonomy as role-based fixtures in English and
      Romanian. Rationale: those two are grounded in recorded traces, so expectations are
      evidence rather than guesswork.
- [ ] Task 25. Add the dimmable-light, media-playback and climate fixtures, with the climate
      role appearing as both target and bystander. Rationale: they widen the surface past
      on/off, which is where the wrong-tool failure lives (v5 F33).
- [ ] Task 26. Add French and Spanish once the schema has stopped moving, authored by someone
      fluent, with house names left in the house's own language. Rationale: translationese is
      not what users say, and the untranslated names are exactly what the room hint protects.
- [ ] Task 27. Declare the house-language × speaker-language matrix and weight fixtures toward
      the mismatched cells. Rationale: N2.

### Slice 6 — the second and third servers

- [ ] Task 28. Add the filesystem-backed notes/list server family with verbatim-fidelity and
      diacritic round-trip fixtures, containing no private nouns. Rationale: it tests an axis
      nothing else does, it is the vendor-neutrality control, and it is the only family that
      can gate CI.
- [ ] Task 29. Wire the notes family into CI end to end against a temp directory. Rationale:
      N6 — a benchmark with no ratchet decays, and this is the part that needs no house.
- [ ] Task 30. Add the calendar family against a dedicated test calendar, with relative
      temporal expressions in all four languages and readback verification. Rationale: it is
      the only fixture family that exercises the verify ladder against a second vendor.

### Slice 7 — reporting and cadence

- [ ] Task 31. Write a one-screen comparison renderer over the safe layer, and bump the
      report schema version. Rationale: a JSON blob nobody reads changes no decisions.
- [ ] Task 32. Establish the cadence in the docs — notes family every CI run, replay on every
      prompt or executor change, full live run before any release touching the action path.
      Rationale: the three modes have different costs and only the cheapest can be automatic.
- [ ] Task 33. Hold out a fixture subset never used for prompt tuning and say so. Rationale:
      v1 M11.

### Slice 8 — audio-in tier, after Slice 7 is green

- [ ] Task 34. Add WAV fixtures through the real STT for a subset of commands, reported
      separately and never gating. Rationale: STT is a routing input, not just a latency one,
      but folding it into the headline would make every model comparison also a microphone
      comparison. Do not synthesise the fixtures with TTS and report the result as an STT
      number — TTS speech flatters STT badly.

---

## Verification Criteria

- A single command runs the suite against the real connected servers, restores the house
  afterwards, and writes a run directory containing a safe report, a detail file, a cassette
  and one trace per fixture.
- `git status` is clean after a full live run, and no committed file contains a real device
  or room name.
- Running the suite twice consecutively yields identical verdicts.
- The report distinguishes first-try routing failures from failures that survived recovery,
  per fixture and per language.
- Every failure class in the v1 M7 table has a fixture, and each fails when the corresponding
  Tier-1 protection is reverted.
- Switching on the office light asserts the lights changed **and** the climate role did not.
- A Romanian command in an English-named house is scored for a Romanian reply by an explicit
  metric.
- Replaying a cassette reproduces the live run's verdicts with no network access.
- The notes-server family passes in CI with no house, no secrets and no private fixtures,
  demonstrating the pipeline does not branch on vendor.
- v5 Task 9 is answerable by pointing at a report rather than re-reading four traces.
- The default build is unchanged: no new dependency in the shipped graph, size budget
  unmoved, fmt / clippy / tests clean.

---

## Potential Risks and Mitigations

1. **The suite leaves the house in a wrong state after a crash.**
   Mitigation: restore is a verified step, a failed restore aborts the suite loudly, the
   safety denylist keeps anything consequential out of reach, and the idempotency self-check
   proves restore works before the suite is trusted.
2. **A private noun reaches the repository.**
   Mitigation: roles in fixtures, real names only in a gitignored map, safe/detail report
   split, run directory outside the tree, and a manual-only redactor. Reinforce with a
   pre-commit check that the fixture directory contains no string from the role map.
3. **Real-house results are not comparable run to run, so no regression is ever detectable.**
   Mitigation: preconditions, the drift verdict, and the replay cassette — replay is where
   the actual ratchet lives.
4. **The cassette drifts from the house and replay results become misleading.**
   Mitigation: `live-verify` mode, run before any release that touches the action path;
   treat divergence as a stale cassette rather than a fixture failure.
5. **No CI gate, so the suite rots.**
   Mitigation: the notes family gates CI end to end with no house data at all, and replay of
   a committed synthetic cassette covers the pipeline shape.
6. **A benchmark run is disruptive or expensive — music at night, air conditioning cycling.**
   Mitigation: quiet hours, volume clamp, short dwell with verified restore, and a dry run
   that names every device before anything moves.
7. **The four-language matrix multiplies authoring cost and the suite rots.**
   Mitigation: English and Romanian first, where traces ground the expectations; the other
   two only once the schema has settled, so they are authored once.
8. **The benchmark becomes the optimisation target.**
   Mitigation: a held-out subset, an honest `suite_version`, and a standing rule that a
   prompt change justified here must also move a real trace.
9. **Measuring the pipeline hides a model regression.**
   Mitigation: keep the existing raw-model harness in `crates/fono-bench/src/assistant_tool_use.rs`
   as a named comparison mode; the difference between the two readings is the measured value
   of the action stack and the most persuasive number available.

---

## Alternative Approaches

1. **Fake house as default, real house as opt-in** — v1's recommendation. Cheaper, CI-able,
   provokes failures on demand. Rejected on the stated requirement that results reflect real
   complexity; the record–replay cassette recovers most of the determinism benefit without
   giving up fidelity, and boundary fault injection recovers the rest.
2. **Real house with no cassette and no preconditions.** Simplest possible. Rejected because
   without a known starting state the same fixture scores differently between runs, which
   makes the suite useless for exactly the regression question it exists to answer.
3. **Hoist `crates/fono/src/actions/` into a shared crate and keep the harness in
   `fono-bench`.** Architecturally tidier and still worth doing eventually. Rejected as a
   prerequisite because the harness's real dependency is the user's config, secrets and
   catalogue resolution, none of which the hoist moves.
4. **Model-as-judge for all scoring.** Attractive for truthfulness and language, which are
   genuinely fuzzy. Rejected for routing and world state, where assertions are exact and
   free; worth revisiting for truthfulness alone if the mechanical check proves too blunt.
