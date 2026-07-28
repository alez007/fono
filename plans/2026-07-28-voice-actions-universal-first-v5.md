# Voice actions — universal fixes first

**Status:** ready for implementation.
**Supersedes:** `plans/2026-07-26-voice-actions-v4.md` §12 (Phases 2–6). v4 remains
the evidence record — its §1 findings F1–F32 and its §5–§11 design (catalogue store,
verification ladder, shortcut semantics) are unchanged and still authoritative. This
document replaces only the *phase ordering and scope*, which v4 had accumulated across
three sessions in the order bugs were found rather than in the order they should be
fixed.

**Why a reshape:** the previous ordering put the single highest-leverage, universally
applicable fix at item nineteen, behind eighteen others — several of which branched on
model class and so contradicted v4's own §10 axiom ("the model-agnostic boundary").
Reordering by leverage and filtering by universality changes both what gets built and
what gets built first.

---

## Objective

Make a tool call succeed, or recover when it does not, on **any** model and **any** MCP
server — before attempting anything that depends on knowing which model or which server
is in play.

Concretely: a spoken command should move the device it named, and only that device; a
command the server refuses should be corrected without the user speaking again; and a
reply should be in the language it was asked in.

---

## The universality test

Every candidate fix is scored on three questions before it enters Tier 1:

1. **Does it work on a cloud model and a local one?**
2. **Does it work on Home Assistant and on an MCP server we have never seen?**
3. **What happens as the model gets smarter — is it redundant, or is it harmful?**

Question 3 is the discriminator. A fix a smarter model renders *redundant* is fine: it
costs nothing when it never fires. A fix a smarter model renders *harmful* — withholding
device names, for instance, which a capable model uses well — is a workaround, not a fix,
and does not belong in the plan at any tier.

**The pattern that falls out of the test:** make failure *recoverable* rather than making
the model *less likely to fail*. Recovery is universal — it uses the server's own words
and needs no knowledge of the model. Prevention is model-specific — grammars only exist
on the local backend, prompt tuning helps some models more than others. Recovery first;
prevention only for the shortfall recovery leaves behind.

### Scoring of every candidate raised so far

| candidate | cloud + local | any vendor | as models improve | tier |
|---|---|---|---|---|
| Retry a refused call with the server's error | yes | yes — the error is the server's | redundant | **1** |
| Retry the missed devices of a partial success | yes | yes — via `Admission` | redundant | **1** |
| Validate arguments against the published schema | yes | yes — schema is the server's | never fires | **1** |
| Behavioural instructions last in the head | yes | yes | harmless | **1** |
| Compress the room hint | yes | yes | harmless | **1** |
| Say which tool to use, not just how to target | yes | yes | harmless | **1** |
| Warm the composed head | local mechanism, universal intent | yes | still helps | **B** |
| Lazy grammar (shape + tool name) | local only — cloud has native tool calling | yes | still helps | 2 |
| Grammar constraining argument values | local only | yes | still helps | 2 |
| Shortcuts / Tier 0 replay | yes | needs verification, which is vendor-shaped | still helps | 2 |
| ~~Suppress the device list on small models~~ | yes | yes | **harmful** | **rejected** |
| ~~Trim the tool catalogue by model class~~ | yes | yes | **harmful** | **rejected** |

The two rejections are recorded rather than deleted: both were proposed in this
investigation, both fail question 3, and both would otherwise be re-proposed the next
time a small model misbehaves.

---

## Evidence this plan answers

Four traces, one local model (`gemma-4-e2b`), one house, four consecutive commands.
Recorded in full as F33–F35 below; v4's F1–F32 are unchanged.

| trace | spoken | tool the model chose | outcome |
|---|---|---|---|
| `assistant-1785184389-0002` | *"Porneste luminile in dormitorul principal"* | `HassLightSet {area, brightness: 10, color: "#FFFFFF"}` | refused — *"invalid slot info"* |
| `assistant-1785184438-0003` | *"Pornește lampa de sare în dormitorul principal"* | `HassLightSet {area, brightness: 10, color: "#FFFFFF"}` | refused — identical |
| `assistant-1785184456-0004` | "Turn on the light in the master bedroom" | `HassTurnOn {area}` — no `domain` | partial: rollers and curtains moved, lights did not respond |
| `assistant-1785184468-0005` | "Turn off the light in the master bedroom" | `HassTurnOff {area}` — no `domain` | partial, mirror image |

