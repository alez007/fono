# Resume point — voice actions work and the two Home Assistant PRs

Written at the end of a session that ran out of budget mid-task. Everything
below is state, not intention: what is committed, what is in the working tree,
what is posted upstream, and the exact next edit in each of the two open
threads.

## 1. Fono repo state

Committed up to `abdb48a` ("Act on the device you named, and stop asking the
home to set nothing"), which landed the two rows this table used to list as
uncommitted: `crates/fono/src/actions/mod.rs` (readback summary, the spoken
device name as target, a value tool that cannot be written without its value,
`__all__` only in the kind slot) and `crates/fono-core/src/tool_grammar.rs`
(`str` in the rails is non-empty, so a blank field cannot be written).

**Uncommitted in the working tree — mine, gate green:**

| file | change |
|---|---|
| `crates/fono/src/actions/vendor.rs` | `open`/`closed` is not `on`/`off` in `desired_state` — the cover/mode bug |
| `crates/fono/src/assistant.rs` | `RecordedCall.sent`; the recogniser `context_hint` experiment reverted to `None` |
| `crates/fono/src/bench_actions/*` | unreachable devices are never targets and skip with a reason; `adjustable_device` probes (stage on, re-read, drop and re-resolve) instead of guessing; covers stage and restore; both copies of every call in `detail.json` |
| `tests/fixtures/bench_actions/home_assistant.toml` | suite v3, 16 cases — added `open_a_cover`, `set_a_cover_position`, `an_unknown_device_is_refused`, `an_area_of_several_lights_all_arrive`, `set_a_temperature_uses_the_climate_tool`; `expect_level` on the dim case |
| `plans/2026-08-02-voice-actions-fast-and-right-v1.md` | findings sections 8, 9, 10 |

**Gate at the point of stopping:** `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace --tests --lib` — all pass, 37 suites.

**Other people's changes are also in the tree** (`AGENTS.md`,
`CONTRIBUTING.md`, `Cargo.toml`, `crates/fono-stt/*`, `tests/check.sh`,
`crates/fono/tests/live_pipeline.rs`, and a second plan file). Check
`git status` before staging anything.

**Nothing is pushed.** The size-budget gate has not run.

### Verified effects of the uncommitted work

- `open_a_cover` passes first try in both languages. Before the mode fix Fono
  told the model a correct call had failed, and the model closed the blind it
  had just opened and said so.
- Both dimming cases now run instead of skipping; `dim_uses_the_brightness_tool`
  passes.
- `HassSetPosition` is called for the first time.
- The named-device substitution is gone: 9 of 19 turns used to aim an area at a
  device the user named by name.

### Not built, still on the list

1. **Withhold value tools from the grammar when the request has no digit.** The
   last measurement: 36 % of all calls are a value tool chosen for a plain
   switch request. This is the only proposal that removes the retry instead of
   shortening it. The escape hatch is proven — 10 refusals in one run, all
   rewritten and sent — and only 2 utterances in 26 spell a number in words.
2. **Three repeats per cell.** Two runs of identical code gave 0.77 and 0.85
   final rate: 28 of 30 cases agreed and 2 flipped. Until repeats land, no
   accuracy claim is worth more than ±8 points.
3. A commit for everything in section 1.

## 2. Upstream, Home Assistant

Two issues are **posted**, and a third is proposed in §5:

- **#178032** — MCP server drops `required` from every intent tool schema.
- **#178033** — light and cover intents match entities of other domains and
  report success.

Working clone: `../hass-tmp/core`, `dev` at `cb9e85aa` (2026-08-02), cloned
`--depth 1 --filter=blob:none`. The house it was reproduced against runs
2026.7.4; the two code paths are the same.

**Before any PR:** the clone is shallow and has no fork remote. Run
`git fetch --unshallow`, add your fork, and branch off `dev`. Home Assistant
wants one PR per component, so these are two PRs to two sets of code owners.

### PR 1 — `mcp_server` passes the converted schema whole

**The edit is already applied** in
`../hass-tmp/core/homeassistant/components/mcp_server/server.py`. It was three
lines: `convert()` returns `type`, `properties` **and** `required`, and the old
code built a fresh dict copying only `properties`. Confirmed with the real
`voluptuous_openapi` in `/tmp/vo` that all three keys are always present, so
passing the result whole is a strict superset. Every other integration
(`anthropic`, `ollama`, `openai_conversation`,
`google_generative_ai_conversation`) already passes it whole.

