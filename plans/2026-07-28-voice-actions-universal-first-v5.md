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
- **F39 — Home Assistant publishes no enum on the three slots that fail.** Read from the
  live `tools/list` dump of the user's own house (26 tools): `area` is a bare
  `{"type": "string"}` on all 22 tools that accept it, `name` is a bare
  `{"type": "string"}`, `domain` is an unconstrained `{"type": "array", "items":
  {"type": "string"}}`, and **not one tool declares a `required` list**. Enums *are*
  published, but only on `device_class` (21 values), `media_class` (20) and
  `todo_get_items.status` (3) — slots no recorded failure has ever touched. So a
  purely schema-derived grammar (16b as first written) constrains tool name and field
  types, fires its enum branch on `device_class`/`media_class` only, and its
  required-fields branch on nothing at all. It cannot reach F33's invented area or F38's
  omitted domain. Corollary: the enum for those three slots has to be *authored by Fono*
  from live state, or it does not exist. Cross-check: `alez007/modelship-conversation`
  @`7342bb6` (Apache-2.0) injects exactly these enums server-side from HA's own registry
  — that patch exists *because* upstream omits them.

---

## The benchmark was blind, and every number before this is suspect

Re-read of the last `bench-actions` run (`~/.local/state/fono/bench/actions/1785233801`)
against its own traces. **Fixed 2026-07-28.**

- **F40 — the harness could not see what the model did, and this is the same shape of bug
  as F36.** Every case in that run reported `"calls": 0` and `"reply": ""`. The traces from
  the *same* run show a tool call per turn. Cause: the harness reconstructed the turn by
  reading `ConversationHistory` *after* `run_assistant_turn` returned, and a turn that acts
  deliberately calls `forget_after_action`, which clears history — so the harness read an
  emptied buffer. As with F36 the mechanism was fine and the observation point was one
  crate away from the data. What this silently disabled, on every number on record:
  - `routing_rate` was pinned at `0.0` — an artefact, not a finding, because `score_routing`
    falls through to `expect_no_call` whenever there is no first call to inspect.
  - `forbid_args` was **never enforced at all**, so invented arguments went unscored.
  - `reply_truthful` and `reply_language_matched` were permanently `null` — meaning **F37's
    language defect was unmeasured** and Task 20 of the benchmark plan looked complete but
    asserted nothing.
  - `retried()` was always false, so Task 3's non-idempotent-retry bound was unmeasured.
  - `recovered` verdicts were unearned, being `all_good && !routed_first_try`.
  Only `final_rate` was ever trustworthy, because `score_outcome` reads the live house
  rather than history. **The 40 % baseline is therefore a valid outcome number and an
  invalid routing number**, which matters for Task 22: a grammar that improves routing
  could not have been observed doing so.
  Fix: the pump writes a `TurnRecord` at the same instant, under the same lock, as the
  history push it mirrors — so it cannot disagree with what the model was told, and it
  survives the clear. Asserted by `what_a_turn_did_outlives_the_history_it_clears`, which
  sits directly beside the test for the clear itself.
- **F41 — with the harness sighted, the dominant failure is an invented argument, not a
  malformed call.** First run of the repaired harness, six office cases (climate and
  media_player, en + ro): routed 33 %, worked in the end 17 %. Four of six first calls were
  `HassClimateSetTemperature` for a plain "turn on the AC" — the model reached for the
  domain's most specific tool and invented the value it requires (`temperature: 24`, and
  `temperature: ""` / `temperature: 0`, both refused as *"invalid slot info"*). Note the
  shift: `domain: ["light"]` was present in four of five calls in the earlier run, so
  **F38's wording fix appears to have worked** and the live failure is now F33-shaped
  (wrong tool, invented argument), not F38-shaped. This is the evidence that settles
  Task 8-as-code.
- **F42 — both Romanian replies were in the wrong language, now visible for the first
  time.** `reply_language_matched` was `null` on every case ever run (F40), so F37's fix
  had never actually been checked. It fails: all three Romanian cases replied in English.
  Track A's language work is **not** done, and the check that proves it only started
  working today.
