# Voice actions — universal fixes first

**Status:** ready for implementation.
**Revised 2026-07-31:** also supersedes v4 §7.2's no-op rule and §9/§9.1's promotion and
demotion rules. Shortcut promotion is no longer a verification question — see Task 18b
for the three sentences that replace it, and Task 19 for why the no-op debt was dropped
rather than paid.
**Supersedes:** `plans/2026-07-26-voice-actions-v4.md` §12 (Phases 2–6). v4 remains
the evidence record — its §1 findings F1–F32 and its §5–§11 design (catalogue store,
verification ladder, shortcut semantics) are unchanged and still authoritative **except
for the promotion rules noted above**. This
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
- **F44 — the rails were switched on for a year and never held the model to anything.**
  The first honest A/B came back with the two arms *byte for byte identical*: same replies,
  same commands, same mistakes, while the trace of every constrained run said `grammar:
  on`. The cause was one line in each decode loop. `llama.cpp` already tells the sampler
  about the token it just handed back, and Fono told it a second time — so the grammar
  read the reply as `<<tooltool__callcall>>{"{"`, which contains no opener any pattern
  recognises, and sat waiting for a start that could never arrive. Every command Fono has
  ever written locally was written unconstrained. The same doubling also fed the
  repetition penalty each token twice, so it saw half the history it was configured for.
  Fixed by routing every decode loop through one function that samples and leaves the
  accepting alone, with a second function for the one caller that genuinely has a token
  from elsewhere. What makes this finding possible to have missed: **eleven tests covered
  the grammar and not one of them asked whether a token was ever forbidden.** They proved
  the text parses, the symbols link, the memory frees, every opener is recognised — all
  construction, no enforcement. The new test counts how much of the vocabulary the sampler
  is refusing, and pins the doubling itself as the disarming mistake it is.
- **F45 — with the rails actually holding, the grammar fixes the failure that was breaking
  most commands, and exposes three it cannot fix.** 19 of 22 cells now differ between the
  arms. Passes go 4 → 5 and first-try routing 18 % → 23 %, which understates it badly; the
  interesting change is *which* mistake disappears. Unconstrained, the model puts the
  **device name in the room field** over and over — `{"area": "Balcony lights"}`,
  `{"area": "Office display"}`, `{"area": "Living room couch Blue"}` — and no amount of
  prompt wording had stopped it. Constrained, that becomes structurally impossible and
  every one of them turns into a real room with the device in its own field. Slot
  confusion is gone. What is left is three defects the rails cannot reach, each now
  visible for the first time and each cheap:
  - **The kind of device is chosen freely and contradicts the device named.** `HassTurnOff
    {"name": "Air conditioner", "domain": ["light"]}` — Home Assistant refuses it outright.
    Fono's own store already records that this device is `climate`, so nothing has to be
    guessed. Four cells fail on exactly this.
  - **Empty and invented values pass the rails because a string may be empty.**
    `{"floor": "", "color": ""}` and a `brightness` of 10 nobody asked for. The grammar
    lets an optional field be skipped, but the model volunteers it blank instead, and a
    blank room is a room the server cannot match.
  - **One device in the house carries a name no command can use.** `Office display, Boxa
    birou` is stored as a single name, so the rails faithfully offer it and Home Assistant
    faithfully rejects it. F43 fixed this for the *fixture* parser; the live store is still
    keeping the joined form.

- **F46 — the correction was offered as a choice, and small models decline it.** After a
  refused call Fono appends *"if you can tell from this what was wrong, correct it and call
  the tool once more; otherwise tell the user plainly what went wrong"*. Every failed cell
  in the sighted run took the second branch: Home Assistant said `MatchFailedReason.NAME`,
  the correction was sitting in the message, and the model apologised instead. This is F44's
  lesson one level up — a mechanism that is *available* is not a mechanism that runs. Asking
  is not a mechanism; removing the alternative is. Measured with the two repairs below:
  passes 5 → 9 of 22, first-try routing 23 % → 41 %, English routing 82 % → 100 %.
  **Credit withdrawn — see F49.** The three changes were measured together and the gain was
  attributed to this one without checking. Read call by call, the compulsory second attempt
  rescued nothing at all.
- **F47 — no server here publishes tool annotations, so nothing may be built on them.**
  MCP lets a server declare `readOnlyHint`, `idempotentHint`, `destructiveHint`. Probing the
  live Home Assistant server directly, all 26 tools carry `name`, `description`,
  `inputSchema` and **nothing else** — no `annotations` key at all, not even on
  `GetLiveContext`, which reads and only reads. So the plan to replace Fono's name-sniffing
  (`checks()` / `repeatable()` keyed on `HassTurnOn`-style names) with the protocol's own
  signal buys nothing today and would be plumbing built on a field no server sets. Dropped
  until a server in front of Fono publishes one. What it was wanted for is answered by
  evidence instead: the two places that needed "does this tool only read" are both settled
  by looking at the house before and after (Task 28).
- **F48 — with the kind of device settled, the same mistake moved to the room.** Every
  climate failure left in the run is Home Assistant answering `no_match_reason=AREA`: the
  model names a real device, now with the right kind (the correction fires and the trace
  shows it), and pairs it with a room the device is not in. Fono cannot fix this the way it
  fixed the kind, because the catalogue records what a device *is* and not where it *is*.
  Same shape, one field further back, and it is the largest remaining cause of a refused
  command (Task 29).
- **F49 — the compulsory second attempt recovered nothing, and the gain came from elsewhere.**
  Reading the sighted run call by call: 13 of 22 cells attempted twice. **6 wrote the first
  call again byte for byte.** Of the 7 that wrote something different, exactly **1** was
  accepted by the server (`room_command_names_the_kind_of_device en`, dropping a bad
  `floor: "1"`) — and the lights still ended `off`, so the cell scored `drifted`. The run's
  own `recovered` counter is **0**. Of the other 6: 3 corrected `domain` themselves and still
  failed on the room, 2 doubled a relative temperature (`+2` then `+4` — the exact hazard the
  non-idempotent class exists to catch, escaping only because both calls were refused), and 1
  guessed a different device outright. Attributing the 5 → 9 by class: must-not-act 0/4 → 2/4
  is **the harness scoring honestly** (Task 28), not the model behaving better; plain command
  +1 and tool choice +1 are real but inside run-to-run noise on 22 cells. So: two of four
  extra passes are instrument, two are unrepeated. Task 24's correction fired 3 times, was
  right 3 times, and was worth nothing 3 times, because the room beside the device was wrong
  in all three. **Two consequences.** (a) Task 29 is now first — three corrections are already
  right and waiting only on it. (b) The retry as built hands the model *the same raw refusal
  the first attempt already read*, so it has nothing new to go on; it costs ~9 s and until it
  is told what specifically to change it will keep repeating or guessing (Tasks 30, 31). The
  method lesson is F36's again with the sign flipped: a mechanism that *is* observed running
  still needs its effect attributed, not assumed, when it ships beside two others.
