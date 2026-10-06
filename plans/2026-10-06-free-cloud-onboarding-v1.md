# Free cloud providers in one screen: first-run onboarding

## Objective

A user with no computer experience installs Fono, opens it from the app menu, and is
dictating in about 20 seconds. They pick any **free-of-charge** cloud provider, the
private local model, or a key they already have, all from one browser screen and
without reading documentation. Keys that already work are never replaced, and choosing
local stays one click.

No crate new to `Cargo.lock`. PKCE needs `sha2`, `base64` and `getrandom`, the
clipboard needs `arboard`, and the browser launch uses the existing `open_url`
(`crates/fono/src/daemon.rs:2267`). Expected binary growth: the welcome page assets
plus the onboarding module, well under 100 KiB.

## Decisions (agreed in conversation)

- **Fono never creates accounts automatically.** Captchas, email and phone verification,
  and the providers' terms rule it out. Fono uses OAuth where the provider offers it,
  and guided sign-up with automatic key pick-up everywhere else.
- **Show every free-of-charge provider, not a fixed set.** The welcome screen lists
  every catalog provider with an ongoing free tier that can run dictation (speech-to-text)
  on its own. Each card offers the same "Connect" action.
- **Trial-credit providers are not "free".** Providers that give one-off credit, or that
  need a card, go under "Other services", never in the free row.
- **Reuse before create.** A saved key that still works skips onboarding. Keys Fono
  creates are labelled `fono-<hostname>` so they're easy to recognise and revoke.
- **The browser is the primary surface.** The terminal wizard needs a TTY
  (`crates/fono/src/cli.rs:829-843`), so a desktop launch never shows it today and
  silently writes a default local config. The terminal wizard stays, with the same
  logic in the same order.

## Check results (2026-10-06)

Checked against each provider's pricing / rate-limit page and the live OpenRouter
model list on 2026-10-06.

| Provider | Free of charge? | Speech-to-text free? | Cleanup free? | Row |
|---|---|---|---|---|
| Groq | Ongoing free plan, no card | Yes: `whisper-large-v3-turbo` 20 RPM, 2,000 requests/day, 28,800 audio-seconds/day | Yes: `gpt-oss-120b` 1,000 requests/day | **Free** |
| Gemini | Ongoing free tier ("Free of charge" input and output) | Yes, on the Flash-Lite default | Yes | **Free** |
| OpenRouter | Free `:free` models only (3 accept audio: `thinkingmachines/inkling-small:free`, `thinkingmachines/inkling:free`, `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free`) | **No with Fono's defaults**: Fono's OpenRouter STT uses `/audio/transcriptions` with paid Whisper, and the polish default is paid | No with defaults | Other (keeps one-click OAuth) |
| Cerebras | Free tier | No STT | Yes | Other (free plan, cleanup only) |
| ElevenLabs | Ongoing free plan (monthly credits) | Uses credits; no polish | No polish | Other (free plan) |
| Cartesia | Ongoing free plan (small monthly allowance) | Small; no polish | No polish | Other (free plan) |
| Speechmatics | $100 one-off credit, no card | Trial | No polish | Other (trial) |
| Deepgram | $200 one-off credit, no card | Trial | No polish | Other (trial) |
| AssemblyAI | One-off free hours, no card | Trial | No polish | Other (trial) |
| OpenAI, Anthropic | Paid | n/a | n/a | Other |

Consequences:

- The free row is **Groq and Gemini**: the only ongoing free tiers that run full
  dictation (speech-to-text **and** cleanup) on Fono's default models. The row is
  generated from catalog data, so a provider that qualifies later appears without UI work.
- OpenRouter's one-click OAuth is still built (B3), but OpenRouter sits under "Other
  services". Routing Fono's OpenRouter STT through a free audio chat model is a separate
  feature and out of scope here.
- Free plans that can't do cleanup (ElevenLabs, Cartesia, Cerebras) carry a "free plan"
  tag under "Other services".

## Target UX

```
              How should Fono understand your voice?

  Free cloud: pick any, each one works fully       Private
 ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───┐   ┌───────────────┐
 │ Groq      │ │ Gemini    │ │ OpenRouter│ │ … │   │ This computer │
 │ Fastest   │ │ Google    │ │ One click │ │   │   │ Offline 1.5 GB│
 │ [Connect] │ │ [Connect] │ │ [Connect] │ │   │   │ [Use local]   │
 └───────────┘ └───────────┘ └───────────┘ └───┘   └───────────────┘
   Other services ▾   (paid or trial credit)

   Already have an API key? Paste it anywhere on this page (Ctrl+V)
```

1. **Step 0 (invisible):** test the saved keys. If one works, skip straight to "Connected ✓ ·
   Change". Offer a key found only in an environment variable as one button.
2. **One screen:** free cloud cards, the local card, and paste-anywhere. A
   "Recommended" badge follows hardware and language; it never hides another card.
3. **Connect:**
   - OAuth providers: sign in, Authorize, and the callback finishes the job.
   - Other providers: Fono opens the key page and shows a 3-step checklist with a live
     status. It picks the key up from the clipboard, from Ctrl+V on the page, or from the
     paste field, then tests it.