- **F43 — a `names:` field is an alias list, and reading it verbatim produced an
  uncommandable name.** The user's house records one speaker as `Office display, Boxa
  birou` — a single entity carrying an English alias and a Romanian one, exactly the comma
  convention `areas:` already splits on. The house parser took the line whole, so `name`
  became a string Home Assistant refuses outright (`MatchFailedReason.NAME`), and a fixture
  naming the Romanian half staged and scored correctly but **could not be put back
  afterwards** — the run ended warning the home might have been left changed. Pre-existing
  and latent; only a bilingual device exposed it. Fixed by splitting the list, keeping the
  leading name as the only one ever *sent* and matching against all of them. The garage-door
  safety check now reads every name too: a door recorded with a second-language alias is
  still a door.

**What this says about the plan's method.** Tier 1 was ordered by leverage and the ordering
was right — but F36 shows a fourth question belongs beside the three universality
questions: *can this mechanism be observed working?* The retry passed every unit test,
shipped, and was dead on arrival in the one code path that mattered. Nothing asserted that
a corrective call was executed rather than spoken, because the assertion lived in a
different crate from the bug.

F40 is the same lesson one level up, and worse: the *instrument* had the defect. Three
rounds of prompt rewording were graded against a routing number that was structurally
incapable of being anything but zero, and a language fix was declared shipped while the
check for it returned `null` on every case. So the fourth question has a companion:
*can the thing that measures this be observed working?* A harness that reports a plausible
number for the wrong reason is more expensive than one that crashes.

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
  **Settled 2026-07-28 — the condition is met, see F41.** The `bench-actions` traces show
  schema-valid invented arguments getting through repeatedly and dominating the observed
  failures: `brightness: 100`, `brightness: 10`, `temperature: 24` on a plain switch-on,
  and `temperature: ""` / `temperature: 0` which Home Assistant refused as *"invalid slot
  info"* despite `HassClimateSetTemperature` declaring the field a bare number. Task 4
  cannot catch any of them by construction. **The code version of Task 8 is now
  evidence-backed and is the highest-value remaining universal fix** — the model's
  dominant error is no longer a malformed call but a well-formed call that answers a
  question nobody asked.

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

### Tier 2 — grammar (unblocked: three rounds of prompt wording have now failed)

Prompt wording is no longer a candidate lever. F33/F37/F38 record three separate
rewordings of the room hint, each verified present in the live prompt, each followed by a
bare `{"area": "Master bedroom"}` that opened the curtains. The remaining honest lever is
structural: make the malformed call unreachable rather than discouraged.

**Why the scoping problem does not exist.** A *lazy* grammar is inert until a trigger
string appears. No `<tool_call>` in the output, no constraint — so stories, jokes and
explanations sample exactly as they do today. It is a property of the sampler, not a mode
Fono selects. All template spellings of the opener go in the trigger list, and the trigger
pattern's capture group marks where constraint begins, so the grammar describes only the
JSON object that follows the opener.

**Correction to this plan's premise about the binding.** The safe wrapper
`LlamaSampler::grammar_lazy` (`llama-cpp-2` 0.1.150, `sampling.rs:329`) is behind that
crate's `common` feature, which we do **not** enable — it links `libcommon`, worth roughly
14 MB against a 25 MiB budget, so enabling it is not available to us. The *implementation*
however lives in `libllama`, which we already link: `llama_sampler_init_grammar_lazy_patterns`
is `LLAMA_API` and is present in the generated `llama-cpp-sys-2` bindings with no feature
gate. Fono therefore calls the raw symbol and drives the returned sampler through the fully
public `LlamaTokenDataArray` apply path, using the same layout-asserted cast precedent
already established in `crates/fono-core/src/brain_tap.rs`. Net dependency and net binary
growth: zero.

**Why it cannot cost prefill.** A grammar is a sampler, not text. It never enters the
prompt, so `PromptStateCacheKey` (which hashes prompt text and tokens) is unchanged, the
pinned head stays valid, and enabling or rebuilding the grammar invalidates nothing. This
is a strict advantage over every prompt-based attempt so far — each of *those* invalidated
the cache. Cost is GBNF parsing at sampler construction: kilobytes of string, microseconds,
once per generation. The honest downside is accuracy, not latency: constrained sampling can
produce a worse *valid* call instead of an invalid one, which is what Task 22 exists to
measure.

**The universality boundary — the rule that keeps other servers safe.**

> A grammar branch derived from a tool's *published schema* is universal and always on when
> the setting is on. A grammar branch derived from the server's *observed live state* is
> universal in mechanism but needs a field name to attach to, so the field names live in
> vendor code. A grammar branch that *contradicts* a published schema is vendor-specific and
> requires a vendor that claims it.

The middle category is new, and F39 forced it: HA publishes no enum on `area`, `name` or
`domain`, so a schema-only grammar constrains nothing that has ever failed. The enum has to
come from the live house dump instead — which is not "the server's own truth" in the strict
sense the first category means, but it is not our guess at intent either: a device name that
is not in the house cannot be a correct answer. Fail-soft is the safeguard — no live state,
no enum, today's behaviour.

**How the middle category avoids becoming maintenance work.** The rejected alternative was
`modelship-conversation`'s approach: a table mapping each tool to the device kinds it acts
on (`HassLightSet` → `light`, `HassMediaPause` → `media_player`). That table is 26 rows
today, it is derived from HA's *internal* handler registry which MCP does not expose, and
every HA release that adds or renames a tool silently rots a row. Fono instead recognises
**field names**, never tool names: a tool with an `area` field gets the room enum, a tool
with a `name` field gets the device enum, a tool with a `domain` field gets the device-kind
enum plus `__all__`. Three names, in `crates/fono/src/actions/vendor.rs`, drawn from HA's
public intent API. A tool HA ships next year works with no change; a tool HA renames works
with no change; a server with none of those fields gets no grammar at all.

A server Fono has never seen receives exactly the constraints it declared about itself. A
loose schema yields loose constraints, because that is what the server said. A tool with no
schema gets no grammar branch at all — unconstrained, i.e. today's behaviour. The user's
requirement ("shouldn't break things for other tools") is therefore structural, not a
promise: an unfamiliar server *cannot* receive a Home-Assistant rule, because no vendor
claims it for them.

- [x] **Task 16a. `[assistant.tools].grammar`, off by default.** One setting obeyed by the
      daemon, the benchmark and everything else — not a hidden env var, because the point
      is to A/B it. *Done:* `crates/fono-core/src/config.rs` (`AssistantTools::grammar`,
      `#[serde(default)]`, false) — and, because a setting nobody can reach is not a switch,
      surfaced as a toggle in the web settings tools section
      (`crates/fono-net/src/web_settings/assets/app.js`).