- **F33 — the failure was tool *selection*, not room naming.** `Master bedroom` was
  correct in all four. The model reached for the brightness-and-colour tool for a plain
  switch-on and invented both a brightness and a colour. The room hint
  (`crates/fono/src/actions/mod.rs:170-183`) is ~1200 characters about *targeting* and
  contains not one word about *choosing among 23 near-identical `Hass*` signatures*.
- **F34 — nothing recovered.** All four turns ended with an apology and a request for the
  user to try again. Two were hard refusals the server explained in plain text; two were
  partial successes where `Admission::PartlyWorked` correctly named the four devices that
  did not respond — and then stopped. The information needed to retry was in hand and
  unused in every case.
- **F35 — the 1510-token head displaces the instructions.** The system prompt ends with
  *"Match the user's language."* Both Romanian commands received English replies. Of 6394
  prompt characters, ~1900 are device names, ~3400 are tool signatures, ~1200 are hint
  prose, and ~330 are what the user told the assistant to be — 5% instruction, 95% index,
  with the instruction furthest from the generation point.
  **Caveat, and it is the important half of the finding:** the naive reading ("the index
  is too big") produces model-specific fixes that fail question 3. The universal reading
  is that *position* is wrong, not size — and position is free to fix.

**Also confirmed by these traces:** the head-pin work from F30/F31 is landing. The first
turn paid 1438 tokens / 25.4 s and pinned a 1510-token `f8_system` entry; the next three
restored it in 3 ms and prefilled 37, 2 and 2 tokens respectively. Total turn time fell
39.4 s → 7.4 s. Track B is what removes the remaining once-per-daemon-start cost.

---

## Task 9 measurement — the same four commands, after Tier 1

Traces `assistant-1785218020-0002`, `-1785218059-0003`, `-1785218132-0004`,
`-1785218202-0005`. Same model, same house, same four commands, in order.

| trace | spoken | what happened |
|---|---|---|
| `…020-0002` | Romanian, "turn the lights on" | partial — lights answered, reply in **English** |
| `…059-0003` | Romanian, "the salt lamp" | nothing ran |
| `…132-0004` | English, "turn on the light" | partial — **curtains and roller moved again**; retry fired and was **spoken, not run** |
| `…202-0005` | English, "turn off the light" | nothing ran |

- **F36 — the retry was correct and unreachable. This is the severe one.** In `…132-0004`
  the model, handed the partial-failure result, did exactly what Task 1 asked of it: it
  emitted two corrective calls, both `HassTurnOn` with `domain: ["light"]`, naming the
  devices that had not responded. Neither was executed. Both were **read out loud as raw
  JSON** — 289 characters of tail, 24 seconds of synthesised speech.
  The cause is that the correction pass treated the model's output as prose. On the first
  pass a tool call is recognised, held back and executed; on the correction pass the same
  text was routed to the speech splitter. So Tier 1's central mechanism has never actually
  been exercised end to end, and the four traces do **not** measure what Task 9 intended.
  The invariant this violated, now enforced unconditionally: *a tool call is never
  speakable, on any pass.*
- **F37 — the language instruction is not enough on its own.** Task 5 moved *"Match the
  user's language"* to the end of the head, immediately before generation, and both
  Romanian commands still received English replies. So position was necessary and not
  sufficient. The prompt asks the model to *infer* the language from the transcript; a
  weak model reads a house full of English device names and answers in English. The
  detected language was known to Fono the whole time — `stt.transcribe` reports it, and
  `AssistantContext::language` was carried to every backend and **read by none**.
  Fix: state the language as a fact per turn, in the volatile tail beside the speaker note,
  rather than hoping it is deduced. Universal — every backend gets the same note, and a
  strong model that was already correct sees a statement it agrees with.
- **F38 — a rule can be present and still be read as optional.** `…132-0004` sent a bare
  `{"area": "Master bedroom"}` and moved the curtains, with the Task 6 hint in the prompt
  and its domain rule *stated*. Two things were wrong with how it was stated: the sentence
  opened with the permission (*"act on the room in one call"*) and only afterwards
  qualified it, and the qualification was phrased as advice (*"pass that kind as the
  domain"*) rather than an obligation. Fix: the obligation leads, says **required**, and
  the one-call economy is a separate rule so it cannot be read as licence to omit the
  domain. Ordering is now asserted by test, not just the wording.

**What this says about the plan's method.** Tier 1 was ordered by leverage and the ordering
was right — but F36 shows a fourth question belongs beside the three universality
questions: *can this mechanism be observed working?* The retry passed every unit test,
shipped, and was dead on arrival in the one code path that mattered. Nothing asserted that
a corrective call was executed rather than spoken, because the assertion lived in a
different crate from the bug.

---

## Implementation Plan

### Tier 1 — universal correctness

Ordered by leverage. Tasks 1–3 are one coherent change and should land together.

- [x] **Task 1. Hand a refused call back to the model, with tools still offered, once.**
      The server's own error text becomes the tool result; the model may correct its call
      and retry inside the same turn. Rationale: this is the single highest-leverage item
      in the plan — it addresses F34 directly, would have rescued three of the four
      traces, and needs no knowledge of the model or the vendor because the corrective
      information is text the server produced. Note the constraint it must not break: the
      wording pass is deliberately tool-less
      (`crates/fono-assistant/src/openai_compat_chat.rs:603-608`), so this is a narrow,
      documented exception, not a general relaxation.
- [x] **Task 2. Do the same for a partial success.** When `Admission::PartlyWorked`
      names devices that did not respond, offer the model the chance to act on those
      specifically rather than only reporting them. Rationale: traces `…456` and `…468`
      reported honestly and stopped; the failed names are already carried through
      `crates/fono/src/actions/vendor.rs` and reach the model as prose today.
- [x] **Task 3. Bound the retry, explicitly.** Exactly one extra round; only after a
      refusal or a partial failure; only where the intent is an absolute end state; never
      chained; never after a model-chosen second call. Rationale: v4 §9's constraint.
      Idempotence currently holds *by accident* — `desired_state` returns `Some` only for
      `HassTurnOn`/`HassTurnOff` — and an accident is not a rule. Without this the
      one-action-per-turn guarantee is silently deleted.
- [x] **Task 4. Validate arguments against the tool's published JSON schema before
      sending.** On failure, do not send: return the validation error to the model as a
      tool result and let Task 1's retry correct it. Rationale: `color: "#FFFFFF"` is not
      a valid Home Assistant colour and never needed to reach the house. The schema is
      already stored per row in the catalogue and already rendered into the prompt, so
      this adds no discovery and no vendor knowledge. Natural extension of
      `drop_empty_arguments` (`crates/fono/src/actions/mod.rs:234-254`) from *blank* to
      *invalid*.
- [x] **Task 5. Move the behavioural instructions to the end of the composed head**,
      after rooms, devices and the tool catalogue. Rationale: F35 — *"Match the user's
      language"* currently sits ~1450 tokens from the generation point and was ignored
      twice. Recency helps a weak model and costs a strong one nothing. Must remain a
      single stable block so the head stays a cacheable byte prefix; this interlocks with
      Track B Task 12 and the two should be designed together even though they ship for
      different reasons.
- [x] **Task 6. Compress the room hint to its load-bearing sentences.**
      `crates/fono/src/actions/mod.rs:170-183` is the longest prose block in the prompt
      and it still failed to prevent a domain-less room command in two of four traces.
      Rationale: length is not instruction strength; a shorter, sharper hint does the same
      job better on every model. Keep the three claims that carry weight — never invent a
      room name, name the kind of device alongside the area, do not narrow a named device
      to a room — and delete the justifying prose around them.
- [x] **Task 7. Say which tool to use, not only how to target it.** Add one sentence
      distinguishing the on/off tool from the brightness-and-colour tool, and stating that
      the latter is only for requests that actually mention brightness or colour.
      Rationale: F33 — the hint is silent on selection while 23 near-identical signatures
      compete. This is a prompt fix for a prompt-shaped failure, universal by
      construction, and independent of how many tools the user happens to have.
- [x] **Task 8. Do not send an optional argument the request did not imply.** Distinct
      from Task 4: a schema-*valid* `brightness: 10` that nobody asked for is still wrong,
      and would have been accepted by the house. Rationale: needs a defensible rule for
      "implied"; the conservative version — drop optional arguments the user's words do
      not support when a simpler sibling tool exists — is the safe starting point, and
      Task 9 will show whether it is sufficient.
- [x] **Task 9. Re-measure the same four commands and gate Tier 2 on the result.**
      Assert on *tool chosen* and *arguments sent*, not on latency. Rationale: every claim
      in Tier 1 is a hypothesis until a trace agrees. This task is the decision point for
      whether Tier 2 is needed at all, and it is the reason Tier 2 is not being built
      speculatively. **Measured 2026-07-28 — see F36–F38.** The verdict is that Tier 1's
      retry was *correct and unreachable*: it fired, produced two well-formed corrective
      calls, and both were read aloud as JSON instead of executed. Tier 2 is **not**
      unblocked by this result, because the measurement did not test what it was meant to.
      Task 9 must be re-run once F36 is fixed.

**Status, 2026-07-28.** Tasks 1–8 shipped. Two qualifications, recorded so the
measurement in Task 9 is read honestly:

- **Task 5 shipped after Track B Task 10, as planned.** The behavioural rules now travel
  apart from the context on `AssistantContext::instructions` and are rendered *behind*
  the tool block by the one shared composer, so the ordering rule exists in exactly one
  place and cannot drift between the warm path and the reply path — which is the failure
  that defeated F28 and F31. Head order is now: rooms and devices → tool catalogue →
  behavioural rules → speaker note. Only the last is volatile.
- **Task 8 shipped as a prompt rule, not as code.** *"Fill in only the arguments the user
  actually asked for"* is rule 5 of the room hint. Enforcing it mechanically needs a
  defensible definition of "implied", which no trace has yet justified — the observed
  failure (`brightness: 10`, `color: "#FFFFFF"`) is also caught by Task 4, because the
  colour was schema-invalid. If Task 9 shows a schema-*valid* invented argument still
  getting through, the code version becomes justified and belongs here.

### Track B — latency (independent of Tier 1; may proceed in parallel)

Local in mechanism, universal in intent: this is Fono doing for a local model what a
cloud provider already does server-side. It cannot make an answer wrong, so it carries no
correctness risk and does not compete with Tier 1 for review attention.

- [x] **Task 10. One composition function for the head** — `prompt_main` → rooms and
      devices → tool catalogue → behavioural instructions — shared by the warm path and
      the request path. Rationale: a pinned checkpoint is reusable only if the live prompt
      is rendered by the code that saved it (F28's lesson). Today composition is split
      across `fono` (`crates/fono/src/session.rs:3325-3335`) and `fono-assistant`
      (`crates/fono-assistant/src/llama_local.rs:2064-2077`), so neither crate can produce
      the whole string and drift is structural rather than accidental.
- [x] **Task 11. Move the speaker note behind the tool block.** Rationale: the identity
      line is the most volatile element in the prompt and currently sits *in front of* the
      largest stable block, so every change of recognised speaker invalidates the entire
      catalogue behind it. `crates/fono/src/session.rs:232-234` already states the correct
      rule; the tool block is appended by another crate afterwards, which breaks it.
- [x] **Task 12. Carry the composed head in the warmup request and the hotkey snapshot.**
      `assistant_cache_warmup` (`crates/fono/src/session.rs:208-217`) sends bare
      `prompt_main` — 72 tokens of a 1510-token head — and the hotkey path
      (`crates/fono/src/session.rs:2903-2911`, `:2979-2987`) sends the same. Rationale:
      the catalogue is on disk and needs no network, so there is nothing to fetch; the
      warm path is simply warming the wrong string.
- [x] **Task 13. Re-warm on the four triggers** — tool enable/disable
      (`crates/fono/src/daemon.rs:4475-4487`), a discovery pass that changed the catalogue,
      config reload, model change — idempotent, debounced, fire-and-forget. Rationale: v4
      §6 has always required this. Re-warming when nothing changed is nearly free: the key
      is content-addressed and `build_prompt_prefix_cache` short-circuits on
      `cache.contains`; when it *has* changed, `insert_pinned` releases the stale pin
      cleanly. So "once at startup, plus on change" is both sufficient and safe.
- [x] **Task 14. Retire the `pin_prefix: ctx.history.is_empty()` heuristic**
      (`crates/fono-assistant/src/llama_local.rs:2109-2113`) in favour of the explicit
      head pin. Rationale: it pins only when the first turn of a conversation happens to
      pay for the head, and it captures whatever volatile decoration that turn carried.
- [x] **Task 15. Pin the invariant and make it visible.** A test that the warmed head is a
      byte prefix of the live prompt across speaker/no-speaker, tools/no-tools and both
      chat templates; trace and `fono doctor` reporting asserted on
      `decoded_prefix_tokens`. Rationale: this exact invariant has broken twice (F28,
      F31), and `cold_prefills` has now reported a healthy `0` on three separate traces
      that were paying full freight — it must never be the assertion.

**Status, 2026-07-28.** Tasks 10–15 shipped. Notes for whoever reads a trace next:

- **Task 14 was narrowed, not deleted.** `pin_prefix` now also requires
  `ctx.speaker_note.is_none()`. With the note composed in by the backend, a first turn
  from a recognised speaker would otherwise pin *head + note* over the warm path's bare
  head, and the next speaker would miss. The heuristic survives only as a fallback for a
  daemon whose warm never ran.
- **`for_backend` is now on the warm path too.** A backend that cannot invoke tools is
  given none and told so instead — a different head. Warming the other one would pin a
  checkpoint that is not a prefix of anything ever sent. Harmless today, since only the
  local backend caches and it can act, but it is exactly the F28/F31 drift class.
- **Task 15's doctor line is an estimate**, four characters per token, and reports the
  head *size* rather than what a turn actually decoded. The assertion that matters is
  still `decoded_prefix_tokens` in a trace.

### Tier 2 — only if Task 9 shows Tier 1 fell short

Not started, not designed further, deliberately.

- [ ] **Task 16. Lazy grammar for tool-call shape and tool name.**
      `LlamaSampler::grammar_lazy` is in the pinned `llama-cpp-2` 0.1.150 — no new
      dependency, no binary growth. Triggers on `<tool_call>`, so prose stays
      unconstrained. Local backends only; cloud backends have native structured tool
      calling and gain nothing.
- [ ] **Task 17. Grammar constraining argument values to the schema.** Only if Task 4's
      validate-and-retry proves too slow or too lossy — prevention saves the round trip
      that recovery spends. Scope limit from v4 §14: schema *enums and types* only, never
      room or device names. Constraining a value the model would otherwise refuse to
      guess converts a clean "I could not find that" into a confident wrong action.
- [ ] **Task 18. Shortcuts / Tier 0 replay.** Design unchanged from v4 §8–§9 and the
      answers already given to §15 Q3: a `#/actions` route carrying the retrieved tools,
      rows for tools that have run only once and are not yet on the fast path, last run
      and who and how long, many phrases per action, user-authored phrases permitted but
      never trusted more than learned ones. Deferred behind correctness for one reason: a
      shortcut replays a routing decision, and replaying a *wrong* routing decision faster
      is worse than not having shortcuts at all.

### Debts carried forward from v4 (small, concrete, still owed)

- [ ] **Task 19. No-op detection.** `confirms` counts an already-on light as `Confirmed`
      (v4 §7.2) — correct for wording, wrong as promotion evidence. Gates Task 18; a
      correctness fix regardless of whether shortcuts ever ship.
- [ ] **Task 20. Give the live house test a permanent home under `tests/`.** It is still
      `tmp/ha-recon/live_light_test.py` and will vanish when `tmp/` is pruned (v4 §12.1).
- [ ] **Task 21. Pin reasoning-off on the action path with a test**, across all backends,
      including the scope limit that Fono's own LLM server must not force it. Guards a
      4–14× lever a refactor could silently drop.
- [ ] **Task 22. Answer v4 §15 Q6 before Task 18 begins.** On a server whose payloads
      `for_result` does not recognise, `Unknown` claims nothing by design — so nothing
      would ever be promoted. The two options are promotion on "no error, N runs" (weaker
      evidence, faster) or routing-only promotion marked plainly unverified in the UI. The
      second keeps the honesty ladder intact and is the recommendation.

### Rejected, with reasons recorded

- ~~**Suppress the device list on small local models.**~~ Fails question 3. A capable
  model uses the names well; hiding them makes it deny devices that exist — the exact
  failure `crates/fono/src/actions/mod.rs:186-205` already warns about.
- ~~**Trim the tool catalogue by model class.**~~ Same failure, and it makes what Fono can
  *do* depend on which model is loaded, which is a worse property than being slow.

---

## Verification Criteria

- A refused call is retried once and succeeds, on both a cloud and a local model, with no
  second utterance from the user.
- A partial failure retries the devices that did not respond before reporting.
- No argument invalid against the published schema ever reaches a server.
- A Romanian command produces a Romanian reply.
- "Turn on the lights in the bedroom" moves the lights and does not move the rollers.
- No Tier 1 item branches on model name, model size, backend class, or vendor — checkable
  by reading the diff.
- First command after daemon start reports `decoded_prefix_tokens` under ~100 rather than
  ~1440 (Track B), asserted on that field and never on `cold_prefills`.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, and
  `cargo test --workspace --tests --lib` all clean; no new dependency; size budget
  unchanged.

---

## Potential Risks and Mitigations

1. **A retry doubles a non-idempotent command** — "raise the temperature by two degrees"
   twice is four degrees.
   Mitigation: Task 3's absolute-end-state rule, written down as a rule rather than left
   as the current accident of `desired_state`, and tested.
2. **A retry masks a persistent failure** — the user hears success after two silent
   attempts and never learns something is wrong.
   Mitigation: the retry is a trace event and the spoken reply says a correction happened.
   Never silent.
3. **Schema validation rejects a payload the server would have accepted**, because an
   advertised schema is looser or stricter than actual behaviour.
   Mitigation: validate types and enums only, never required-field sets; on validator
   error, log and pass through rather than block. The failure mode must be "we sent it
   anyway", never "we refused to try".
4. **Re-offering tools after a refusal erodes the one-action-per-turn guarantee** by
   precedent, one exception at a time.
   Mitigation: Task 3 makes the exception structural — reachable only from the retry path,
   never from the wording pass — rather than a flag someone can set elsewhere.
5. **Moving the instructions to the end (Task 5) invalidates the pinned head once.**
   Mitigation: expected and harmless — one cold prefill after upgrade, stable thereafter,
   guarded by Task 15.
6. **Tier 1 fixes the four traces we have and misses the class of failure we have not
   seen.**
   Mitigation: Task 9 measures rather than assumes, and Tier 2 exists precisely for the
   shortfall. The plan is explicitly staged so that "it did not improve enough" is a
   supported outcome with a defined next step.
7. **Track B and Tier 1 Task 5 both touch head composition and conflict.**
   Mitigation: Task 10 lands the shared composition function first; Task 5 is then a
   reordering inside one function rather than a change in two crates.

---

## Alternative Approaches

1. **Grammar first, recovery second.** Prevention is one round trip cheaper than recovery
   and would have stopped the invented colour outright rather than correcting it.
   Rejected as the *opening* move because it is local-only: cloud users would get nothing
   until Tier 1 landed anyway, and Tier 1 makes the grammar's job smaller when it arrives.
2. **A cheap second inference to sanity-check the call before executing.** Universal and
   vendor-neutral, but it adds a model round trip to every action — precisely the
   direction the latency work is pushing against, and it substitutes the same fallible
   component for judgement about its own output.
3. **Ship shortcuts first and route around the model entirely.** Superficially attractive
   given how much of the latency is the model, but a shortcut is promoted from a *verified*
   run — so verification has to be trustworthy before replay is safe. Tier 1 is what makes
   it so.
4. **Accept the model-specific fixes and gate them on backend class.** Would likely improve
   the local traces fastest. Rejected because it makes Fono's behaviour depend on which
   model is loaded, breaks v4 §10, and creates two prompt surfaces to keep byte-stable for
   the cache — twice the maintenance for a benefit that shrinks as local models improve.
