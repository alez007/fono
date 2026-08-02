# Voice actions: fast and right

Target: **a spoken command lands in under 10 s, most of the time under 5 s**, and
the reply is true and in the user's language. Today the median is 27.8 s and the
95th is 58.1 s.

Everything below is measured on the run in
`~/.local/state/fono/bench/actions/1785611827/` (22 turns, `gemma-4-e2b`, local,
en + ro) and on three thread-affinity probes run the same evening.

---

## 1. Where the time goes

### 1.1 Tokens are the only currency, and we cannot make them cheaper

Decode rate against core count, same fixture, three repeats each:

| cores given to the process | decode threads | decode tok/s (median) | prefill tok/s |
|---|---|---|---|
| all 8 (4 P + 4 LP-E) | 7 | 15.4 | 50 |
| 0–3 (P only) | 3 | 14.6 | 30 |
| 0–5 | 5 | 12.4 | 34 |

**Decode does not scale with cores.** Three P-cores decode as fast as seven
mixed ones, so decode is memory-bandwidth bound: there is no thread setting, no
core pinning and no scheduling trick that makes a generated token cheaper on
this CPU. Prefill *does* scale, so reading is compute bound.

The consequence sets the whole plan: **the only ways to go faster are to
generate fewer tokens, to read fewer tokens, and to take fewer turns.** (A
fourth way — the machine's Arc iGPU or NPU — is out of scope here and is a
binary-size decision, not a code decision.)

Rates to budget against, warm: **~15 tokens/s written, ~40–50 tokens/s read.**

### 1.2 The measured turn

Summed over 22 turns: 454 s total, of which