- [x] **Task 16b-i. Schema-derived GBNF from the tool catalogue.** Mechanically, per enabled
      tool, from `Tool.schema` in `crates/fono-core/src/tool_catalog.rs`: tool **name** must
      be one of the enabled names; **required** fields must be present; field **types** must
      match; a schema **enum** constrains to its listed values. No vendor knowledge, no
      language knowledge. *Rationale corrected by F39:* against stock HA this branch is
      largely inert — it binds the tool name and the field types, and its enum branch fires
      only on `device_class`/`media_class`/`status`. It is still worth having (it is free,
      and it is the only branch an unknown server ever sees), but on its own it kills neither
      F33 nor F38. *Done:* `crates/fono-core/src/tool_grammar.rs`.
- [x] **Task 16b-ii. Catalogue-derived enums for the slots HA leaves open.** Persist each
      device's `domain` beside its name — `parse_devices` in
      `crates/fono-assistant/src/mcp_client.rs` already reads it and was discarding it — then
      build the `name`, `area` and `domain` branches from the store: room names from
      `place_names()`, device names from `device_names()`, device kinds from the observed set
      via `device_domains()`. Fail-soft: a slot with no live state gets no branch. *Done:*
      `Device`/`set_devices`/`device_domains` in `crates/fono-core/src/tool_catalog.rs`, enum
      authoring in `crates/fono-core/src/tool_grammar.rs`, field names in
      `crates/fono/src/actions/vendor.rs`.
      *Landed one step short of the original wording:* the device enum is the whole exposed
      set, **not** scoped per tool to that tool's own domain. Scoping needs a tool→domain
      mapping, and the only available source for it is the tool-name table this plan
      explicitly rejected as unmaintainable (see the maintenance argument above). So
      `HassLightSet`'s `name` slot currently offers every device, not only the lights — it
      still cannot name a device the house does not have, which is the F33 class. Narrowing
      it further would need a signal MCP does not publish; revisit only if Task 22 shows
      wrong-device survives.
- [x] **Task 16c. Wire it into the sampler chain.** One extra link before greedy in
      `crates/fono-core/src/llama_gen.rs`, built only when tools are actually offered;
      otherwise pass today's plain sampler. Local backends only — cloud providers enforce
      required fields themselves. Kill path: setting off ⇒ no grammar object constructed.
      *Done:* threaded through `GenParams::grammar` in
      `crates/fono-assistant/src/llama_local.rs`, including the cold
      `run_inference_with_model` fallback so a prefix-cache miss cannot silently disarm it.
- [x] **Task 16d. Record `grammar: on|off` in the trace** so a scored run can never be
      misattributed later. *Done:* at the assistant generation call site rather than in the
      shared span helper, which polish also uses.