**Exactly where the session stopped:** adding the test assertion.
`tests/components/mcp_server/test_http.py:384-394` picks `HassTurnOn` out of
`session.list_tools()` and asserts `type` and one property. `HassTurnOn` has no
required slot, so it cannot carry the regression. The next edit is to assert
`required` on a tool that has one — `HassSetPosition` (`position`) or
`HassClimateSetTemperature` (`temperature`) — which first needs a check of
which intents `setup_integration` in
`tests/components/mcp_server/conftest.py` actually registers. If neither
intent is registered there, either extend the fixture or assert
`"required" in tool.inputSchema` on a tool that is.

Reproduction to quote in the PR, taken from a bare REST call with no client in
the path:

```
HassClimateSetTemperature (no temperature) -> HTTP 500
HassSetPosition           (no position)    -> HTTP 500
```

### PR 2 — `required_domains` on the light and cover intents

**Not started.** The change is `required_domains={DOMAIN}` on the handlers in
`homeassistant/components/light/intent.py` and
`homeassistant/components/cover/intent.py`. `fan`, `lawn_mower`,
`media_player` and `vacuum` already set it; light and cover set `platforms`
instead, which filters on the *assistant sending the intent*, not on the domain
of the matched entity. So `light.turn_on` is called with a `cover.` entity id,
does nothing, raises nothing, and `async_handle` files the entity under
`success_results`.

Reproduced twice, bare REST, nothing moved and success was reported:

```
HassLightSet {"name":"Curtain Left","brightness":50}
  before: cover.curtain_left open, position 40
  reply : response_type=action_done success=['cover.curtain_left'] failed=[]
  after : cover.curtain_left open, position 40

HassOpenCover {"name":"Living square"}
  before: light.living_square off
  reply : response_type=action_done success=['light.living_square'] failed=[]
  after : light.living_square off
```

Second, client-visible effect worth keeping in the PR text: `required_domains`
also narrows the published `domain` slot, so `HassSetVolume` publishes
`enum: ["media_player"]` while `HassLightSet` publishes a bare string. A model
reading the second writes `"domain": ["light"]` for something that is not a
light; a model reading the first cannot.

Open question for the maintainers, already flagged in the issue: a
client-supplied `domain` slot **replaces** `required_domains` rather than
intersecting it, so `HassLightSet {"domain":["cover"]}` still reaches a cover
after the fix. Do not silently change that behaviour in the same PR.

Tests to add: `tests/components/light/test_intent.py` and
`tests/components/cover/test_intent.py` — a light intent aimed at a cover must
not report success.

### PR 3 — the entity dump never says which floor anything is on

**Not started, and the smallest of the three.** Every intent tool publishes a
`floor` argument. Nothing any MCP client can read says which floors exist: the
word `floor` appears **zero** times in `GetLiveContext` and zero times in the
`Assist` prompt (measured — see §5). So `floor` is an argument the API offers
and never grounds, and a model fills it with something it invented.

The change belongs in the exposed-entity dump in `homeassistant/helpers/llm.py`,
beside the `areas:` line that is already there — one field per entity, public
registry data, and it reaches every LLM client rather than only MCP. The
argument to make in the issue is the one sentence above; the evidence is that
Fono has to withhold the argument entirely to stop the invented values, which
is a worse outcome for the user than grounding it.

Until it lands, Fono withholds `floor` (§5, step 2). That is the right
behaviour whatever upstream decides, so this PR is not a blocker for anything.

## 3. Facts about the test house worth not re-deriving

- 2,308 entities; 59 lights, 6 covers. 14 unreachable is this house's normal
  baseline (one run hit 68 while a hub was down, and its numbers are void).
- Every reachable cover reports `current_position` **whatever state it is in**,
  which makes a cover the only device kind a value fixture can rely on without
  staging anything first.
- A light reports `brightness` only while it is on. `Couch` is two entities
  sharing one name, so it is not addressable — that is why the two dimming
  cases used to skip.
- `Living square` is unique and `rgbw`: brightness and colour are both
  settable. Its five siblings are brightness-only channels of the same fitting.
- `HassClimateSetTemperature` is the only value tool in the catalogue with no
  `minimum` and no `maximum`. The device declares `min_temp: 7, max_temp: 35`
  and the intent forwards neither, which is why enforcing published bounds
  would not have caught the `temperature: 0` that started all of this.

## 4. Latency, for context