| span | n | sum | mean |
|---|---|---|---|
| `llm.generate` | 61 | 196 s | 3.2 s |
| prefill (suffix, i.e. tool results + the user's line) | 61 | 153 s | 2.5 s |
| prefill (cold prefix) | 41 | 86 s | — |
| `tool.execute` | 24 | 6.5 s | 0.27 s |
| `tool.verify` | 16 | 3.4 s | 0.21 s |

**The house is free. Talking to the model is everything.** Home Assistant
answers in 270 ms and the readback in 210 ms; together they are 2 % of the run.

A turn that goes right first time costs about 5 s (turns 3, 4 and 13 measured
4.8–6.1 s). Everything above that is a second or third round trip.

### 1.3 The five specific wastes, largest first

**a. Reading the whole house back — 15 s and 33 s in single turns.**
`GetLiveContext` answers with every entity in the home. Two turns fed **517**
and **1,090** tokens of tool result back into the model, costing 14.8 s and
33.0 s of prefill *for one question about one light*. The tool already accepts
`area`, `domain` and `name`; we call it without them, and `brief()` caps the
answer at 2,000 characters, which is far too generous at 40 tok/s. This is the
single largest item in the whole benchmark and it is entirely self-inflicted.

**b. Ordinary tool results — 62 to 158 tokens, 1.0–4.3 s each.**
We feed Home Assistant's raw JSON envelope back verbatim:
`{"speech": {}, "response_type": "action_done", "data": {"success": [{"name":
"Balcony lights", "type": "entity", "id": "light.balcony_lights"}], "failed":
[]}}`. The model needs one line: *Balcony lights is on*. We are paying 1–4 s per
turn to read punctuation.

**c. A second generation to say one sentence — 0.5–3.7 s.**
After the call lands we prefill the result and generate a confirmation of
9–56 tokens. We already know the truth (`tool.verify` read the house back
before we asked). We are paying the model to restate a fact we hold.

**d. Fields nobody wants — about 20 wasted tokens per call, 1.3 s.**
The model writes every property the schema mentions, empty ones included:

```
{"area":"Yard","brightness":100,"color":"","domain":["light"],"floor":"","name":"Balcony lights"}
```

`color`, `floor` and (after "narrowest target wins") `area` are all deleted
before the call is sent. The median first generation is 42 tokens; the call
actually sent is about 20. Half of every command's cost is text we throw away —
and `brightness: 100` in that example is not merely waste, it is the invented
value that made the call wrong (§2.1).

**e. Cold start: 1,561 tokens read twice, 83 s.**
Turn 1 reads the prefix cold (41.0 s). Turn 2 reads *the same prefix* cold again
(42.3 s), logging `cold_prefill reason=no_prefix_match`, and only then stores it;
turns 3–22 all hit. The first turn's prefix is not landing in the cache. In the
daemon this is paid once per model load, but it is 41 s of a user's first
command after every restart, and it is a bug, not a cost.

---

## 2. Why commands still fail

Six failed and one drifted out of 22. Grouped by cause, not by fixture.

### 2.1 A value nobody asked for, still

`pornește Air conditioner` → `HassClimateSetTemperature {..., "temperature": 21}`.
Route A now stops that call from being sent, and the model usually corrects
itself — but it costs a full extra round trip (5–9 s) every time, and it fired
**15 times in 22 turns**. The refusal is right; paying for it twice per command
is not.

Root cause is upstream of the model: Home Assistant declares **no required
fields on any tool** (verified across all 26), so the rails permit a call with
no `temperature` — yet the model, shown a tool named *set temperature*, writes
one. The prompt rule ("if a tool cannot be called without a value nobody gave,
it is the wrong tool") does not hold.

### 2.2 Silence, wrong language, and untrue confirmations

Three faults, one cause: **we ask the model to report on something we already
checked.** In the last run it acted without speaking, answered Romanian in
English, and said "I turned it off" about an air conditioner the readback showed
still running in `dry`. `tool.verify` had the true state in hand 200 ms after
the call, in every one of those turns.

### 2.3 The wrong target when the device was named out loud

`pornește Air conditioner` → `HassClimateSetTemperature {"area": "Garage"}`.
The utterance contains a catalogued device name **verbatim** and the call
targets somewhere else entirely. Nothing mechanical stops it today.

### 2.4 A question answered by acting

*"Sunt aprinse luminile din bucătărie?"* → `HassLightSet`, then `HassTurnOn`.
A question moved the house. This is the one failure class where being wrong
costs the user something real.

### 2.5 Two that are the house, not us

- **The Office air conditioner sits in `dry`**; `HassTurnOff` does not move it.
  The readback knows. The fixture scores a failure and the model claims success
  — the honest outcome is to tell the user it did not turn off (§2.2 fixes it).
- **`Guest bedroom lights`** is switched on, Home Assistant reports success, and
  the readback says `off`. Needs one look at whether that entity is a group.

---

## 3. What we cannot do at all

From the live catalogue (26 tools) and the house (79 devices, 8 kinds):

| the user asks for | state |
|---|---|
| lights on/off, brightness, colour | works |
| rollers / covers open, close, stop, position | tools exist (`HassSetPosition`, `HassStopMoving`), **never tested** |
| volume set, step, mute | set + step tested; **mute untested** |
| air conditioning on/off | works, slowly |
| air conditioning to a temperature | **never tested** — and it is the tool that misfires |
| the lock | **no tool exists.** The house reports `lock: FRONT DOOR`; nothing in the catalogue locks it |
| shopping list | **`HassListAddItem`, `HassListCompleteItem`, `HassListRemoveItem` are disabled.** Only `todo_get_items` is on, so Fono can read the list and not change it |
| media play, pause, next, search | tools exist, **never tested** |
| vacuum start, return | tools exist, **never tested** |
| "everything off in here" | `__all__` now taught, **never tested** |
| "turn on the sauna" (no such device) | **never tested** — should refuse |
| "and turn the lights off too" (follow-up) | **never tested**; history is dropped after an action |

The committed suite is 11 cases. Six of them are lights.

---

## 4. The plan

Ordered by seconds saved per unit of work. Each slice states what it costs the
user if it is wrong.

### Slice 1 — Ask the house one question, not all of them

`GetLiveContext` is called with the filter the user's words already supply
(`name`, then `area`, then `domain`), and its answer is capped hard — a few
hundred characters, not two thousand.

*Saves:* 14–32 s on state questions, which are the p95 of the whole benchmark.
*Risk if wrong:* the model lacks a fact it needed and asks again — one extra
round trip, on a turn that costs 33 s today.

### Slice 2 — Feed back one line, not an envelope

Replace the raw tool JSON in the next prompt with the sentence `tool.verify`
already composes: `Balcony lights: on`. Failures keep their reason.

*Saves:* 1–3.5 s per turn, every turn with a tool call.
*Risk if wrong:* the model loses detail on an unusual failure; keep the raw text
whenever the call errored.

### Slice 3 — Speak the sentence the model already wrote

The model writes a sentence before the call, in the user's language, and Fono
throws it away. Then Fono pays a second prefill and a second generation to get
an equivalent sentence back.

Keep the first sentence instead. When `tool.verify` agrees that the call did
what the sentence claims, speak that sentence and end the turn. When the
readback disagrees, or the call failed, or the model wrote no sentence, do the
second generation as today — with the true reading in it.

*Saves:* the whole second round trip on the success path: 1.0–4.3 s of result
prefill plus 0.5–3.7 s of generation. Those tokens are already paid for.
*Universal:* no table of any kind. The sentence is the model's own, so it is in
the user's language by construction, whatever that language is, on every
backend.
*Risk if wrong:* the sentence is a promise ("I will turn on the lights") spoken
after the fact. The house has already confirmed it, so the promise is true.

### Slice 4 — Offer only the tools the words can fill

Three mechanical rules. None of them reads a word of any language.

1. **A field Fono would delete cannot be written.** `str` in the rails becomes
   non-empty, so `"color":""` and `"floor":""` never cost a token.
2. **A number comes from the user, never from the model.** The digits in the
   utterance become the closed list of values a numeric field may hold, exactly
   as device names already do. No digits means no list, and the field leaves
   the grammar for that turn.
3. **A tool with no writable value field leaves the grammar too**, when those
   value fields are the only thing that separates it from the plain on/off tool.
   Target fields (`name`, `area`, `floor`, `domain`, `device_class`) are already
   named in the vendor slot table, so "value field" needs no new knowledge.

Rule 3 is what stops the earlier dead end, where the number was dropped and
`HassClimateSetTemperature {"name": "Air conditioner"}` went out with nothing to
set.

*Saves:* about 20 wasted tokens per call (1.3 s), plus the 5–9 s retry on the 15
turns in 22 that refuse and retry today.
*Universal:* digits are the same in every language Fono hears. The rails are
local-only, so cloud backends keep the ask-once refusal, unchanged.
*Risk if wrong:* a user who says a number in words rather than digits loses the
fast path. The existing escape hatch already covers it — the refusal invites the
same call a second time and sends it. **Measure before building:** find out what
Whisper actually writes for a spoken "seventy". If it writes `70`, the risk is
close to zero.

### Slice 5 — A named device is the target

If the utterance contains a catalogued device name verbatim, that device is what
the call acts on: `name` is set to it, and `area` and `floor` are dropped. Exact
string matching, no language, no vendor.

*Saves:* accuracy, not time — fixes §2.3.
*Risk if wrong:* a device whose name is also a common word ("Gate", "TV Living")
captures a command meant for an area. Match only on the full name, longest
first, and only when the model gave no name of its own.

### Slice 6 — Warm the cache on the first turn

Find why turn 1's prefix does not reach the cache and turn 2 re-reads 1,561
tokens. Then continue with the disk tier already planned in
`plans/2026-08-01-prompt-state-cache-disk-tier-v1.md`.

*Saves:* 41 s, once per restart, on the user's first command.

### Slice 7 — Fix `required` upstream in Home Assistant

Home Assistant declares **no required field on any of its 26 intent tools**.
`HassClimateSetTemperature` accepts a call with no `temperature`, and
`HassSetVolume` one with no `volume_level`. The schema therefore states that a
tool named *set temperature* has nothing it must set.

That is the origin of the defect the user reported. Fono works around it inside
its own rails, and every other Home Assistant client repeats the same work.

One field added to a handful of intent schemas upstream makes the bad call
unwritable for everybody who constrains a model with a schema.

*Effort:* small, and it belongs to Home Assistant, not to Fono.
*Second candidate:* the house reports a `lock` device and the catalogue holds no
intent that locks it. Confirm this, then use the intent that exists or propose
one.

### Slice 8 — Cover what the house can actually do

Grow the suite from 11 cases to roughly 25, in this order:

1. **Enable the shopping-list tools** and add *add / complete / remove an item*.
2. **The lock** — find out whether Home Assistant exposes an intent for it at
   all; if not, say so in the docs rather than letting the model improvise.
3. Rollers: open, close, stop, "half way".
4. Air conditioning **to a temperature** — the tool that misfires has no test.
5. Volume mute and unmute.
6. Media: pause, resume, next.
7. "Turn everything off in here" (`__all__`), and one whole-floor command.
8. An unknown device — must refuse, must not act.
9. A follow-up that refers to the previous command.

And two harness changes: default `--repeats` to 3 so a coin toss stops reading
as a result, and record **the call as sent** in `detail.json` beside the model's
draft, as the conversation log now does.

---

## 5. The budget this adds up to

A one-call command, after slices 1–4, at the measured 15 tok/s written and
45 tok/s read:

| step | now | after |
|---|---|---|
| read the user's line | 0.2 s | 0.2 s |
| write the call | 3.2 s (42 tok) | 1.4 s (20 tok) |
| Home Assistant | 0.3 s | 0.3 s |
| read the house back | 0.2 s | 0.2 s |
| read the result | 1.0–4.3 s | 0.3 s |
| say the confirmation | 0.5–3.7 s | 0 |
| **total** | **5.4–12 s** | **~2.4 s** |

with the retry (currently 15 turns in 22) removed rather than shortened. A
median under 5 s is reachable; the 10 s target has room in it for a second call
when the model genuinely needs one.

---

## 6. Order of work

Ranked on three tests: does it hold in every language and on every server
(universal), how much code it is (simple), and how many seconds or failures it
removes (effective).

| # | slice | universal | simple | effective | note |
|---|---|---|---|---|---|
| 1 | 3 — speak the sentence already written | yes | yes | 1.5–8 s on ~16 turns in 22 | also removes silence and wrong-language replies |
| 2 | 1 — ask the house about one thing | yes | yes | 14–32 s on the worst turns | the whole p95 |
| 3 | 4 — offer only the tools the words can fill | digits only | medium | 1.3 s per call + 5–9 s on 15 turns | largest total, most code |
| 4 | 8 — repeats, and the missing device kinds | yes | yes | none directly | without it no number above can be trusted |
| 5 | 5 — a named device is the target | yes | yes | one failure class | exact string match |
| 6 | 2 — feed back one line | yes | yes | 1–3.5 s, retries only after slice 3 | |
| 7 | 6 — warm the cache on the first turn | yes | unknown | 41 s once per restart | find the cause first |
| 8 | 7 — `required` upstream | for all Home Assistant clients | yes | removes the cause of slice 4 | not Fono's code |

Slices 3 and 1 need no new concept and no new data. Do them together, then
re-measure with three repeats before starting slice 4.

---

## 7. Built: slices 1, 2 and 3 — measured

22 turns, English and Romanian, `gemma-4-e2b`, one repeat per cell.

| | before (A-ask) | after |
|---|---|---|
| worked in the end | 68 % | **73 %** |
| worked first try | 41 % | **55 %** |
| median turn | 27.8 s | **11.3 s** |
| slowest tenth | 58.1 s | **20.8 s** |
| model time, whole run | 454 s | 312 s |
| tokens read back after a call | 148 s, largest 1,090 | **72 s, largest 491** |

One repeat per cell, so the accuracy figures are three cases wide and could be a
coin toss. The timings are not: every turn got faster.

**A regression found by the run, and fixed.** Five Romanian commands were read
out as JSON and never run. The command is held back while it is being written,
so it is not spoken by mistake; the code then treated what it was holding as the
record of what the model wrote. One delivery hiccup and the hold is empty while
the command sits, whole and parseable, in the finished reply. Whether a turn
carries a command is now decided on the whole reply.

**Fixed since: each language paid its own cold read of the whole prompt.** Turn 1
paid 39 s and turn 2 paid 35 s, 74 s of a 312 s run. Corrected reading of the
traces: these were not the same prefix twice. Both are 1,579 tokens and both are
identical up to the last four words, where one says *"Reply in English."* and the
other *"Reply in Romanian."*. Exactly two prefix hashes appear across all 22
turns, alternating with the language, each cold once. A checkpoint is only usable
when its whole token list is a prefix of the new prompt, and the only checkpoints
taken were of a whole prefix, note included — so nothing was ever stored at the
boundary *before* the note, and a second language started from token zero. The
same would happen for a change of speaker, which sits in the same slot. Two
consequences worth separating: the benchmark never warmed the cache at all
(`cache_entries: 0` on the first lookup), so it paid this twice, while the daemon
does warm the note-free head at startup, which every language shares.

Built: the cold read now stops at the head boundary, checkpoints there under the
same pinned key the startup warm uses, and reads on. The benchmark warms the
cache the way the daemon does. A test asserts the head leads a prompt in either
language and is where the two part; the token-level check is a runtime
`starts_with` that falls back to reading straight through.

Measured, same 22 turns, en + ro:

| | before | after |
|---|---|---|
| cold whole-prefix reads | 2 × 1,579 tokens, 35 s and 39 s | **none** |
| median turn | 11.3 s | **9.9 s** |
| slowest tenth | 20.8 s | **14.8 s** |
| model time, whole run | 312 s | **208 s** |
| worked in the end | 72.7 % | 68.2 % |
| worked first try | 54.5 % | 54.5 % |

The two turns that used to carry the cold read fell 56.8 s → 16.5 s and
49.9 s → 9.9 s. The accuracy figure moved by one case, which is noise at one
repeat per cell. The warm itself costs 42.5 s once, before any turn — in the
daemon that is paid at startup, and the disk tier in
`plans/2026-08-01-prompt-state-cache-disk-tier-v1.md` would carry it across
restarts.

The head checkpoint did not fire in this run: the warm already pinned the same
entry, so the read found it restored and had nothing to stop for. It earns its
keep when the warm has not run or its head is stale.

**Still open, in the order they now cost the most:**

1. A multi-device command still reports every device it reached: one 491-token
   result cost 11.3 s to read.
2. Slice 4 — 8 of 22 first calls were still refused for a value nobody asked
   for, each costing a retry.
3. The air conditioner sits in `dry`, where Home Assistant's `turn_off` does not
   move it. Not ours.

## 8. Built: the readback summary, and four cases that widen the suite

### The roll call is now a count

`state_of_the_house` recited every device a command reached, unbounded. One
twelve-lamp living-room command produced 358 characters of `Couch is on, Couch
is on, Couch Blue is off, …` — read twice, because the turn retried, for 5.7 s
of a 15.3 s turn. Two of the entries were the same device twice, and a sensor
was in there as well.

It now drops exact repeats, names every device while there are fewer than five,
and above that groups by equality of the state string: the largest group gets a
count and every smaller group is counted **and** named, capped at six names.
The same example becomes `11 devices — 8 are off; 3 are on: Couch, Living
square red, Living square white` — 181 characters, and the devices that
disobeyed keep their names, which is the only reason to read the line at all.
Grouping is string equality, so it knows nothing about what `on`, `cool` or
`41` mean and works against a server Fono has never seen.

### The suite went from 11 cases to 15

Six of the eleven were lights. The four added are the kinds of command a person
actually gives that nothing exercised:

- **`open_a_cover`** — a blind reports `open`/`closed`, not `on`/`off`, and the
  same two switch intents move it. The harness had to learn that a state it had
  never seen is still a state it can stage and put back.
- **`an_unknown_device_is_refused`** — the worst failure short of a lie: asked
  for something the home does not have, a model quietly does it to the nearest
  thing that is. Skips on a home that owns the name.
- **`an_area_of_several_lights_all_arrive`** — an area holding at least three
  lights, where every member has to land. The existing area case can resolve
  onto an area with one lamp, where "they all came on" and "it came on" are the
  same sentence.
- **`set_a_temperature_uses_the_climate_tool`** — the mirror of the switch-on
  case. A suite that only ever *forbids* the temperature field says nothing
  about whether the model can still reach for it when asked, and the fix for the
  invented value is exactly the kind of fix that could break this. Every
  language spells the number in words in at least one of them, so it is also the
  end-to-end test of the escape hatch.

`dim_uses_the_brightness_tool` gained `expect_level = 30`: it pinned the tool
and never checked the lamp, so a refused call scored as a pass.

### Harness changes the four cases needed

- **An unavailable device is never a target.** Six cases of one run scored
  `failed` while a hub was offline — a number about the house reported as a
  number about the model. A domain that is present but wholly unreachable now
  skips, and says so. `unknown` is deliberately still targetable: it means
  nobody has reported yet, not that the device is beyond reach.
- **`dimmable_device` is now `adjustable_device`** and asks for any settable
  level, so it covers a speaker's volume and a blind's position, not only a
  lamp's brightness.
- **`--show-house` marks unreachable devices**, which is the commonest reason a
  run comes back full of skips and was invisible in a list of states.

### What the four found on first contact

Each of them caught something, and three caught the same thing.

- **A named device is not being used as the target.** `open the Curtain Left`
  produced `HassTurnOn {"area": "Guest bedroom", "domain": ["cover"], "floor":
  "__all__"}` — a different cover, in a different area, and the reply claimed
  success. `set the Air conditioner to twenty-three degrees` reached the
  *Master bedroom thermostat*. The catalogued name is in the utterance verbatim
  and nothing mechanical uses it. This is now the largest accuracy item, and six
  light cases never showed it.
- **`__all__` is leaking into `floor`.** Stripped before the call travels, so it
  is harmless today, but the prompt teaching is being over-applied.
- **An area command is being narrowed to one device.** `turn on all the lights
  in the Kitchen` produced `HassLightSet {"area": "Kitchen", "domain":
  ["light"], "name": "Kitchen lights"}`; two of the three lamps stayed off and
  the reply said they were on.
- **The escape hatch works end to end.** `twenty-three degrees` carries no
  digit, the call was stopped once, the model wrote the same call again and it
  was sent. First proof of the designed behaviour, at the cost of one retry.
- **A refusal can be the fastest turn in the suite.** The unknown device was
  refused in 1.1 s with no call at all.

## 9. The first full run of the widened suite on a healthy house

30 turns (15 cases × en + ro), `gemma-4-e2b`, 14 devices offline which is this
house's normal baseline. Four turns skipped for want of a target, so 26 scored.

Latency has arrived: **median 8.8 s, slowest tenth 14.4 s**, against 27.8 s and
58.1 s three changes ago. The 10 s target is met at the median. Prefill, which
was the largest item in the run before last, is spent: 239 s → 66 s over a
longer suite, median 12 tokens per read.

**What is left is one number: 2.4 generations per turn.** A turn that generates
once takes 2.3 s; a turn that generates twice or more takes 9.5 s. Generation is
now 143 s of the 230 s the model spent. Every remaining latency question is the
same question — why did this turn need a second attempt.

Sorting the 38 call results by what happened answers it:

| what became of the call | n | share |
|---|---|---|
| Fono refused an invented number | 10 | 26 % |
| the house did not reach what was asked for | 9 | 24 % |
| landed cleanly | 9 | 24 % |
| the server refused the call outright | 8 | 21 % |
| answered from a reading, or partial | 2 | 5 % |

Only a quarter of calls land. The two largest groups have one cause each, and
both causes are mechanical.

### The named device is not being used as the target

Nineteen turns spoke a catalogued device name **verbatim**. Ten put that name in
the call. **Nine did not** — and every one of the nine substituted an area,
which is a different device:

| said | first call | reached |
|---|---|---|
| turn on the Balcony lights | `{"area":"Yard","domain":["light"]}` | three lights in the Yard |
| open the Curtain Left | `{"area":"Guest bedroom","domain":["cover"]}` | the Guest bedroom roller |
| turn off the Air conditioner | `HassTurnOff {"area":"Master bedroom"}` | both beds, two lights, a salt lamp and a curtain |
| set the Air conditioner to 23 | `{"area":"Master bedroom"}` | the Master bedroom thermostat |
| pune Air conditioner la 23 | `{"area":"bucătărie"}` | the Kitchen thermostat |

The third row is the one to look at twice: a request to switch one device off
switched off a bedroom. The last row also translated an area name into Romanian
and invented one the home does not have, which rule 1 forbids in words.

Six light cases never showed this, because a light called `Balcony lights` is in
the Balcony and the wrong mechanism reaches the right device. Covers and climate
are where the coincidence stops.

### A tool the words cannot fill is still being offered

Eight calls were refused by the server, and every one of them was
`HassClimateSetTemperature` **with no temperature at all**. Route A stops the
invented value; the model then writes the same tool with the field missing, and
the house rejects it. Stopping the value was half the job — the tool is still
the wrong tool.

Together these two groups are 45 % of all calls and the whole of the second
generation. Neither needs a word of any language.

### Smaller findings

- **The escape hatch is now routine, not exceptional.** Ten refusals, and the
  model rewrote the same call and got through on `seventy` and on `twenty-three
  degrees` in both languages.
- **A third of every call the model writes is discarded.** 573 tokens drafted,
  381 sent. Sixteen calls carried a blank field; eight put `__all__` in `floor`,
  where it means nothing.
- **A refusal is the fastest turn in the suite** — the unknown device was
  refused in 0.7 s with no call at all. The Romanian half of the same case
  instead aimed a temperature tool at the Kitchen and failed twice.
- **A blind cannot be opened.** Both languages sent a switch intent at the
  area and moved a different cover, then reported success. `open` and `closed`
  are being treated as `on` and `off` by the model as well as by the harness,
  but at the wrong target.

### One harness gap to close

`plain_switch_on_invents_no_brightness` and `dim_uses_the_brightness_tool` both
skipped: no light in the house reports a level. That is true only because a
light reports its brightness **when it is on**, and this house has exactly one
lit lamp, which shares its name with a twin and so cannot be commanded by name.
The requirement is state-dependent and should not be: the resolver should be
able to switch a candidate on, read it again, and use it. Two guard fixtures for
the invented-value defect are silently absent until it is.

## 10. Built: the three free fixes, and what they moved

Same 15 cases in English and Romanian, same house, same model. Four cases still
skip for want of a lit lamp that reports a level.

| | before | after |
|---|---|---|
| worked in the end | 57.7 % | **84.6 %** |
| worked first try | 50.0 % | **65.4 %** |
| passed / recovered | 13 / 2 | **17 / 5** |
| drifted / failed | 2 / 9 | **0 / 4** |
| median turn | 8.8 s | 8.8 s |
| slowest tenth | 14.4 s | 13.4 s |

Latency is unchanged, which is the point: none of the three costs a round trip,
a prompt character or a word of any language.

Sorting the calls by what became of them shows where the gain came from:

| | before | after |
|---|---|---|
| landed cleanly | 9 (24 %) | **16 (41 %)** |
| the house did not reach what was asked | 9 (24 %) | 6 (15 %) |
| the server refused the call outright | 8 (21 %) | **2 (5 %)** |
| Fono refused an invented number | 10 | 10 |
| Fono refused a value tool with no value | — | 4 |
| calls carrying a blank field | 16 | **3** |

### The name the user spoke is the target

A device name the *server itself* published, that exactly one device answers
to, found whole in the words at a word boundary, is put in the device field and
the wider place is dropped after it. Longest match wins, and a name that is
also an area name in this home is left alone, so "everything in the Office"
is never narrowed to a device called *Office*.

It works, and the record shows it working: `aprinde Balcony lights` produced
`{"area": "Yard", "brightness": 100, "color": "white"}` — no device at all —
and what travelled was `{"name": "Balcony lights", …}`. Six climate and cover
turns that used to reach a whole bedroom now reach the one device.

### A tool that sets one thing must be given it

Where a field is the only value a tool sets, and no tool sharing that field
sets anything else, the field is compulsory whatever the schema says. It is
insisted upon in the rails, so a local model cannot write the empty call, and
refused at the door for a backend with no rails.

Server refusals fell from 8 to 2, and all eight of the old ones were the same
empty `HassClimateSetTemperature`.

### Nothing writable that Fono deletes

Text in the rails can no longer be empty. Calls carrying a blank field fell
from 16 to 3.

### What is left

- **`__all__` in `floor` is now on 25 calls of 39**, up from 8. Harmless — it
  is stripped before the call travels, and the wider place is dropped whenever
  a nearer target exists — but it is written every time and paid for every
  time. `floor` is the one target field with no published list of values, so
  it is free text and the everything-word leaks into it. Pinning it needs a
  floor catalogue the server does not currently offer.
- **A blind's state is not understood.** `Curtain Left` was asked to open, was
  open afterwards, and the readback called that a disagreement, because only
  `on` and `off` are recognised as the two ends of a switch. The model then
  "corrected" itself with a switch-off and claimed success.
- **A refusal can end the turn in silence.** `turn on the Air conditioner` was
  refused once and the model wrote nothing further — no call, no sentence. The
  complaint has to leave the model somewhere to go.
- **An air conditioner in `cool` does not answer `turn_off`**, which is the
  house, not Fono.

### The recogniser and digits

The assistant path now carries a four-number demonstration as the recogniser's
prompt. A recogniser's prompt is a continuation channel, not an instruction
channel, so asking for a spelling in words does nothing and showing it is the
only thing that works. Digits and punctuation belong to no language, so the
same handful of characters serve every language Fono hears, and a leak into
the transcript is a stray digit rather than a stray sentence. **Unmeasured:**
the command benchmark feeds text and never opens the recogniser, so this needs
a spoken test before it is believed.

### Measured: 58 % → 77 %, at the same speed

Same 15 cases in both languages, same house, four skipped for want of a target.

| | before | after |
|---|---|---|
| worked in the end | 57.7 % | **76.9 %** |
| worked first try | 50.0 % | **61.5 %** |
| failed | 9 | **5** |
| median turn | 8.8 s | 9.3 s |
| slowest tenth | 14.4 s | 14.6 s |

The call ledger says which change did what:

| what became of the call | before | after |
|---|---|---|
| landed cleanly | 9 | **15** |
| the house did not reach what was asked | 9 | **7** |
| the server refused the call outright | 8 | **2** |
| Fono refused an invented number | 10 | 10 |
| Fono refused for the new reason | — | 4 |
| calls carrying a blank field | 16 | **3** |

The named-target rule is visible in every climate turn: the model writes
`{"area": "Master bedroom", "floor": "__all__"}` and the house receives
`{"name": "Air conditioner"}`. The empty set-temperature call is gone from the
server's side of the ledger — it is now refused here, with a sentence naming
the tool that switches things, and both languages then reach the right tool.

Latency did not move, which is the point: none of the three costs a round trip.

### What the run left behind

- **`__all__` has moved house rather than left it.** A blank field is no longer
  writable, so the model writes `"floor": "__all__"` instead — 8 calls before,
  25 after. Every one is stripped before the call travels, so it costs tokens
  and nothing else, but the cause is plain: `floor` is the one target field
  whose permitted values this server never published, so it is the one field
  with nothing holding it. Either it is constrained by a published list of
  floors, or it is not offered at all; there is no third option that stops the
  model filling it with something it invented.
- **A blind that is already open cannot be told so.** `HassTurnOn` on
  `Curtain Left` was accepted, the readback said `open`, and Fono reported the
  command as not having reached the state asked for. The judge knows `on` and
  `off`; it does not know that `open` is what `HassTurnOn` produces on a cover.
  The model then sent `HassTurnOff` at the same blind and claimed success.
- **The air conditioner still cannot be switched off.** It sits in `cool`;
  `HassTurnOff` is accepted and the mode does not change. This is the house.
- **A device the home has not got is still acted on in Romanian** — the pizza
  oven became the Kitchen thermostat. English refuses it in 0.6 s.

## 10. What the house can actually do, read from Home Assistant directly

Read from the REST API rather than from `GetLiveContext`, because the intent
catalogue publishes **state** and the capability lives in attributes the
catalogue never forwards. Everything below is a fact about this house that the
suite could not have learned from the tools it uses.

### The value tools are wider than the suite

| tool | value field | bounds published | exercised |
|---|---|---|---|
| `HassLightSet` | `brightness` 0–100, `color`, `temperature` | yes | brightness only, and both cases skip |
| `HassSetVolume` | `volume_level` 0–100 | yes | yes |
| `HassSetPosition` | `position` 0–100 | yes | **never** |
| `HassSetVolumeRelative` | `volume_step` −100…100 or up/down | yes | yes |
| `HassClimateSetTemperature` | `temperature` | **none** | yes |
| `HassStopMoving` | — | — | **never** |

`HassClimateSetTemperature` is the **only** value tool in the catalogue with no
`minimum` and no `maximum`, and it is the tool that produced the reported
defect. The device declares `min_temp: 7, max_temp: 35`; the intent schema
forwards neither. So a check that enforced the published bounds would have
caught an out-of-range `brightness`, `position` or `volume_level` and would
**not** have caught `temperature: 0`. That is a concrete upstream contribution:
publish the device's own bounds on the climate intent, and mark the value
required.

### Covers are the reliably adjustable devices, and nothing tests them

| cover | state | position | can |
|---|---|---|---|
| Curtain Left | open | 100 | open, close, **set position**, stop |
| Guest bedroom roller | open | 100 | open, close, **set position** |
| Kids bedroom Roller | closed | 0 | open, close, **set position** |
| Master bedroom roller | closed | 0 | open, close, **set position** |
| Gate | closed | 0 | open, close, set position, stop |

Every reachable cover reports `current_position` **whatever state it is in**.
That makes a cover the one device kind whose level a fixture can rely on
without first switching anything on — which is exactly what the two skipped
light cases need and cannot get.

### A dimmable light is invisible while it is off

Of the lights this house exposes, about fifteen can be dimmed and the rest are
`onoff` only. A light reports `brightness` **only while it is on**, so
`adjustable_device{light}` can see one candidate: `Couch`, at full brightness.

`Couch` is two entities with the same name, so it is not addressable, so it is
not targetable, so both `plain_switch_on_invents_no_brightness` and
`dim_uses_the_brightness_tool` skip. Two guards against the reported defect are
absent, and the reason is a name collision in one house.

`Living square` is the useful device here and the suite cannot ask for it by
capability: unique name, `rgbw`, so both a brightness and a colour are
settable. Its five siblings (`Living square (1)`, `red`, `green`, `blue`,
`white`) are brightness-only channels of the same fitting.

### What follows

- **A cover position case earns its place twice**: it covers an enabled tool
  nothing has ever called, and it is the only value case that cannot skip.
- **`adjustable_device` must be allowed to find out.** Resolve to a targetable
  device of the domain, stage it on, read the house again, and if it still
  reports no level, drop it and re-resolve. `House::without` already exists for
  exactly this shape — a device the run learns the hard way is unusable.
- **`open`/`closed` is not `on`/`off`.** `desired_state` maps `HassTurnOn` to
  the literal `on` and compares the readback for string equality, so opening a
  blind is reported to the model as a failed call. The model then closes it and
  says it did. This is the clearest wrong action in the suite and it is ours.

## 11. The upstream defect, located in three lines

`homeassistant/components/mcp_server/server.py:41-53` builds each tool
descriptor like this:

```python
input_schema = convert(tool.parameters, custom_serializer=custom_serializer)
return types.Tool(
    name=tool.name,
    description=tool.description or "",
    inputSchema={
        "type": "object",
        "properties": input_schema["properties"],
    },
)
```

`voluptuous_openapi.convert` returns `{"type", "properties", "required"}`.
The MCP server copies two of the three keys and throws `required` away.

The intent handlers themselves are correct.
`SetTemperatureIntent.slot_schema` declares `vol.Required("temperature")`
(`homeassistant/components/climate/intent.py:31`), and a local run of
`convert` over that schema emits `"required": ["temperature"]`. So the
obligation exists, `convert` publishes it, and the transport discards it.
This is why all 26 tools in Fono's catalogue report no required field.

The consequence for any MCP client that constrains a model with the
published schema: `HassClimateSetTemperature` appears callable with no
temperature. A model asked to switch an air conditioner off reaches for
the most specific climate tool, invents `temperature: 0` to satisfy a
field it cannot tell is optional, and the house is asked to set zero
degrees. When the invented value is refused the model writes the same
tool with the field absent, which the schema also permits, and the
server rejects the call.

Secondary, weaker, and reported as a note rather than a defect:
`vol.Coerce(float)` carries no range, so the climate intent publishes no
`minimum` and no `maximum` while the device declares `min_temp: 7,
max_temp: 35`. Every other value intent does publish bounds
(`brightness` 0-100, `position` 0-100, `volume_level` 0-100). A client
that enforces published bounds therefore catches a bad brightness and
not a bad temperature.
