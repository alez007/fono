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