Median turn 28 s → 8.8 s across the session, slowest tenth 58 s → 14.4 s. The
remaining cost is one number: 2.4 generations per turn. One generation is
2.3 s; two or more is 9.5 s. Every remaining latency question is "why did this
turn need a second attempt", and the two mechanical causes are the ones item 1
of section 1.3 addresses.

## 5. Probed the house over MCP — what the protocol already carries

Read-only probe of the live house on 2026-08-03: `initialize`, `prompts/list`,
`prompts/get Assist` (11,989 chars), `resources/list`, `tools/call
GetLiveContext` (19,040 chars). Server `home-assistant 1.26.0`, MCP
`2024-11-05`. It changes what needs a PR and what does not.

### The room of every device is in the dump, and Fono discards it

```text
- names: Air conditioner
  domain: climate
  state: 'off'
  areas: Hallway
  attributes:
    current_temperature: '29'
    temperature: '23'
```

`areas:` is present on **138 of 156** entities. `parse_devices`
(`crates/fono-assistant/src/mcp_client.rs:306-327`) reads `- names:` and
`domain:` and drops the rest; the area line feeds only the flat list of area
names (`:242-257`). So "the catalogue records what a device is and not where" —
the reason `HouseFacts::agree` drops a wrong area instead of correcting it
(`crates/fono/src/actions/mod.rs:1217-1226`) — was never a limit of the
protocol. It is a parser that throws the field away.

What reading it unlocks, with no upstream change and no new permission:
devices grouped by room in the prompt; a wrong area beside a named device
corrected rather than dropped; and an explanation for the "office AC" case —
the dump says `Air conditioner … areas: Hallway`, so no area-plus-kind call
could ever have reached it and naming the device is the only route.

Watch the alias: areas arrive as `areas: Kitchen, bucătărie`. `parse_places`
already splits on the comma; a device-to-room reader must too, or the store
gains a room called *"Kitchen, bucătărie"*.

### `device_class` is in the dump too, for the kinds where it matters

| domain | classes present | entities carrying one |
|---|---|---|
| `cover` | `curtain`, `gate`, `shutter` | 6 of 6 |
| `media_player` | `speaker`, `tv` | 3 of 7 |
| `switch` | `switch` | 1 of 20 |

So *"open the curtains"* and *"open the gate"* can be told apart, and a wrong
class can be corrected exactly as a wrong `domain` already is — again with no
upstream change. Earlier notes assumed this needed the REST API; it does not.

### Floors are the one thing genuinely missing

Zero occurrences in either payload. This is PR 3.

### Two lines of Home Assistant's own `Assist` prompt are worth keeping

Not the prompt itself — 12k chars, it repeats the device list without state and
includes all 57 sensors, and Fono's own hint is better targeted. But:

- *"Use HassTurnOn to lock and HassTurnOff to unlock a lock."* This answers the
  open question about the two locks in this house: the intent exists, no docs
  caveat is needed.
- *"When controlling a device, prefer passing just name and domain. When
  controlling an area, prefer passing just area name and domain."* Home
  Assistant states Fono's own rules 2 and 4. No reason to pay for them twice.

### Also confirmed, for the fixture

`cover.current_position` on 5 of 6 covers, `light.brightness` on 13 lights,
`climate.current_temperature` and `temperature` on all 5.

### Order of work this implies

1. **Read the room and the device class** in `parse_devices` and store them.
   Biggest gain, no upstream work, no new permission.
2. **Withhold a target field with no published values** — today only `floor`.
   Scope the rule to the fields named in `vendor::SlotFields` and nothing else:
   free text (`item`, `message`, `search_query`) and numbers must keep their
   freedom, and a field with a published enum (`device_class`) already has
   values. Blank text is unwritable since `abdb48a`, so the model now writes
   `"floor": "__all__"` instead — 25 calls of 39.
3. **Fix `name` on the list tools before the shopping list is enabled.**
   `HassListAddItem.name` holds the name of a *list*, not a device, and the
   rails narrow `name` to this house's 79 device names for every tool on the
   server (`crates/fono/src/actions/mod.rs:509-513`). The correct call becomes
   unwritable, which is worse than an invented value, and
   `aim_at_what_was_said` would push a device name into it as well.
4. **A word-only pass after a failure**, so a turn cannot end in silence:
   `crates/fono-assistant/src/llama_local.rs:2783-2800` returns the corrective
   pass's prose, which is empty by construction because the pass was forced to
   be a command. The sentence from the first pass is in `promised` (`:2683`)
   and is thrown away.
