# ADR 0017 — Cloud STT language stickiness (in-memory rerun-target cache)

Date: 2026-04-28
Status: Accepted — amended twice; OpenAI now receives no language field
at all (see the second amendment at the bottom)
Supersedes: relevant portions of [ADR 0016](0016-language-allow-list.md)

## Context

ADR 0016 established the multi-language allow-list and the
`LanguageSelection { Auto, Forced, AllowList }` enum. With the
`AllowList` mode in production, two failure modes surfaced:

1. **Cloud Turbo misdetection.** Groq's `whisper-large-v3-turbo`
   (and to a lesser extent OpenAI's `whisper-1`) occasionally classify
   accented English as Russian, Bulgarian, or other Slavic languages
   for non-native English speakers. The transcript is then rejected by
   the allow-list filter and falls through to garbage.
2. **No symmetric solution for switchers.** Users who genuinely
   alternate between two or three languages (English at work, Romanian
   at home) cannot use a "forced primary" knob — every other-language
   utterance breaks. The allow-list lets the provider auto-detect
   freely; symptom 1 is the cost of that freedom.

We need a defence against symptom 1 that does not break symptom 2.

## Decision

Add an in-memory per-backend cache of the most recently
correctly-detected language code. The cache is consulted **only as a
rerun target** when the provider returns an out-of-allow-list
language; never as a first-call hint.

### Rules

1. First call: never force `language=`. Let the provider's auto-detect
   handle language switching for free.
2. On in-allow-list detection: record the code in the cache.
3. On banned (out-of-allow-list) detection:
   - If the cache holds a code for this backend → re-issue the same
     audio once with `language=<cached>`; return the rerun's response.
   - Otherwise → accept the unforced response as-is. No rerun.
4. Cache is keyed by backend `name()` (`&'static str`); one
   `Arc<LanguageCache>` is shared process-wide via
   `LanguageCache::global()` so batch and streaming variants of the
   same provider see the same cache.
5. OS locale is used to seed the cache at daemon start **if and only
   if** the locale's alpha-2 is in the configured allow-list.

## Rejected alternatives

### Local-Whisper "language bridge" before every cloud call

Run local Whisper's `lang_detect` on the prefix audio and force
`language=<detected>` on the cloud call. **Rejected.**

- Cloud users typically chose cloud precisely because they can't run
  local inference at acceptable latency. The bridge contradicts the
  whole reason they're on cloud.
- Adds a `whisper-rs` link dependency to the slim cloud-only build,
  defeating `cloud-all` as a lightweight option.
- The first-call detection is still correct in the common case
  (~95%); paying the bridge cost on every utterance is wasteful.

### File-persisted cache (`~/.cache/fono/state/lang_cache.json`)

**Rejected.**

- Cold-start hit-rate is marginal: the cache is helpful only when the
  user happens to open the same language they last spoke in.
- When the cached value is stale across sessions (different topic,
  different language), it actively misleads the rerun and produces
  worse output than today's behaviour.
- Adds corrupt-file recovery, race-on-write, serde plumbing, and
  `state_dir` propagation for negligible benefit.
- Daemon restarts are infrequent; in-memory rebuild within one or two
  utterances is cheap.

### Cache-as-first-call-force

Send `language=<cached>` on every request. **Rejected — actively
harmful for switchers.**

Trace with `languages = ["ro", "en"]`, cache `ro`, user switches to
English:

| Request | Cache | `language=` | Provider | Output |
|---|---|---|---|---|
| #1 (ro) | ro | ro | forced ro | ✓ correct |
| #2 (en) | ro | **ro** | forced ro on English audio | **garbled Romanian-as-English** |
| #3 (en) | ro | ro | same garbled decode | ✗ |

Once stickiness pins the wrong language for a switcher, every
subsequent call is broken until the cache is manually cleared. That's
worse than the bug we set out to fix.

The rerun-target design avoids this entirely: the first call is
always unforced, so the provider's auto-detect handles ro↔en switching
for zero cost. The cache only matters when auto-detect actually
misfires.

### Primary/secondary language model

Designate one entry of `general.languages` as primary and force it on
ambiguous calls. **Rejected.**

- The user-visible mental model "you have one main language plus some
  fallbacks" doesn't match how bilingual / multilingual users actually
  work. The right peer for any given utterance is whichever the user
  just spoke; config-file order is unhelpful as a tiebreaker.
- The implementation requires "primary" to leak into multiple call
  sites (first-call language, rerun fallback when cache empty, wizard
  copy, tray submenu). Each leak becomes a switcher-breaking bug.
- The peer-symmetric model (cache reflects what was last heard) needs
  no order anywhere, which is testable: two configs `["ro", "en"]`
  and `["en", "ro"]` must produce byte-identical transcripts on the
  same audio.

`LanguageSelection::primary()` is renamed to `fallback_hint()` and
its doc-comment scope-restricts use to single-language transports
(streaming WebSockets that physically can't accept a peer set on
connection setup). All other call sites consult the cache instead.

## Consequences

- The `cloud_rerun_on_language_mismatch` knob default flips from
  `false` to `true`. Cost-sensitive users can opt out.
- `cloud_force_primary_language` is deprecated; superseded.
- The wizard collects a checkbox set with no "primary" picker.
- A new tray "Languages" submenu offers a read-only peer display plus
  "Clear language memory" for the rare case where the cache has gone
  stale across topic changes.
- One-off Turbo misdetections self-heal after the first
  correctly-detected utterance per session (or immediately on
  cold-start when the OS locale ∈ allow-list).

## Amendment — 2026-07-29

This ADR solved the *out-of-allow-list* misdetection. It never covered
confusion **between** two configured languages, and by design it could
not: an in-allow-list detection is recorded as a success and returned
verbatim, so a Romanian/English speaker whose English is tagged `ro` gets
no rerun and no arbitration. That was the live bug reported this session.

Two changes, neither of which invalidates the rules above for the
providers they still apply to:

1. **Where the provider accepts a plural language set, use it.** OpenAI's
   `gpt-transcribe` generation takes `languages[]` and code-switches
   inside a single utterance, so the whole allow-list goes out on the
   first (and only) call. No first-pass guess to defend against, hence no
   rerun lane and no cache consultation on that path. Rules 1–5 continue
   to govern every single-`language` backend (Groq, `whisper-1`,
   `gpt-4o-transcribe`, and the streaming transports).
2. **A language verdict is now optional.** Every backend reports
   `language: None` when its own confidence signal says it is unsure —
   empty `languages[]`, ElevenLabs' `language_probability`, or mean
   `avg_logprob` below `-1.0`. Consumers must treat `None` as "do not
   act on language": the assistant then omits its reply-language
   instruction and uses the default voice rather than committing to a
   guess. Rule 2 gains the corresponding condition — only a `Some`
   verdict is recorded in the cache.

`cloud_rerun_on_language_mismatch` remains as documented, but is now a
legacy knob: it has no effect on plural-`languages[]` models.

## Second amendment — 2026-07-29 (same day): OpenAI gets no language at all

Amendment point 1 above is **withdrawn for OpenAI**. Sending the
allow-list turned out to be the cause of a worse bug than the one it
fixed, and the measurements are unambiguous.

Reported symptom: a Romanian question, correctly transcribed into
Romanian, answered by the assistant in English, with the turn summary
line reading `| en |`.

Probe (45 calls, `gpt-transcribe`, the five Romanian fixtures under
`tests/fixtures/equivalence/`, three repetitions per condition, plus
French / Chinese / English cross-checks):

| condition | reported language |
|---|---|
| `languages[]=en,ro` (what Fono sent) | `en` on 15/15 Romanian clips |
| no language field | `ro` on 15/15 |
| `languages[]=ro,en` | `ro` on 15/15 |

The cross-checks isolate the rule: **whenever `en` is the first
`languages[]` entry the endpoint reports `en`, whatever the audio** —
Chinese audio with `languages[]=en,ro` came back tagged `en`; the same
clip with `languages[]=fr,ro` came back correctly tagged `zh`. The
transcript text was correct in every single call; only the label lied.
The field does not constrain decoding either: sending `languages[]=ro`
on French audio still reported `fr`.

Since a wrong label steers the TTS voice and the assistant's reply
language, and the field buys neither constraint nor accuracy, Fono now
sends **no** `language` and **no** `languages[]` to OpenAI — batch and
realtime, for any allow-list size, including a single configured
language. `general.languages` applies to OpenAI purely as a
post-validation filter on the detection that comes back; an
out-of-list detection reports `None` (rule 2's "do not act on
language"). Rules 1–5 are unchanged for every other backend.