4. **Try it:** "Hold the dictation hotkey and say something". The text appears on the
   page, and onboarding ends there.
5. After one provider connects, the other cards read "Add as backup" (optional, never asked).

## Implementation Plan

### A. Catalog data (`fono-core`)

- [x] A1. Add onboarding metadata to `CloudProvider`
      (`crates/fono-core/src/provider_catalog.rs`):
      `free_tier: FreeTier` (`None` / `Ongoing` / `TrialCredit`),
      `key_prefixes: &'static [&'static str]`,
      `onboarding: Onboarding` (`OAuthPkce { auth_url, exchange_url }` / `KeyPage`),
      and `tagline: &'static str` (the one plain-language strength shown on the card).
      Keeping this in the catalog means a change to a provider's free tier is a one-line edit.
- [x] A2. **Verify each provider's free tier against its pricing page, on the day of
      implementation**, and record the check date in a comment next to `free_tier`.
      Starting assumptions to confirm or correct:
      - Groq: ongoing free tier, no card.
      - Gemini: ongoing free tier, no card.
      - OpenRouter: free `:free` models with a daily cap. **Check whether any free model
        accepts audio.** If none does, OpenRouter can't run dictation on its own: it moves
        out of the main row and is offered for polish and the assistant only.
      - Cerebras: free tier, but no speech-to-text, so it isn't in the main row.
      - ElevenLabs, Speechmatics, Cartesia: free plans with monthly allowances. Confirm
        whether the allowance covers speech-to-text and whether a card is required.
      - Deepgram, AssemblyAI: trial credit, so "Other services".
      - OpenAI, Anthropic: paid, so "Other services".
- [x] A3. New module `crates/fono-core/src/onboarding.rs`, shared by the browser page
      and the terminal wizard:
      - `free_dictation_providers()`: `free_tier == Ongoing` and the provider's
        speech-to-text is wired. Ordered by the recommendation rule, with ties broken by
        catalog order.
      - `detect_provider(key) -> Option<&CloudProvider>`: matches on `key_prefixes`.
        Longest prefix wins, so `sk-or-` and `sk-ant-` beat `sk-`. A bare `sk-` maps to
        OpenAI only when no longer prefix matches.
      - `recommend(snapshot, languages)`: returns the card to highlight and nothing else.
- [x] A4. Applying a provider configures every capability it covers in one call, by
      reusing `apply_primary_provider` (`crates/fono/src/wizard.rs:388`). Move it into
      `fono-core` if the daemon needs it. No per-capability questions.

### B. Daemon and HTTP API (`fono-net` web settings and `fono` daemon)

- [x] B1. First-run trigger (`crates/fono/src/daemon.rs:70-73`, `crates/fono/src/cli.rs:829`):
      with no config on disk, no TTY and a graphical session, start the web settings
      listener on **loopback only**, then call `open_url(<settings>/#welcome)`. Reuse
      the lazy-start path in `open_settings_web_via_tray`
      (`crates/fono/src/daemon.rs:4034-4076`). Headless (no display): keep today's
      behaviour.
      Also add a tray item "Set up cloud…" that opens the same page.
- [x] B2. Token-gated endpoints (same auth as existing `/api/*`):
      - `GET  /api/onboarding/state`: saved keys with live test results (via
        `key_probe`), env-only keys, the free provider list with taglines, the recommended
        card, and local model size and download progress.
      - `POST /api/onboarding/key {value}`: `detect_provider` → test → save → apply.
        The response never echoes the key.
      - `POST /api/onboarding/connect/{provider}`: returns the URL to open. For OAuth
        providers it creates a PKCE session (verifier and `state`, 10-minute life,
        single use).
      - `POST /api/onboarding/watch` / `DELETE …`: start or stop the clipboard watcher.
      - `GET  /api/onboarding/events`: polling (or SSE if the server already has it)
        for "key received / tested / failed".
- [x] B3. OpenRouter OAuth PKCE (`https://openrouter.ai/auth`, exchange via
      `POST https://openrouter.ai/api/v1/auth/keys`):
      - `code_challenge_method=S256`, a random `state`, and
        `key_label=fono-<hostname>`. Localhost callbacks are accepted on any port.
      - `GET /oauth/openrouter/callback` is not token-gated, because the browser arrives
        there from OpenRouter. It rejects an unknown or reused `state`, exchanges the code,
        saves the key, and shows "Done, you can close this tab".
      - Hostname comes from `/etc/hostname`, `COMPUTERNAME` or `scutil`, with no new crate.
      - Optional: OpenRouter labels a localhost app `localhost:<port>` on the consent screen.
        A static redirect page on the project site would show "Fono" instead. Only worth
        doing if the consent screen confuses testers.
- [x] B4. Clipboard watcher: poll through the existing `arboard` handle
      (`crates/fono-inject/src/inject.rs:363`) every 500 ms.
      - Runs only while a connect checklist is open, and stops after 2 minutes.
      - Matches only the expected provider's key prefix, and only the whole trimmed
        clipboard text. It never logs or stores anything that doesn't match.
      - On Wayland, background reads may fail. Ctrl+V on the page is the fallback, and
        the checklist doesn't depend on the watcher.