- **F50 — a refused repeat is worth more than the time it saves.** Task 30 was written to stop
  a wasted round trip and it moved the score more than either of the two corrections did:
  11 → 15 of 22, first-try 50 % → 68 %, Romanian 2 → 5. The refusal fired on 10 of 22 cells.
  The reason is not the clock. A repeat that goes out earns a *second* server reply saying the
  same thing, and the model then words its answer against two refusals rather than one; the
  cells that changed are mostly ones whose only remaining fault was answering in the wrong
  language, which suggests the second refusal was crowding out the language decision. So the
  cheapest thing in the run was not "stop paying for nothing" but "stop handing the model
  contradictory evidence it has to reconcile". Generalisable, and worth applying wherever a
  turn accumulates near-duplicate observations.
- **F51 — where the remaining seven failures sit, after Tasks 29 and 30.** None is a room or a
  kind any more, and none is a repeat. Two are the *tool* chosen, not its arguments —
  `stinge Balcony lights` and `aprinde Couch Blue` reach for the brightness-and-colour tool.
  The rails constrain values and never which tool, so nothing here can help; this is model
  capacity. Two are the Guest bedroom room command, which the house accepts and then does not
  act on — that needs looking at the house, not at Fono. Two are Romanian cells whose command
  was right and whose reply came back in English, and one is a question about state answered
  by acting on it. Reply language is still partly the harness: `bench_actions/turn.rs`
  withholds the language on purpose, where a spoken turn has already decided it and says so.
- **F52 — a setting whose "off" position is never the right answer is not a setting.** The
  rails shipped behind `assistant.tools.grammar`, a config key and a web toggle, on the
  reasoning that anything measurable deserves a switch. Once measured the switch had no
  defensible "off": every arm with the rails off was worse or equal, and the page had to
  offer the user a choice between *"the rules Fono adds"* and *"the assistant's own
  judgement"* — which is a choice between a command that works and one that names a room the
  house does not have. Removed from the config, the CLI (`--grammar`), the settings page, the
  actions page (four places that branched on it) and the run summary. Two things this
  clarifies. (a) The `--grammar on|off` flag earned its keep exactly once — the A/B that
  found F44 — and keeping it afterwards would have been paying rent on a finished
  measurement; a `git revert` of one commit reproduces the arm if it is ever wanted again.
  (b) The one argument for keeping an escape hatch, a stale catalogue blocking a renamed
  device, is answered by refreshing the catalogue on a name mismatch (Task 34), not by
  letting the user disable the rails for the whole house. **Removal caveat:** the key was
  never in a release, but the settings page had written it into at least one live config, and
  `AssistantTools` is `deny_unknown_fields` — so a stray `grammar = true` makes `config show`
  fail loudly and every `unwrap_or_default()` path silently fall back to a **default config**.
  No tolerance code was added (nothing shipped with the key); the one affected file was
  edited. Worth remembering as the general cost of deleting a config key: on the strict
  sections it is not a no-op.
- **F53 — the universality claim for the rails rests on one model.** Every number in F44–F51
  comes from a single local model. The argument that the rails generalise is *structural*
  rather than empirical: they constrain the three shapes Fono's own prompt asks for and Fono's
  own parser reads, pinned together by `every_accepted_opener_arms_the_rails`, so a model that
  writes its command in its own native format neither arms the rails **nor parses today** —
  already broken, with or without them. The failure mode is fail-open, which is why removing
  the switch is safe. Confirming it on a second local model is cheap and has not been done.

- **F54 — the 700-token house hint earns every token, and one "redundant" rule is
  load-bearing.** `assistant.tools.place_names` reads like a room list; it is the room
  names *plus* the device names *plus* six rules, and on a 14-room / 77-device home it costs
  about 700 tokens of prefill per turn — the documentation said thirty, wrong by a factor of
  twenty. Four arms, same 22 commands, one house (`FONO_ACTION_HINT`):

  | arm | right | first try | p50 | p95 |
  |---|---|---|---|---|
  | everything | **15** | 0.68 | **9.5 s** | **18.7 s** |
  | minus the two rules the code enforces (`lean`) | 11 | 0.50 | 9.5 s | 29.6 s |
  | minus all rules (`no-rules`) | 11 | 0.50 | 10.4 s | 25.2 s |
  | minus the device list (`no-devices`) | 8 | 0.36 | 10.5 s | 29.8 s |

  Monotone, and monotone by language too (en 10/8/7/6, ro 5/4/3/2). There was no trade to
  make: **the full hint is also the fastest**, at the middle and at the tail. 700 tokens of
  prefill is cheap next to a failure, which buys a second attempt and a longer reply. Both
  halves stay.

  The finding worth keeping is the `lean` arm, which dropped the two rules that looked
  redundant — the rails make an invented name unwritable (rule 1) and `HouseFacts` drops a
  room named beside a device (rule 4) — and lost four commands. Rule 4 does not do what its
  wording says. The code drops the room whether the model was told to or not; what the rule
  buys is the model **naming the device at all**. Without it, asked to switch the office air
  conditioner off, the model wrote a bare `{"area": "Office", "floor": "1"}` — no device
  name, so nothing for Task 29 to key on and nothing to drop. *A rule can be redundant with
  the code as stated and load-bearing for the code as used.* The general form: prompt text
  and code enforcement are not substitutes, because the enforcement has preconditions that
  the text is what satisfies.

  Method note against F52: deleting the rails switch was right because its "off" was never
  the right answer. The same reasoning applied to this hint would have been wrong — its
  "off" costs seven commands, and nobody had measured that either. The lesson is not
  "delete settings", it is "measure before deciding either way".

- **F55 — one underscore in a rule label threw the rails away on every generation, and
  nothing but a stderr line said so.** Home Assistant publishes a field called
  `device_class`. The label built from it, `list3-device_class`, is illegal in GBNF:
  llama.cpp's rule-name character set is letters, digits and hyphen, no underscore
  (`llama-grammar.cpp:98`). Its parser clears **every** rule on any error, so 23 KB of
  correct grammar was discarded over one character in a label that exists only to be read.
  Every command in a 22-command run was written unconstrained, and the run scored 12 rather
  than the 15 it had scored the day before — I came within one commit of recording that as a
  regression caused by an unrelated change of mine.

  Three properties made it expensive rather than annoying. It **fails open**: a rejected
  grammar samples exactly as no grammar does, which is F44's shape reached by a different
  road, and now that the on/off switch is gone (F52) there is no longer any setting whose
  position explains the difference. It is **data-dependent**: the tests build grammars from
  fixtures and check the resulting text, and the real catalogue has a field name the fixtures
  do not — no test had ever handed a built grammar to llama.cpp and asked whether it was
  accepted. And it is **all-or-nothing**: a single bad name loses the whole file, not one
  field.

  Fixed by dropping `_` from the surviving character class. Two tests now stand where none
  did: a plain unit test that refuses any rule name outside llama.cpp's alphabet, and an
  acceptance test that builds a grammar containing exactly this kind of field name and asserts
  llama.cpp took it. Both fail without the fix. The benchmark also now says so at the end of a
  run in which any generation stamped `grammar: "rejected"`, so a broken-rails run can no
  longer be read as a measurement of a model.

  General form, and the third time this plan has hit it: *a mechanism that degrades silently
  to the previous behaviour needs a test of its effect, not of its request.* `rails_bit` was
  added for exactly this after F44 and it worked — it is what caught this one.