- [ ] **Task 22. Measure the grammar A/B on identical text.** Two `bench-actions` runs, same
      fixture, one number each, against the 40 % baseline already on record. Rationale: the
      benchmark's whole value is that it removes the microphone confound — F36 showed two of
      five live turns were misheard (`"lumidile"`, `"aparatul masinii"`), so a live-voice
      comparison cannot attribute a change to the grammar. Ships only if it beats 40 %.
      **Unblocked but re-based, 2026-07-28 — read F40 before running this.** The A/B had no
      valid control until today: the harness could not see a tool call at all, so the
      routing number on record is a structural zero, not a measurement. A grammar whose
      entire purpose is to constrain *which call is emitted* was about to be graded on the
      one statistic that could not respond to it. The 40 % is still usable as an **outcome**
      baseline; the routing baseline has to be re-established from a sighted run first. Both
      arms must also be run on the same fixture **after** the F43 alias fix, or a bilingual
      device resolves differently between them.
- [x] **Task 17. Vendor tightening: the `__all__` escape value.** *Folded forward into
      16b-ii, not deferred behind Task 22.* The reason it was conditional no longer holds:
      F39 established that Fono authors the `domain` enum itself, so the only question left
      was whether that enum includes a room-wide value — and a `domain` enum *without* one
      would remove the ability to say "turn everything off in here". Authoring the enum and
      adding `__all__` to it are the same edit. Deferring it would have shipped a regression.
      Note the mechanism landed one step short of the original wording: `__all__` is *offered*
      in the enum, but `domain` is not yet made mandatory where HA declares it optional — that
      part still contradicts the schema and still waits on Task 22.
      Design, and why the two earlier options were rejected:
      - **Rejected — keyword matching the transcript** for a device kind ("light/lumina/
        lampa/…"). Fono supports 30+ languages; the table is unmaintainable and would have
        missed `"lumidile"` anyway.
      - **Rejected — unconditionally required `domain` with no escape.** Costs the ability
        to say "turn everything off in here".
      - **Chosen — mandatory field, enumerated value, `__all__` among the choices.** Zero
        language knowledge in Fono: the model already demonstrated it can map
        `dormitorul principal` → `"Master bedroom"`, so it can name a device kind when
        forced to state one. The failure was *omission*, which is the path of least
        resistance for any model; removing the silent default removes the class.
      - **Strictly better than today:** `__all__` is auditable. Right now "curtains moved"
        and "the model meant to move the curtains" are indistinguishable — both look like a
        missing field. With an explicit value the trace says which, and room-wide can be
        treated as the deliberate, riskier act it is (name what will be touched, or
        confirm). That option does not exist while omission is legal.
      - **Residual risk, stated:** the model may *choose* `__all__` when the user said "the
        lights". Grammars do not fix wrong choices, only omissions — but that is one visible
        failure mode instead of two invisible ones, and prompt wording has never been tested
        at preventing a *stated* wrong value, which is a far easier ask than preventing
        omission.
      - **Wart, accepted:** `__all__` is not a real HA domain value; it must be intercepted
        in Rust and translated to "omit `domain`" before the call goes out. Vendor code,
        which is the right place for it.
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
- [x] **Task 20. Give the live house test a permanent home under `tests/`.** It is still
      `tmp/ha-recon/live_light_test.py` and will vanish when `tmp/` is pruned (v4 §12.1).
      *Done.* Now `tests/live_house.py`, self-contained: the MCP-over-SSE client is
      inlined (it used to import from a sibling scratch file), the tool catalogue is
      fetched live rather than read from a scratch fixture, the hardcoded house address
      became `HA_BASE` / `--base`, and the kitchen became `--area`. Deliberately not
      wired into `check.sh` — it needs a real house and moves real lights.
- [x] **Task 21. Pin reasoning-off on the action path with a test**, across all backends,
      including the scope limit that Fono's own LLM server must not force it. Guards a
      4–14× lever a refactor could silently drop.
      *Done.* `thinking_switches` extracted from the request body so it can be asserted
      without a network call, and `every_backend_is_told_not_to_think_before_it_acts`
      names all six backends individually — adding a seventh without deciding this now
      fails the test. The scope limit is pinned on the other side of the boundary:
      `default_model` extracted in the LLM-server proxy, with two tests asserting a
      client's `reasoning_effort` is passed through untouched and that filling in an
      absent model is the only mutation Fono makes.
- [ ] **Task 23. Answer v4 §15 Q6 before Task 18 begins.** On a server whose payloads
      `for_result` does not recognise, `Unknown` claims nothing by design — so nothing
      would ever be promoted. The two options are promotion on "no error, N runs" (weaker
      evidence, faster) or routing-only promotion marked plainly unverified in the UI. The
      second keeps the honesty ladder intact and is the recommendation.
      *Renumbered from 22, 2026-07-28: two unrelated tasks carried that number — the
      grammar A/B in Tier 2 and this one — so "Task 22" was ambiguous in a plan whose
      other entries cross-reference it by number. Every existing reference to "Task 22"
      means the grammar A/B.*

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