### C. Welcome page (web settings assets)

- [x] C1. A `#/welcome` view built from `/api/onboarding/state`. The card row is generated
      from the catalog, so the frontend has no hardcoded provider list.
- [x] C2. A document-level `paste` listener (no clipboard permission needed). It posts to
      `/api/onboarding/key` and shows "Groq ✓" in place, or a plain error ("This
      doesn't look like an API key" / "Groq rejected this key").
- [x] C3. Connect checklist for providers without OAuth: three steps with a live status
      ("waiting for the key…" → "tested ✓"), a "Reopen page" link, and a paste field.
- [x] C4. Local card: start the download, then show progress. Reuse what the wizard
      triggers with `spawn_model_downloads` (`crates/fono/src/wizard.rs:1116`).
- [x] C5. "Try it" step: show the dictation hotkey, and display the next dictation on the
      page. A small "Skip" link closes onboarding.
- [x] C6. (Done as the header cloud button and the tray item "Connect a service…",
      both opening the same welcome view.) Settings gets an "Accounts" section built from the same component, so there is
      one interface for later changes, backup providers and revoking keys.
- [x] C7. Plain-language copy only: no model names, env-var names or request limits on
      cards. Details go behind an "ⓘ".

### D. Terminal wizard parity

- [x] D1. (Terminal: the key is pasted into the prompt; no clipboard watcher or
      OAuth there, since OpenRouter is not in the free row.) Restructure `pick_path` / `configure_cloud` (`crates/fono/src/wizard.rs:963`,
      `:1202`) into the same order:
      1. Reuse working saved keys. `prompt_or_reuse_key` at `:498` already skips when a
         key exists; also run the live test.
      2. Prompt: paste a key, and detect the provider automatically.
      3. List the free providers and connect one (print the URL, run the clipboard
         watcher, OAuth through a localhost callback, or OpenRouter headless mode
         over SSH).
      4. Local.
      5. Other services.
- [x] D2. The wizard keeps working without a browser.

### E. Tests

- [x] E1. `detect_provider`: one fixture per prefix, longest-prefix precedence, rejects
      whitespace/garbage, never matches a partial string.
- [x] E2. PKCE: the S256 test vector from RFC 7636 Appendix B; `state` is single use;
      a session expires after 10 minutes.
- [x] E3. Catalog consistency: every `Ongoing` provider has `key_prefixes`, a `console_url`
      or OAuth URLs, a non-empty `tagline`, and wired speech-to-text when it is in the
      main row.
- [x] E4. Endpoint tests: the key endpoint never echoes the value; the callback rejects
      a bad `state`; an existing working key short-circuits `state` to "connected".
- [x] E5. Extend `crates/fono/tests/wizard_primary_flow.rs` for the new order.

### F. Docs and records

- [x] F1. Add an ADR on the onboarding policy: no automated account creation, OAuth
      where offered, free tier means no card and no one-off credit, keys labelled
      `fono-<hostname>`, and clipboard watching limited in scope and time.
- [x] F2. Update `docs/providers.md` with the free-tier table and its check date.
- [ ] F3. (status.md entry done; CHANGELOG and ROADMAP at release.) Add an entry to `docs/status.md`; add a CHANGELOG and ROADMAP entry at release.

## Verification Criteria

- From an empty config, launching from the app menu (no TTY) opens the welcome page.
  Connecting a free provider that has OAuth takes ≤ 20 seconds; a provider without
  OAuth takes ≤ 30 seconds (stopwatch, signed-in Google account).
- Reinstalling with a working key in `secrets.toml` shows "Connected ✓" and creates
  no new key.
- Pasting an existing key of each supported prefix configures the right provider with
  no other input.
- Choosing local never requires a key and never deletes an existing one.
- Pre-commit gate passes: `cargo fmt --check`, clippy `-D warnings`, and
  `cargo test --workspace --tests --lib`. The size-budget gate passes.
- `cargo tree` shows no crate new to the lock file.

## Risks and Mitigations

- **Free tiers change without notice.** They're kept as catalog data with a check
  date, the card copy never quotes limits, and the "Add as backup" fallback covers a
  provider that disappears.
- **No free OpenRouter model with audio input.** OpenRouter moves to polish/assistant
  only, and the main row loses its only one-click option. The other providers still
  connect in ≤ 30 seconds.
- **Wayland clipboard.** The watcher is best-effort; page-level Ctrl+V always works.
- **Clipboard privacy.** The watcher runs only during an open checklist, stops after
  2 minutes, matches one prefix, and logs nothing. The ADR states this.
- **Callback endpoint exposure.** It is loopback-only and requires a single-use `state`
  that expires after 10 minutes. A forged callback without the verifier can't exchange
  a code.
- **The first-run browser open surprises someone launching from a terminal.** Only
  trigger it when there's no TTY. With a TTY the terminal wizard runs as today.