- **F56 — naming the language wins commands and costs a full cold prefill on every turn.**
  Five Romanian cells were failing on the reply language alone: the call was right, the
  light moved, the answer came back in English. The benchmark withheld the language on
  purpose, so telling it looked like a free correction. It won 3 cells (14 → 17, ro 5 → 7)
  and took p50 from **10.8 s to 49.2 s**.

  Cause: **22 of 22 turns cold-prefilled**, against 1 of 22 without it. Any note appended to
  the system prompt defeats prefix matching, however short the note is — and the English
  turns, which all carried an identical note, were cold too, so it is not the alternation
  between languages. This contradicts the stated design (`traits.rs:110-113`), which puts
  notes last precisely so that a change of speaker costs a handful of tokens instead of the
  device list. `speaker_note` travels the same path and presumably pays the same price, which
  would make it a live latency defect and not only a benchmark one.

  Two things worth separating. Production **does not name the detected language today**: the
  turn is handed `general.language_override()`, so a user who has not pinned a language sends
  nothing (`session.rs:3307`). So the English replies are a real product failure, not a
  harness artefact — my earlier claim that these five cells "would pass in production" was
  wrong. And the fix is blocked behind the cache: at 38 s per turn it fails the standing rule
  about what latency a correctness gain may cost. Reverted, with the number recorded, until
  a note is cheap. Tasks 38 and 39.

  **Amended by F57**, twice. The blanket claim about production above is wrong: the path from
  dictation to the assistant *does* name the recogniser's verdict
  (`crates/fono/src/assistant.rs:678-687`, explicitly "not the configured hint — which is
  `None` in auto-detect mode, i.e. exactly when naming the language matters"). Only the live
  push-to-talk session path does not, and it says why (`session.rs:4612`, "language not yet
  surfaced"). So the cold prefill was already being paid on the paths that name a language,
  which makes F56's latency half a live bug that had shipped, not a benchmark artefact. And
  the cause was not the note: see F57.

- **F57 — the prompt cache was destroying its own entry, and the language note only exposed
  it.** F56 read the 38 s cold prefill as the cost of appending a note. Wrong. The cache had
  two independent defects, and the note merely made every turn's prompt differ enough to stop
  papering over them.

  *One:* the turn-start prefix and the mid-turn wording-pass prefix were filed in the same
  layer, and the wording pass's prefix strictly contains the turn-start one. `insert` drops
  any same-layer entry the new one covers — right for prefix matching, fatal here, because the
  covered entry is precisely the one the next turn asks for by name. The useful checkpoint was
  deleted seconds after being written, every turn, by the same turn.

  *Two:* the longest-prefix search was given three candidate layers and the mid-turn layer was
  not among them, so within a turn the second pass could not reuse the first.

  Fixed by splitting on lifetime rather than on position: a prefix that outlives the turn is
  filed apart from one that dies with it (`GenParams::prefix_outlives_turn`), and both are
  searched. Measured on the same 22 commands with the language named: **2 slow cold reads
  instead of 22**, one per language, p50 **49.2 s → 9.9 s**. The two that remain are honest —
  `en` and `ro` produce genuinely different prompt text, so each has to be read once.

  The lesson is about diagnosis, not caching. F56 named a *correlate* as the cause and drew a
  design conclusion from it ("notes are expensive, the design says otherwise, therefore the
  design is broken"). The design was fine; two lines of bookkeeping were not. A latency
  finding that blames a feature should be made to explain why the mechanism it blames
  behaves as it does, before a task is written against it.

- **F58 — eight of twenty-two commands are carried out in silence, and the benchmark scores
  them as passes.** Present in every run recorded here, `full` included, so it predates
  everything in this session and nothing in it caused or fixed the number. The model calls the
  tool, the house obeys, and the reply is the empty string: the user hears nothing at all.

  The clearest case is `an_impossible_request_is_refused_plainly`, in both languages. The
  whole point of that case is that a refusal is *spoken plainly*; it passes with silence. So
  does every dim, the relative change, and both air-conditioner switches.

  This is the largest user-visible defect on the board and the one thing no number here
  tracks, because the scorer reads the house and never asks whether anything was said. Same
  shape as F40 and F55: a green result standing in for a mechanism nobody checked was running.
  Task 42.

- **F59 — the silence is caused by the corrective attempt, and every earlier pass count in
  this plan is inflated by it.** With the scorer fixed to fail a turn that acted without
  speaking, the same 22 commands score **10, not 15**. Eight of the twelve failures are
  silence. So the 5 → 15 climb recorded across F46, F50 and F54 credited eight cells that a
  listener would have called broken.

  The mechanism is a collision between two changes that are each right alone. When a call
  fails, the correction is *written* for the model rather than asked of it: the prompt ends
  with the tool opener, so the only thing the model can write next is another command
  (`llama_local.rs:2509`). Then a second command equal to the first is refused rather than
  sent, because sending it only buys a second identical refusal. Both correct. Together they
  leave a pass whose entire output is a command that was never sent — held back, because
  reciting JSON to a user teaches the conversation that describing a command is a way of
  carrying one out — and `spoken` is empty by construction. There was never any prose in that
  pass to fall back on.

  Confirmed rather than inferred: the two warnings at `llama_local.rs:2541` and `2557` fire
  together, ten times in the run, and the eight silent cells are exactly the ones whose first
  call the executor judged worth another try. Reading the house back widened it — three of the
  ten are `accepted, but the devices are not in the state you asked for`, a verdict that did
  not exist before that change and that makes a call retryable.

  The lesson is not about either change. It is that **a mechanism that removes the model's
  alternatives owes the user a fallback**, and neither change owned the case where the forced
  command is then discarded.

- **F60 — a stalled generation was the machine being busy, not the name check.** A run showed
  one generation at 2,848 ms per word against a median of 92, and a per-word cost 1.7× higher
  with the name check applied than without. Both were artefacts of running the benchmark under
  `nice -n 10` on a desktop doing other work. Rerun at normal priority: median 68 ms per word,
  **maximum 112** — the forty-fold outlier is gone, and p95 for a whole command falls from
  49 s to 32 s.

  So the note in F52 that the name check "costs nothing in time" stands, and the doubt raised
  against it does not. The general point is narrower and more useful: **the benchmark competes
  with the model for the same cores, so a politeness flag on the run is a measurement error,
  not a courtesy.** Builds and tests still run niced; the benchmark does not.

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

F44 is the third instance and the sharpest, because this time the thing that could not be
observed working was a *safety mechanism*, and the setting that reported on it reported
only that it had been switched on. "Armed" and "having any effect" are two different
facts, and a trace that records the first while implying the second is worse than silence.
The rule this leaves behind: a constraint has to be measured by what it *refuses*, never
by whether it was installed — and any switch worth having in the trace is worth a second
field saying whether it ever bit.

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
- [x] **Task 22. Measure the grammar A/B on identical text.** Two `bench-actions` runs, same
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

      **Done, 2026-07-31 — and it took three runs, because the first two measured nothing.**
      What had to be built first: a `--grammar on|off` switch so one binary runs both arms
      on one fixture, and two harness repairs without which no arm finishes honestly — a
      device whose name matches a room name was being staged against the wrong thing, and a
      case that could not be staged silently scored as a failure of the model. Then the
      first real comparison came back with the arms *identical*, which is F44: the rails had
      never applied at all. With that fixed the arms differ on 19 of 22 cells and the
      grammar clearly earns its place (F45) — it eliminates the device-name-in-the-room-field
      mistake outright, which was the single largest cause of failed commands and had
      survived three rounds of prompt rewording.

      **On the shipping criterion.** "Beats 40 %" is not yet met on outcomes — 5 of 22
      against a baseline measured on a different, smaller set — and it is the wrong gate to
      hold this behind, for the reason F40 already established about the routing number: the
      three failures that remain (Tasks 24–26) are *not* things a grammar can fix, and two
      of them are one-line repairs outside the model entirely. The grammar is kept on, and
      the number is re-taken after Tasks 24–26 land, which is the first run where a miss can
      only mean the model chose badly.
- [x] **Task 24. Stop sending a kind of device that contradicts the device named.** A
      command that names `Air conditioner` and calls it a `light` is refused by the server,
      and Fono's own catalogue already records what that device is — the answer needs no
      guessing and no model. Four of the remaining failures are this and nothing else.
      Preferred shape: when a call names a device Fono knows, a `domain` / `device_class`
      that disagrees with the record is corrected before the call goes out, and the
      correction is noted in the trace so a model that keeps needing it stays visible.
      Universality: the *field names* come from the vendor layer as they already do for the
      grammar slots, so a server Fono has never seen is untouched.
      *Done 2026-07-31* as `HouseFacts` in `crates/fono/src/actions/mod.rs`, applied in
      `run_one` after the blank strip and before the schema check. No tool-name table: the
      field names come from `slot_fields()`, the kind from the devices the home reported, and
      a server that claims neither field leaves every call exactly as written. Corrects
      rather than refuses — the device named is the request. Two details beyond the wording
      above: the corrected value keeps the shape it was written in (list or scalar, as the
      tool's schema asks), and a name this home uses for two kinds of thing is left out of
      the record entirely, because there is no one answer to correct to.
- [x] **Task 25. Dropped as already paid, 2026-07-31. Not a task; a note.** The premise was
      that a blank value reaches the server — F45 says *"a blank room is a room the server
      cannot match"* — and that does not describe the live path.
      `drop_empty_arguments` (`crates/fono/src/actions/mod.rs:533-553`) has removed `null`,
      the empty string and the empty list, nested ones included, since Task 4, and it runs
      before the schema check and before anything is sent
      (`crates/fono/src/actions/mod.rs:888`). No blank has ever left Fono.

      Doing it in the grammar instead would be **strictly worse**, by this plan's own
      scoring: the rails are local-only, so it would replace a fix that works on every
      backend with one that works on one — the trade rejected in Alternative Approaches §1.

      What is left of that F45 bullet is the *other* half of it, and it is not about blanks:
      an invented value that is **not** blank — `brightness: 10` nobody asked for — is
      schema-valid, so neither the strip nor Task 4 touches it. That is the code version of
      Task 8, already recorded above as the highest-value remaining universal fix. Nothing
      is owed here.
- [x] **Task 26. Split a joined alias in the live catalogue, as F43 did for the fixture.**
      The house records one speaker as `Office display, Boxa birou`; stored whole, it is a
      name no command can use, and the rails offer it faithfully because they trust the
      catalogue. Same comma convention, same fix, one layer further in: keep the leading
      name as the only one ever sent, match against all of them.
      *Done 2026-07-31.* Split on the **read** side, not at discovery: the stored row keeps
      the line as the home said it, and `primary_name` trims it to the leading name in the
      two readers that face outward — `device_names` (the prompt list and the grammar enum)
      and `devices` (the actions page). Deliberate, because the aliases are load-bearing in
      the other direction: Home Assistant answers with whichever alias matched, and
      `record_device_run` already matched a reply against every comma-separated alias of a
      stored row. Splitting at discovery would have offered the right name and then lost the
      per-device history for every bilingual device. Nothing is stored differently, so no
      refresh reports a change and no warm prefix is thrown away.
- [x] **Task 27. Make the one correction attempt compulsory instead of invited.** F46: the
      retry prose offers the model a choice between correcting itself and explaining the
      failure, and every small model in the run chose to explain. When a call failed and
      nothing moved, the turn now continues with the tool-call opener already written and the
      rails armed, so there is no prose branch to take — exactly once. A second failure is a
      real answer and is spoken.
      Universality: the trigger is "the server reported an error and admitted no change",
      which no vendor knowledge is needed to read. The *forcing* half is local-only, because
      only a local model's next token is ours to pick; on a cloud backend the invitation
      stands as before, which is what it always was.
      *Done 2026-07-31* in `crates/fono-assistant/src/llama_local.rs`, with the opener
      exposed from `local_tools.rs` rather than spelled a second time.
- [x] **Task 28. Judge the harness's two behaviour rules on the house, not on the call.**
      Two of the remaining failures were the harness being wrong, and both had the same
      cause — a rule about *what the model did to the house* was implemented by looking at
      the tool call, which cannot say.
      - **A question about state.** Asked *is the balcony light on?*, the model looked it up
        with `GetLiveContext` and answered correctly, and the harness failed it for calling
        anything at all. Reading the house to answer a question about the house is the right
        behaviour. The assertion is now that nothing in the house moved (`expect_no_change`,
        renamed from `expect_no_call`), which is also the only honest way to express it:
        F47 says no server states which of its tools only read. A house that moved while the
        model called nothing at all is credited as drift, not blamed.
      - **A command that must never be repeated.** Asked for two degrees warmer, the model
        wrote the temperature against a device Home Assistant could not find, was told so,
        and tried once — nothing moved either time, and the harness charged it four degrees.
        A repeat can only double an effect if the call before it did something, so the
        executor's own failure verdict is now carried through to the scorer.
      *Done 2026-07-31* in `crates/fono/src/bench_actions/{fixture,runner,turn}.rs` and
      `crates/fono/src/assistant.rs` (`RecordedCall::failed`).
- [x] **Task 29. Learn which room each device is in, and hold the room field to it.** F48:
      with the kind of device now settled from the catalogue, the *room* is where the same
      mistake moved. Every remaining climate failure is Home Assistant answering
      `no_match_reason=AREA` — the device named is real, the kind is now right, and the room
      volunteered with it is one the device is not in. Fono cannot correct this because its
      catalogue records a device's kind but not its room, so the fix is one field further
      back: record the room at discovery beside the kind, then treat the pair exactly as
      Task 24 treats the kind — correct a room that disagrees with the device named, and
      drop the field when the device is unambiguous on its own. Four of the eleven remaining
      failures are this and nothing else.
      Universality: same seam again — the field name comes from `slot_fields().place`, and a
      server that publishes no room per device supplies nothing and is untouched.
      **Promoted to first, 2026-07-31 (F49).** Three of Task 24's corrections are already
      right and fail only on this field. Cheapest form first: when a command names a device
      the catalogue knows, *drop* the room and floor rather than correct them — the catalogue
      does not record a device's room yet, and a device name is unambiguous without one.
      Recording the room at discovery is the fuller fix and can follow.
      *Done 2026-07-31* in `crates/fono/src/actions/mod.rs` (`HouseFacts::agree`, `sole`) and
      `vendor.rs` (`SlotFields::wider_place`). Dropped the guessed room and floor, not
      corrected them, and only for a name this home uses once. Measured alone on the same
      house, rails on both sides: **9 → 11 of 22**, first-try 41 % → 50 %, drift 2 → 0, and
      p50 27.7 s → 17.7 s because the commands that used to fail and be retried now land
      first time. Both climate cells that had been correcting the kind and failing anyway now
      pass. The room was dropped 19 times in 22 cells.
- [x] **Task 30. Refuse a second attempt that repeats the first.** F49: 6 of 13 second
      attempts were the previous call byte for byte. A request identical to the one that just
      failed is not an attempt; it is a wasted round trip on the user's clock. Compare the
      parsed call with the one that failed and, if they match, do not dispatch — end the turn
      with the failure spoken. Universality: pure equality on the call Fono already parsed;
      no vendor knowledge, no schema knowledge.
      *Done 2026-07-31* in `crates/fono-assistant/src/local_tools.rs` (`same_request`) and the
      retry loop in `llama_local.rs`. Compared as JSON where both sides parse, so a reordered
      field is still the same request. **11 → 15 of 22**, first-try 50 % → 68 %, and the
      refusal fired 10 times in 22 cells — the largest single jump of the three moves, which
      was not the expected result. It was proposed to save time and it bought correctness: a
      repeat that is sent is a *second* server reply saying the same thing, and the model then
      words its answer against two refusals it has to reconcile. Refuse the repeat and it
      answers against one. Romanian 2 → 5 is almost all of the gain, and reply-language
      failures fall with it — the second refusal was crowding the language decision out.
- [ ] **Task 31. Tell the second attempt what to change.** F49: the retry re-reads the same
      raw server refusal the first attempt read, so it repeats itself or guesses a different
      device. Hand it the *reason* rather than the dump — Home Assistant states
      `no_match_reason=AREA` or `NAME`, which names the field at fault. Universality is the
      open question and the reason this is last: the mapping from a server's error shape to
      "which field was wrong" is per-server, and F47 says nothing in the protocol supplies
      it. Consider instead whether Task 29 removes the need: if the room is never sent when a
      device is named, the largest cause of a refusal is gone and the retry may be droppable
      outright rather than improved. Measure Task 29 alone before building this.
      **Re-based 2026-07-31 after Tasks 29 and 30, and now near the bottom of the list.** The
      retry is no longer the thing wasting time: it fires on 2 of 22 cells rather than 13, and
      both of those cells passed — so on the current evidence it is 2 of 2 rather than 0 of
      13. It costs one extra generation on the 10 cells where the repeat is refused, and
      nothing on the rest; p50 for the whole run is 18.7 s. The standing budget is that a
      correction may not cost more than about five seconds unless it earns better than one in
      three, and that is now met by a wide enough margin that spending per-server error
      parsing on it would be premature. Revisit only if a run shows the retry firing often and
      failing again.
- [x] **Task 33. Delete the rails switch.** F52. `assistant.tools.grammar` is gone from the
      config struct and its default, from `bench-actions --grammar on|off`, from the run
      summary, from the settings page toggle, and from the four places on the actions page that
      branched on it — the field list's "held to" annotations, the per-tool "nothing is
      narrowing these values" nudge, the "nothing but the assistant's own judgement" server
      note, and the page lede. `actions::build` now builds the rails unconditionally. Verified
      by running one bench case with no setting and no flag anywhere: `rails_bit: true`, the
      right call, passed.
- [ ] **Task 34. Refresh the catalogue when a name no longer matches.** The one real cost of
      the rails, and the reason a switch felt necessary (F52): they hold the model to the
      names Fono learned at connect, so a device renamed at the server becomes unspeakable
      until the catalogue is re-read. Trigger on the shape Task 30 already recognises — the
      server refused and nothing moved — and re-read the catalogue once before the second
      attempt rather than asking the user to notice. Universal: "the server rejected a name we
      supplied" needs no vendor knowledge, and a server Fono supplies no names for can never
      reach it. Note this makes Task 31's job partly redundant a second time over.
- [ ] **Task 35. Confirm the rails on a second local model.** F53: every number behind the
      rails comes from one model, and the case that they generalise is structural rather than
      measured. `bench-actions --backend/--model` already changes model without touching the
      config, so this is one run, not a change. Cheap enough that leaving it undone is only
      defensible while the structural argument holds — and the argument is exactly what a
      second model would test.
- [x] **Task 36. Measure what the house hint is worth, half by half.** The hint costs ~700
      tokens per turn and had never been measured; the documentation claimed thirty. Four
      arms behind `FONO_ACTION_HINT`, one house, the same 22 commands.
      *Done 2026-07-31* — F54. Answer: keep all of it. Every subtraction cost commands
      (15 → 11 → 11 → 8) and none bought time; the full hint is the fastest arm at p50 and
      p95 both. Two rules that looked redundant with Tasks 29 and 33 turned out to be what
      makes those code paths reachable. `docs/configuration.md` corrected: the token cost,
      what the setting actually sends, the one honest reason to switch it off (a cloud
      backend gets your device list), and a stale claim that a local model cannot act.
      `HintArm` stays until Task 35 closes, then goes.
- [x] **Task 32. Count a clean run as soon as its window closes.** Reported from use: a
      command given twice was still going through the assistant. The rule was right and the
      arithmetic was off by one utterance — a closed window was only ever noticed *inside a
      turn*, so the run that had just earned its window was counted on the way into the turn
      after next, and two clean runs took four utterances rather than three.
      *Done 2026-07-31* in `crates/fono-core/src/tool_catalog.rs` (`close_windows`, called from
      `settle` and from the read path both `replay` and the page use). Nothing is loosened: a
      run still has to survive its half-minute and a repeat inside it still resets the count.
      Why it slipped through: the test applied the rule in the order `remember` → `settle`,
      and a turn applies it the other way round — so the test passed on a sequence the live
      path never produces. Pinned now in the turn's own order
      (`said_twice_the_third_time_is_already_fast`), and that test fails without the fix.
      The page had the same blind spot in words: a first run still inside its window and a run
      the user contradicted both count nothing, and both were told "one more clean run".
- [x] **Task 37. Name each list once, and hold each server to its own house.** Reported from
      use: every tool on the page repeated "held to 77 devices in this home" for every field.
      The page was faithful — the rails really did write the whole house out again per tool per
      field, and the live log put the cost at 117,430 bytes of rules built and parsed on every
      turn. Two defects behind one symptom.
      *Done 2026-07-31.* Each slot list is now one named GBNF rule the tool rules refer to
      (`list-…` in `crates/fono-core/src/tool_grammar.rs`). Same language, same names, same
      sampling cost: a rule reference expands to the parse stacks an inline alternation would.
      On the real house — 22 tools, 77 devices, 14 rooms, 8 kinds — 117,430 bytes became
      22,939, and the grammar became readable enough to put in a trace.
      The second defect was worse than the first and latent: `SlotValues` was keyed by field
      name alone and the vendor was probed over the *union* of every server's tools, so one
      `Hass*` tool anywhere made every server Home-Assistant-shaped. An unrelated server with
      a field called `name` was held to your device names — the right value unwritable, which
      is the failure the rails exist to prevent, pointed the wrong way. Two homes merged.
      Slot values are now keyed by `(server, field)`, the probe is per server, and the store
      readers answer for one source (`place_names_of`, `device_names_of`, `device_domains_of`).
      A server Fono does not recognise is held to nothing, as the module always claimed.
      Pinned by what each rejects: the device list appears exactly once; a second server's
      `name` admits a string this house never heard of; a reader asked for one server does not
      answer for the other; an awkward field name still yields a rule name GBNF takes, checked
      against llama.cpp itself, because a grammar it refuses samples exactly like none at all.
      That last one is why the arming check came first — live traces read `grammar: "on"`, so
      the rails were on and the work was worth doing.
      The page states the fact once per server and keeps the per-field badge only where a tool
      departs from it — a server-published enum, or a field typed so nothing is held after all.
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
- [x] **Task 18. Shortcuts / Tier 0 replay.** Design unchanged from v4 §8–§9 and the
      answers already given to §15 Q3: a `#/actions` route carrying the retrieved tools,
      rows for tools that have run only once and are not yet on the fast path, last run
      and who and how long, many phrases per action, user-authored phrases permitted but
      never trusted more than learned ones. Deferred behind correctness for one reason: a
      shortcut replays a routing decision, and replaying a *wrong* routing decision faster
      is worse than not having shortcuts at all.

      *Split in two, 2026-07-29.* The route was asked for again from the other end — not
      as a home for shortcuts but as a debugging instrument — and the two halves have
      different dependencies, so they ship separately:

      - [x] **Task 18a. The inspector.** Read-only, depends on nothing but data already in
            the store, so it lands now. Everything the prompt is built from becomes visible
            in one place: the rendered catalogue verbatim plus its hash, each tool's schema
            as the server published it with per-field badges (`enum` / required / free
            string), the room and device and kind lists Fono authors the grammar from,
            which published field each of those lands in as claimed by the vendor probe,
            `available` vs `enabled` told apart, and the sentences about this home shown
            exactly as the model receives them — or plainly marked absent when the setting
            is off. Enable/disable per tool moves here as a bisect lever: with 23
            near-identical `Hass*` signatures competing (F33), switching one off and
            re-running separates "chose the wrong tool" from "sent the wrong arguments"
            without editing config or restarting. Grouped by MCP server, because five
            servers of ten tools each makes a flat settings section unusable — which is
            the reason "Tools & actions" in settings keeps only the servers, the
            enabled/offered counts, and a link here. *Done 2026-07-29.* The gap being
            closed is the shape of the two most expensive bugs in this plan: with F36 and
            F40 the mechanism was right and the observation point was in another crate.
            Its limit, stated so the page is not over-trusted: it shows what was stored
            and what the model was *told*. What the model actually *decoded*
            (`decoded_prefix_tokens`, `grammar: on|off|rejected`) stays in the trace. The
            page is state; the trace is the event.
      - [x] **Task 18b. The shortcut rows.** Run counts, promotion, phrase editing,
            forget. **Ungated, 2026-07-31** — the two gates it carried (Task 19, Task 23)
            both dissolved when promotion stopped being a verification question. Read the
            evidence rule below before building. *Done 2026-07-31.*

            Three deviations from the design below, all in the direction of less
            machinery:

            1. **`identity_args` and `canonical_args` are not vendor methods.** Both
               turned out to be rules about the *shape of the arguments*, which needs no
               vendor at all: `names_a_thing` refuses a call carrying a number, and
               `stable_args` re-serialises the JSON with sorted keys
               (`crates/fono-core/src/tool_catalog.rs:1481,1515`). So the fast path asks
               the vendor layer nothing, and a server nobody has seen is judged by the
               same rule as Home Assistant. The alias-collapsing half of
               `canonical_args` is unnecessary too — Task 26 made the catalogue offer the
               leading name only, so two aliases can no longer reach the model as two
               names.
            2. **The `120 ms instead of 2.4 s` pair is not shown; the single number is.**
               The baseline it needs is the tool's own last think time, and the first
               replay overwrites that with its own near-zero one — so the comparison
               would decay into a lie on exactly the rows that earned it. The row states
               what the command itself took and the section states plainly that the
               assistant is not asked, which is the claim the pair was there to support.
            3. **Not grouped by server, and no `can't be checked` label.** A phrase is the
               user's, not a server's, and the whole section is one collapsed panel above
               the servers. The label belonged to Task 23, which dissolved.

            *Design settled 2026-07-29; evidence rule replaced 2026-07-31.*

            **Evidence: three sentences.**

            1. **A run is clean** if the reply reported no error **and** the user did not
               touch the same device again **within 30 seconds** of the reply finishing.
            2. **Two clean runs of the same phrase, with the same call, make it fast.**
            3. **One dirty run, or a changed tool, makes it slow again.**

            That is the whole rule. What each part already covers, and what it replaced:

            - **"No error" is built and free.** It is the transport failure, MCP `isError`,
              `NothingWorked`, `PartlyWorked` (`crates/fono/src/actions/vendor.rs:278-282`)
              and `Contradicted` (`crates/fono/src/actions/mod.rs:869`). A partial failure
              is therefore already a dirty run, which is why the earlier per-device
              promotion clause is gone as a separate rule — it falls out of this one. The
              per-device counters still exist and still feed the UI; they are simply not a
              second gate.
            - **"Touched the same device again" replaces all correction detection.** No
              word lists for *no* / *not that one* / *undo*. A word list needs one entry per
              language and Fono is spoken in several; the device is the same in every
              language. Both error directions are cheap: a missed complaint delays a
              promotion, a false complaint keeps a phrase slow. Neither moves a device.
              That property is the reason this signal is safe to use at all.
            - **The 30-second bound is load-bearing, not a tuning knob.** Without it the
              best promotion candidates would be the ones it excludes: "turn on the kitchen
              lights" said again half an hour later is a *new* command (someone switched
              them off), not a complaint. A complaint is fast — an unobeyed user repeats
              themselves at once. 30 s is generous for that and far too short to catch a
              real second use.
            - **The clock starts when the reply finishes, not when the command arrived.** An
              action turn is ~2.4 s and the spoken reply adds more; starting at the key
              press lets a slow turn eat the window and push a real complaint outside it.
            - **"A changed tool" is the existing structural rule** — new `schema_hash`,
              `available = 0`, `enabled = 0` (v4 §5, §9.1).
            - **Two existing rules stay untouched:** `Dangerous` never auto-promotes, and a
              call that asks for an *amount* never promotes (`identity_args`) — a shortcut
              for "two degrees warmer" would double it.
            - **Judging is lazy and therefore free.** A run is scored the next time anything
              reads the phrase, which for a promotion is always after its window closed. No
              timer, no background task. A phrase never said again is never promoted, which is
              correct.
              **Corrected 2026-07-31 (Task 32).** "Read" has to mean any read, not "the next
              turn": scoring only inside a turn counted a closed window one turn late and cost
              a whole extra utterance.
            - **Asymmetric on purpose:** two positives to promote, one negative to demote. A
              promotion that does not happen costs 2.4 s once; a wrong replay moves the
              wrong thing in the physical world.
            - **The one honest weakness:** silence reads as clean — the user may simply have
              left the room. Bounded by the fact that the free error signals run first, and
              that two clean runs are required, so a single silence promotes nothing.
            - **One case to watch, not to build for:** "turn on the light" then "dim it" five
              seconds later is read as a complaint. The only cost is that the phrase stays
              slow. If it proves common the fix is one clause with machinery that already
              exists — a follow-up asking for an *amount* is not a complaint, and
              `identity_args` already tells the two apart. Do not build it before we see it.

            **Vendor boundary — nothing house-shaped leaks into the general path.** Home
            Assistant knowledge stays entirely behind `actions::vendor::Vendor`, which
            already carries `slot_fields()`, `repeatable()`, `checks()`, `confirms()` and
            now `targets()` (which things a reply says were reached, and whether each
            landed — HA reads it off `data.success[]` / `data.failed[]`; every other server
            returns empty and therefore collects no per-device history rather than a row of
            zeroes). The fast path needs exactly two further questions, both of which are
            vendor questions and neither of which is a tool-name table:
            - `fn identity_args(&self, call) -> bool` — are these arguments *naming a
              thing* (room, device, kind) rather than *asking for an amount*? Only
              identity-shaped calls may be keyed by a phrase; "two degrees warmer" must
              never become a shortcut, for the same reason `repeatable()` refuses it.
            - `fn canonical_args(&self, call) -> String` — the stable key. HA answers with
              whichever alias matched, so `Office display, Boxa birou` and `boxa birou`
              have to collapse to one shortcut instead of two. This is the same
              alias/case-folding rule `record_device_run` applies, lifted to the key.
            `Unknown` declines both, and `targets()` returns empty for it, so on an
            unrecognised server the clean-run rule has no devices to watch. It degrades to
            its weaker but still sufficient form: **no reported error, and the user did not
            say the same phrase again within 30 s.** Same two-clean-runs threshold, same
            single-dirty-run demotion — the phrase itself is the only handle, which is
            exactly the case the 30-second bound exists for. Labelled `can't be checked`
            (Task 23), which describes the wording of the reply and not the eligibility.

            **Store.** One new table, one row per phrase, no history: `shortcut(phrase_norm
            UNIQUE, phrase_raw, lang, source, tool, args_json, origin, runs, last_run,
            last_ok, enabled)`. `origin` is `learned` or `written`; a written phrase is
            executed and verified exactly like a learned one and is never trusted more —
            it starts unpromoted like everything else. Normalisation is lowercase, strip
            punctuation, fold diacritics, collapse whitespace; matching is exact on the
            normalised form, because a fast path that guesses is just a worse model.

            **UX — the page gains one section, above the servers.** *Things you can say.*
            The layout rule from the inspector rows carries over unchanged and is the whole
            reason those rows were reworked first: a **reserved, right-aligned status
            column** (`.act-ran`, two lines: outcome word, then when), so twenty phrases
            can be compared by scanning one strip instead of reading twenty sentences.
            - Row: the phrase in the user's own words, largest thing on the line. Under it,
              in mono and dim, the action it replays (`HassTurnOn · Hall lamp`) — what it
              *does* is the footnote, what you *say* is the row.
            - State, as one pill, never more: **learning** (has worked, not yet on the fast
              path — this is deliberately visible, because the list of phrases that never
              triggered is the model's blind-spot list and therefore the source of
              `bench-actions` fixtures), **fast** (replayed without the model), **written**
              (yours, still earning its place), **paused**, **can't be checked** (Task 23).
            - The payoff is a number, so show it: a `fast` row carries `120 ms instead of
              2.4 s`, taken from its own `last_ms` against the tool's. A fast path with no
              visible saving is a claim; with the pair of numbers it is a measurement.
            - Per row, two actions only: *Add another way to say it* and *Forget*. Editing
              the phrase→action mapping by hand is not offered — the mapping is won by
              verification, and hand-writing it would make the verification gate
              decorative. Adding phrases and forgetting are the two edits that cannot lie.
            - Anything `capability: dangerous` is never auto-promoted (v4 rule) and the row
              says so in place of the promotion state, rather than silently missing one.
            - Grouping: by the server that offers the action, same as the tool rows, and
              collapsed by default once there are more than a screenful.

### Debts carried forward from v4 (small, concrete, still owed)

- [x] **Task 19. No-op detection — dropped, 2026-07-31. Not a task; a note.** The debt was
      that `confirms` counts an already-on light as `Confirmed` (v4 §7.2), and the stated
      reason to fix it was that a no-op "is not evidence that targeting was correct". That
      reason does not survive reading the code: **a `Confirmed` verdict never proves correct
      targeting in any case.** `confirms` looks up the devices the *server itself claimed it
      touched* — `claimed_entities` reads `data.success[]`
      (`crates/fono/src/actions/vendor.rs:299,392-397`) — not the devices the user asked
      for. Send the command to the wrong lamp, watch that lamp obey, and the verdict is
      `Confirmed`. A pre-state read does not close that hole; it only distinguishes "something
      changed" from "nothing changed". So the proposed fix would have traded one weak signal
      for another, at the price of an extra round trip, and still not been promotion-grade.

      What retires it is that the behavioural rule in Task 18b covers all three cases and
      costs nothing: an already-on lamp with a satisfied user is clean (the user asked for a
      state and has it); a wrong lamp is dirty on the next turn — **the exact case the
      pre-state read cannot see**. Two further points that were about to be paid for and are
      not needed: the check is already narrow (`checks()` requires a tool that names a state,
      `crates/fono/src/actions/vendor.rs:285-287`, so a weather question is never `Confirmed`
      — it is `Accepted`), and the readback is a plain MCP call at ~100 ms with no model in
      the loop (`crates/fono/src/actions/mod.rs:915`), so the latency objection was never the
      strong one.

      **Two things are still owed, both cheap and neither a gate:** the actions page should
      read *"state as asked"* rather than *"confirmed"* for a post-condition pass, and the
      limit above — a pass proves the claimed devices reached the named state, and nothing
      more — belongs in a doc comment on `confirms` so a later task cannot over-trust it.
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
- [x] **Task 23. Answered, 2026-07-31 — and it shrank.** The question (v4 §15 Q6) was what
      may be promoted on a server whose payloads `for_result` does not recognise, since
      `Unknown` claims nothing by design and nothing would ever be promoted. It stops being
      a dilemma once promotion is not a verification question: under the Task 18b rule an
      unrecognised server promotes on **exactly the same evidence as any other** — no
      reported error, and the user did not come back within 30 s — because that evidence
      does not depend on a vendor reading the reply. `Dangerous` still never auto-promotes.

      What survives of the original recommendation is the honesty of the *wording*, not the
      eligibility: the `can't be checked` pill stays, and now says only that Fono cannot
      narrate more than "it was sent". It no longer means "cannot be learned". Nothing here
      gates Task 18b.
      *Renumbered from 22, 2026-07-28: two unrelated tasks carried that number — the
      grammar A/B in Tier 2 and this one — so "Task 22" was ambiguous in a plan whose
      other entries cross-reference it by number. Every existing reference to "Task 22"
      means the grammar A/B.*

- [x] **Task 38. Refuse a rule name llama.cpp cannot read, and prove the grammar is
      accepted.** F55. `device_class` put an underscore in a GBNF rule label and llama.cpp
      threw away all 23 KB of the grammar, so every command in a whole run was written
      unconstrained while the trace said the rails were on.
      *Done 2026-07-31* in `crates/fono-core/src/tool_grammar.rs`: the label keeps letters,
      digits and hyphen only. Two tests where there were none — a plain one that refuses any
      rule name outside llama.cpp's alphabet, and an acceptance test that hands a grammar
      built from a field name of exactly this shape to llama.cpp and asserts it was taken.
      Both fail without the fix. `crates/fono/src/bench_actions/mod.rs` now ends a run by
      naming the traces that said `grammar: "rejected"`, because a run with the rails off is
      a measurement of a different thing and reads exactly like a worse model.
- [x] **Task 39. Hand the model what Fono saw, alongside what the server claimed.** The
      old check only knew two tool names, so a tool that sets a value was never verified at
      all and a false success went unopposed: Home Assistant answered `action_done, success`
      on a light that was still on, Fono's own note said nothing had moved, and the model —
      never told — announced the light was on.
      *Done 2026-07-31* in `crates/fono/src/actions/{mod,vendor}.rs`. Any acting call that
      names what it touched is read back, and the outcome the model receives carries both the
      server's claim and the reading, so the two can be compared rather than one trusted. The
      verdict stays `unproven` when the desired end state is unknown, because the readback
      carries `state` and no attributes — a successful dim would otherwise be reported as a
      failure. Cost: 5 extra reads in a 22-command run at ~200 ms each.
      The finding is the disagreement rate. **The server's claim and the house disagreed in 4
      of 5 checks.** Reading the house back is not belt and braces. The `unproven` case earned
      its keep immediately: handed "Air conditioner is off", the model said so, where before
      it would have claimed the temperature was set.
- [x] **Task 40. Make a system-prompt note cheap again.** F56, F57. Done, and the diagnosis in
      F56 was wrong: the note was not the cost. The cache filed the turn-start prefix in the
      same layer as the mid-turn wording pass, whose prefix strictly contains it, so every
      turn deleted its own useful checkpoint seconds after writing it; and the longest-prefix
      search did not look in the layer the mid-turn pass writes to. Split by lifetime
      (`GenParams::prefix_outlives_turn`) and both layers searched. 22 cold reads → 2, p50
      49.2 s → 9.9 s, with the language named. Unblocks Task 41.
- [x] **Task 41. Tell the model which language it is answering in.** F56, F57. The benchmark
      now declares the language, as the dictation path already did
      (`crates/fono/src/assistant.rs:678-687`) — so the earlier claim that the detected
      language never reaches the model was wrong, and only the live push-to-talk path still
      withholds it (`session.rs:4612`, and it says so). Romanian 5 → 6 on the run with the
      cache fixed. Surfacing it on the live path is left as its own task, since it needs the
      recogniser verdict plumbed rather than a prompt change.
- [x] **Task 42. Do not carry out a command in silence — the instrument half.** F58, F59.
      A turn that acted without saying anything now fails, and says why. It used not to: an
      empty reply has no language to judge and makes no claim to weigh, so both of those
      checks answered "not applicable" and the verdict read not-applicable as satisfied.
      Silence did not slip past one assertion, it switched two off. Scored on its own field
      so the two remain independent. The honest number is 10 of 22, not 15.

      The preamble-stripping suspicion recorded here was wrong; the cause is F59. Left as
      Task 43.
- [ ] **Task 43. Say something when the forced command is discarded.** F59. The correction is
      written into the model's mouth as a tool opener, so the pass can only produce a command;
      when that command is then refused as a repeat of the one that just failed, there is no
      prose anywhere in the pass and the turn ends in silence. Eight of twenty-two commands.
      The fix has to produce words, which means one more wording pass with no opener written
      and the failure described — not a canned sentence, which would be in the wrong language
      and would claim more than Fono knows. Cost is one short generation on a path that has
      already failed once, so it is paid only where the turn is going badly anyway.

      The general shape, worth applying beyond this instance: a mechanism that removes the
      model's alternatives owns the case where its own output is discarded.

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
