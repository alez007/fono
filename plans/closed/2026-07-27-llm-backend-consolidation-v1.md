# Consolidate LLM Backend Selection (Local / Cloud / Network)

## Status: Completed (shipped 2026-07-27 — `765e11f` schema + surfaces, `0dc8c51` lint follow-up, `09c5824` `none`-is-a-choice resolver; recorded as ADR 0039)

## Objective

Make the assistant and cleanup ("polish") LLM configuration mean exactly what it
says, in one shape, across every surface: config file, tray, CLI, doctor, wizard
and the web settings page.

Target model — the shape the web UI already uses, promoted to be the *actual*
config schema:

| Selection | Meaning | Engine |
|---|---|---|
| `none` | role disabled | — |
| `local` | embedded llama.cpp running a GGUF on this machine | in-process |
| `cloud` (a named provider) | OpenAI / Anthropic / Gemini / Groq / Cerebras / OpenRouter | HTTPS + API key |
| `network` | **any** OpenAI-compatible HTTP server — Ollama, llama.cpp-server, LM Studio, vLLM, LocalAI, LiteLLM | HTTP + URL |

Engine-specific naming (`ollama`) is removed from the schema; compatibility with
the OpenAI chat-completions wire format is the only contract.

### The three defects being fixed

1. **`ollama` means two different things.** `AssistantBackend::Ollama`
   (`crates/fono-core/src/config.rs:1345-1359`) is the *only* way to reach the
   embedded llama.cpp engine, and also the way to reach a network server. They
   are told apart by a magic marker string in `[assistant.cloud].provider`
   (`"ollama-server"` / `"openai-compatible-local"`) plus an `http(s)://` URL
   smuggled into the secret-name field `api_key_ref` — see
   `crates/fono-assistant/src/factory.rs:506-518` and the fork at
   `crates/fono-assistant/src/factory.rs:599-609`. That is why the user's
   `backend = "ollama"` at `/root/.config/fono/config.toml:72` runs the embedded
   GGUF.
2. **Tray/CLI vocabulary disagrees with the file.**
   `assistant_backend_str(&Ollama)` returns `"local"`
   (`crates/fono-core/src/providers.rs:220`) while serde writes `"ollama"`.
   The tray (`crates/fono/src/daemon.rs:512-515`) therefore says *local* while
   the file says *ollama*. Meanwhile the polish twin maps the same variant to
   `"ollama"` (`crates/fono-core/src/providers.rs:72`) — the two roles use
   contradictory vocabularies for the same word.
3. **Two sources of truth for the cloud provider.** Both `assistant.backend`
   and `[assistant.cloud].provider` carry the provider id, and the web UI writes
   both (`crates/fono-net/src/web_settings/assets/app.js:489-494`). They can
   silently disagree.

Secondary defect fixed for free: the two roles have structurally identical
provider sets but two separate enums that model "local" in opposite ways
(`PolishBackend` has a real `Local` variant plus a distinct `Ollama`;
`AssistantBackend` has neither).

### Target config shape

```toml
[assistant]
enabled  = true
backend  = "network"        # none | local | network | openai | anthropic | gemini | groq | cerebras | openrouter

[assistant.local]           # used when backend = "local"
model        = "gemma-4-e2b"
quantization = "q4_0"
context      = 8192

[assistant.cloud]           # used when backend = a provider id
model        = ""           # empty = catalogue default
api_key_ref  = ""           # empty = canonical env name for that provider

[assistant.network]         # used when backend = "network"
url         = "http://192.168.0.200:11434/v1/chat/completions"
model       = "gemma4:12b"
api_key_ref = ""            # optional bearer for authenticated gateways
```

`[polish]` takes the identical shape. Key decisions:

- **`backend` is the single source of truth** for the selection. `provider` is
  deleted from both cloud sub-tables (it was redundant with `backend`).
- **`api_key_ref` always means "name of a secret", never a URL.** The URL lives
  in `network.url`, typed.
- **`[*.network]` is a typed sub-table**, following the existing
  `[stt.wyoming]` precedent (`crates/fono-core/src/config.rs`, `SttWyoming`).
- **One shared enum** `LlmBackend` replaces `PolishBackend` and
  `AssistantBackend`. Per-role defaults (`Local` for polish, `None` for
  assistant) move into each role's `Default` impl rather than the enum.

### Assumptions

- Per the task, no long-term backward compatibility. A **single-release,
  disposable** load-time migration is still included so an existing
  `backend = "ollama"` does not hard-fail `Config::load` and take the daemon
  down; it is explicitly marked for deletion.
- STT and TTS are **out of scope**. Their `backend` key already holds the full
  selection (`local` / `wyoming` / provider id), which the new LLM shape now
  matches. Their own `cloud.provider` duplication is noted as follow-up, not
  fixed here.
- No new third-party dependencies. Every change is inside crates already in the
  graph, so the size budget is unaffected.

## Implementation Plan

### Phase 1 — Schema

- [ ] Task 1. Introduce a single `LlmBackend` enum in
      `crates/fono-core/src/config.rs` (replacing `PolishBackend` at
      `crates/fono-core/src/config.rs:977-987` and `AssistantBackend` at
      `crates/fono-core/src/config.rs:1345-1359`) with variants `None`, `Local`,
      `Network`, `OpenAI`, `Anthropic`, `Gemini`, `Groq`, `Cerebras`,
      `OpenRouter`, keeping `#[serde(rename_all = "lowercase")]`. Do **not**
      derive `Default`; the two roles need different defaults. Rationale: the
      provider sets are already identical, and one enum makes it impossible for
      the two roles to drift into contradictory vocabularies again.
- [ ] Task 2. Add a deprecated, undocumented `Ollama` variant to `LlmBackend`
      that exists **only** so a legacy file deserializes and reaches
      `migrate()`. Mark it clearly as removable in the next release. Rationale:
      serde runs before migration, so the legacy token must parse or the whole
      config load fails and the daemon refuses to start.
- [ ] Task 3. Add `LlmNetwork { url, model, api_key_ref }` with
      `#[serde(default)]`, and wire it as `network: LlmNetwork` on both `Polish`
      (`crates/fono-core/src/config.rs:923-955`) and `Assistant`
      (`crates/fono-core/src/config.rs:1128-1180`). Rationale: replaces the
      marker-string + URL-in-key-slot hack with a typed table.
- [ ] Task 4. Remove the `provider` field from `PolishCloud` and `AssistantCloud`
      (`crates/fono-core/src/config.rs:1004-1027`,
      `crates/fono-core/src/config.rs:1382-1388`), leaving `{ model,
      api_key_ref }`. Add the missing `#[serde(default)]` to `PolishCloud` while
      there so a partial table stops being a hard parse error (it currently is,
      unlike its assistant twin). Rationale: eliminates the dual source of truth
      for the provider id.
- [ ] Task 5. Set per-role defaults explicitly: `Polish::default().backend =
      LlmBackend::Local`, `Assistant::default().backend = LlmBackend::None`
      (`crates/fono-core/src/config.rs:957-973`,
      `crates/fono-core/src/config.rs:1321-1337`). Rationale: preserves today's
      intent — cleanup works out of the box, chat is an explicit opt-in.
- [ ] Task 6. Bump `CURRENT_VERSION` to `2` (`crates/fono-core/src/config.rs:12`)
      and extend `Config::migrate` (`crates/fono-core/src/config.rs:2109-2153`)
      with a disposable normalisation: for each role, when `backend == Ollama`,
      resolve to `Local` or `Network` and populate `network.url` / `network.model`
      from the legacy fields, then never write `Ollama` again. For the assistant,
      the legacy discriminator is the marker string plus an `http(s)://`
      `api_key_ref`; absent either, it was the embedded path, so map to `Local`.
      For polish, `Ollama` always meant a server, so map to `Network`, taking the
      URL from `polish.cloud.api_key_ref` and falling back to
      `http://localhost:11434/v1/chat/completions`. Rationale: existing users
      keep working through one release without the schema carrying the ambiguity.

### Phase 2 — Provider metadata and vocabulary

- [ ] Task 7. Collapse the duplicated helper pairs in
      `crates/fono-core/src/providers.rs` into one set over `LlmBackend`:
      `llm_backend_str`, `parse_llm_backend`, `llm_key_env`,
      `llm_requires_key`, `all_llm_backends`. Fix the vocabulary so
      `Local => "local"` and `Network => "network"` with no aliasing lie;
      accept `"ollama"` on the *parse* side only, mapped to `Network`, as a
      typing convenience. Rationale: kills the `"local"`-vs-`"ollama"`
      asymmetry at `crates/fono-core/src/providers.rs:220` /
      `crates/fono-core/src/providers.rs:72` that makes the tray disagree with
      the file.
- [ ] Task 8. Replace `configured_polish_backends`
      (`crates/fono-core/src/providers.rs:395-418`) with a role-agnostic
      `configured_llm_backends` whose `Network` gating is
      "`[role.network].url` is non-empty", not "`OLLAMA_HOST` is in
      secrets.toml". Use it for the assistant too, replacing the ad-hoc inline
      filter at `crates/fono/src/daemon.rs:489-498`. Rationale: the current
      `OLLAMA_HOST` proxy is unrelated to whether a server is configured, and
      the assistant has no way to surface Network in the tray at all today.
- [ ] Task 9. Update `cloud_pair` (`crates/fono-core/src/providers.rs:273-288`)
      and the catalogue-coverage skip-lists in
      `crates/fono-core/src/provider_catalog.rs:1182` and
      `crates/fono-core/src/provider_catalog.rs:1192` for the merged enum.

### Phase 3 — Runtime construction

- [ ] Task 10. Delete `manual_local_server_endpoint`
      (`crates/fono-assistant/src/factory.rs:506-518`) and the fork in
      `build_ollama` (`crates/fono-assistant/src/factory.rs:599-609`,
      plus the no-feature twin at
      `crates/fono-assistant/src/factory.rs:641-649`). Reduce
      `uses_embedded_local_model`
      (`crates/fono-assistant/src/factory.rs:526-529`) to a single
      `matches!(.., Local)`. Rationale: this is the whole bug — one variant, two
      execution shapes, discriminated by string sniffing.
- [ ] Task 11. Give both factories explicit `Local` and `Network` dispatch arms
      (`crates/fono-assistant/src/factory.rs:250-259`,
      `crates/fono-polish/src/factory.rs:66-104`). `Network` constructs the
      OpenAI-compatible client from `network.url` + `network.model`, with the
      optional bearer resolved through the secret store when
      `network.api_key_ref` is set. Replace
      `PolishBackend::None => unreachable!()`
      (`crates/fono-polish/src/factory.rs:101`) with `Ok(None)` to match the
      assistant and remove a latent panic.
- [ ] Task 12. Extract the shared cloud key/model fall-through — duplicated at
      `crates/fono-assistant/src/factory.rs:49-121` and
      `crates/fono-polish/src/factory.rs:19-47` — into one `fono-core` helper
      that takes `LlmBackend` + `LlmCloud`. Reconcile the divergent
      unknown-provider default (`""` at
      `crates/fono-assistant/src/factory.rs:134` vs `"llama3.1-8b"` at
      `crates/fono-polish/src/defaults.rs:19-24`) onto the catalogue value.
      Rationale: with `provider` gone from the cloud table, the resolver must
      key off `backend`, so both copies change anyway.
- [ ] Task 13. Update the remaining runtime consumers, none of which the
      compiler will catch because they use `matches!` rather than exhaustive
      matches: `server_assistant_model_name`
      (`crates/fono-assistant/src/factory.rs:350-353`) → `Local`⇒`local.model`,
      `Network`⇒`network.model`, else `cloud.model`; `chat_endpoint`
      (`crates/fono-assistant/src/factory.rs:377-388`) → `None` for
      `None`/`Local`/`Network`/`Anthropic`; the override target at
      `crates/fono-assistant/src/factory.rs:292`; `llm_timeouts`
      (`crates/fono-mcp-server/src/summarize.rs:62`) → long timeouts for
      `Local` and `Network`; `FALLBACK_ORDER`
      (`crates/fono-mcp-server/src/summarize.rs:71-79`); model prefetch at
      `crates/fono/src/models.rs:257` and `crates/fono/src/models.rs:360-367`;
      `backend_is_vision_capable` (`crates/fono/src/session.rs:196-204`).
- [ ] Task 14. Align the embedded-engine thread choice between the two roles:
      polish branches on `stream_injection`
      (`crates/fono-polish/src/factory.rs:229-237`) while the assistant always
      uses `with_threads` (`crates/fono-assistant/src/factory.rs:578-587`).
      Pick one policy and document it in a comment. Rationale: with both roles
      now sharing a `Local` variant, an unexplained behavioural split is a
      future bug report.

### Phase 4 — Tray, CLI, doctor, wizard

- [ ] Task 15. Update the tray menu construction and active-index resolution
      (`crates/fono/src/daemon.rs:479-582`) to the merged helpers, and the
      tray-driven switch handler at `crates/fono/src/daemon.rs:2500` /
      `crates/fono/src/daemon.rs:2453` / `crates/fono/src/daemon.rs:2517` so
      auto-download of the GGUF triggers on `Local`. Rationale: this is where
      the user sees "local" today for a config that says "ollama".
- [ ] Task 16. Show the network host in the tray label when `Network` is
      selected (e.g. `network · 192.168.0.200:11434`), mirroring the Wyoming
      peer labels. Rationale: with three local-ish options, a bare "network"
      does not tell the user *which* server is live.
- [ ] Task 17. Extend `fono use assistant` / `fono use llm`
      (`crates/fono/src/cli.rs:2074-2090`, setters at
      `crates/fono/src/cli.rs:2016-2025` and
      `crates/fono/src/cli.rs:2065-2071`) to accept `local`, `network --url
      <URL> [--model <ID>]`, and provider ids — following the existing
      `fono use tts --uri` precedent. The setters must clear the sub-tables
      that do not apply to the new selection. Rationale: the CLI is the
      scriptable path and must be able to express every state the UI can.
- [ ] Task 18. Update `fono use show` (`crates/fono/src/cli.rs:2111`,
      `crates/fono/src/cli.rs:2207-2218`) and the doctor provider tables
      (`crates/fono/src/doctor.rs:697-720`, model resolution at
      `crates/fono/src/doctor.rs:549-555`) to print the resolved selection plus,
      for `Network`, the endpoint.
- [ ] Task 19. Add a doctor reachability probe for a configured `Network`
      endpoint (HTTP GET of the server's model list), reported next to the
      existing per-provider key checks. Rationale: "I pointed it at my server
      and nothing happens" is the main failure mode this shape introduces, and
      doctor is where users already look.
- [ ] Task 20. Update the wizard: the three `AssistantBackend::Ollama` sites
      (`crates/fono/src/wizard.rs:797`, `crates/fono/src/wizard.rs:1125`,
      `crates/fono/src/wizard.rs:1795`) become `LlmBackend::Local`, and the
      polish sites (`crates/fono/src/wizard.rs:430`,
      `crates/fono/src/wizard.rs:1088`, `crates/fono/src/wizard.rs:1367`,
      `crates/fono/src/wizard.rs:1783`, `crates/fono/src/wizard.rs:1878`)
      follow. Keep the wizard offering only `local` and cloud providers —
      `network` stays a settings-page/CLI choice so the first-run flow does not
      grow a URL prompt.

### Phase 5 — Web settings UI

- [ ] Task 21. Delete `assistantIsNetwork`
      (`crates/fono-net/src/web_settings/assets/app.js:289-292`) and reduce both
      segment resolvers (`crates/fono-net/src/web_settings/assets/app.js:281-297`)
      to a direct read of `*.backend`. Rationale: the JS currently reimplements
      the Rust marker-string hack; with a real `network` value both become
      one-liners.
- [ ] Task 22. Rewrite the two segment writers
      (`crates/fono-net/src/web_settings/assets/app.js:424-475`) and the two
      provider-card handlers
      (`crates/fono-net/src/web_settings/assets/app.js:483-494`) to write only
      `backend` plus the one sub-table that applies, clearing the others. Drop
      `provider` writes and the `LOCAL_SERVER_URL`-into-`api_key_ref`
      assignment; seed `*.network.url` instead.
- [ ] Task 23. Rebind the Network panels
      (`crates/fono-net/src/web_settings/assets/app.js:604-607` for cleanup,
      `crates/fono-net/src/web_settings/assets/app.js:676-679` for the
      assistant) to `*.network.url` / `*.network.model`, add the optional
      bearer-secret row, and reword the helper text away from Ollama toward
      "any OpenAI-compatible server (Ollama, llama.cpp, LM Studio, vLLM, …)".
      Update the section summaries at
      `crates/fono-net/src/web_settings/assets/app.js:595` and
      `crates/fono-net/src/web_settings/assets/app.js:662`, which currently
      display `api_key_ref` as if it were a URL.
- [ ] Task 24. Add a **Test connection** action to the Network panel: a small
      authenticated daemon endpoint that calls the configured URL's model-list
      route and returns the ids, rendered as inline status plus a model
      dropdown that falls back to free text. Rationale: the biggest single UX
      win here — it turns "type a URL and a model id and hope" into a verified
      pick, and the endpoint may be LAN-side so the browser cannot probe it
      directly.
- [ ] Task 25. Replace the free-text local-model field in `localLlmPanel`
      (`crates/fono-net/src/web_settings/assets/app.js:301-305`) with a
      dropdown of installed/installable GGUF ids fed from `/api/meta`,
      following the `tts_local` engine/voice pattern
      (`crates/fono-net/src/web_settings/assets/app.js:311-346`), keeping a
      free-text escape hatch. Rationale: today a typo silently yields "model
      not found" only at first use.
- [ ] Task 26. When the master enable toggle turns a role on while
      `backend == none`, auto-select `local` rather than rendering an empty
      cloud provider grid (current behaviour: `assistantSeg()` falls through to
      `'cloud'` with no card pressed,
      `crates/fono-net/src/web_settings/assets/app.js:293-297`). Rationale: the
      "enabled but no backend" state is reachable and looks broken.
- [ ] Task 27. Update the config-coverage test and its `FILE_ONLY` allow-list
      (`crates/fono-net/src/web_settings/mod.rs:919-951`, fixture setup at
      `crates/fono-net/src/web_settings/mod.rs:980-990`) for the new leaves:
      bind `*.network.*`, drop the removed `*.cloud.provider`, and revisit the
      `polish.local` / `assistant.local` entries now that Task 25 surfaces the
      model id. Rationale: this test is the CI gate that stops a new key from
      being invisible in the UI.

### Phase 6 — Tests and docs

- [ ] Task 28. Rewrite the factory tests that encode the old semantics:
      `crates/fono-assistant/src/factory.rs:667-1137` (notably
      `wizard_legacy_ollama_provider_ignores_stale_endpoint` at
      `crates/fono-assistant/src/factory.rs:749-763` and
      `manual_ollama_endpoint_still_builds_without_model_file` at
      `crates/fono-assistant/src/factory.rs:772`) and
      `crates/fono-polish/src/factory.rs:253-333`. Replace them with tests that
      `Local` never emits HTTP and `Network` never loads a GGUF.
- [ ] Task 29. Add migration tests: a `version = 1` file with
      `assistant.backend = "ollama"` and no markers becomes `Local`; the same
      with `provider = "ollama-server"` plus a URL becomes `Network` with that
      URL in `network.url`; `polish.backend = "ollama"` becomes `Network`.
      Rationale: locks the one-shot migration so the disposable path is provably
      correct before it is deleted.
- [ ] Task 30. Add a round-trip invariant test asserting the tray/CLI string and
      the serialised TOML value agree for every `LlmBackend` variant, replacing
      the test that currently pins the asymmetry
      (`crates/fono-core/src/providers.rs:504-507`). Rationale: this is the
      guard that prevents defect 2 from ever returning.
- [ ] Task 31. Update the integration tests that name the old variants:
      `crates/fono/tests/provider_switching.rs`,
      `crates/fono/tests/wizard_primary_flow.rs`,
      `crates/fono/tests/pipeline.rs`,
      `crates/fono-core/examples/filter_probe.rs`,
      `crates/fono/examples/smoke_assistant.rs`,
      `crates/fono/examples/smoke_realtime_live.rs`.
- [ ] Task 32. Rewrite the assistant and cleanup sections of
      `docs/providers.md` around the four-way model, documenting `[*.network]`
      as engine-agnostic and listing verified-compatible servers. Correct the
      stale `prefer_web_search` default claim at `docs/providers.md:772` (code
      says `false`, `crates/fono-core/src/config.rs:1332`).
- [ ] Task 33. Write an ADR under `docs/decisions/` recording the unified
      local/cloud/network selection, why engine-specific naming was dropped,
      and why `api_key_ref` is never a URL. Cross-reference ADR 0036
      (`docs/decisions/0036-local-llm-server-openai-ollama.md`), whose
      marker-string convention this supersedes.
- [ ] Task 34. Add the `CHANGELOG.md` entry in plain user language (schema
      change + what to re-check), update `ROADMAP.md`, and update
      `docs/status.md` at end of session, per the project's release rules.

## Verification Criteria

- `nice -n 10 cargo fmt --all -- --check`, `nice -n 10 cargo clippy --workspace
  --all-targets -- -D warnings`, and `nice -n 10 cargo test --workspace --tests
  --lib` all exit 0.
- `nice -n 10 ./tests/check.sh --size-budget` passes; the binary does not grow
  (no new dependencies are introduced).
- A grep of the workspace for `ollama-server`, `openai-compatible-local`, and
  `manual_local_server_endpoint` returns zero hits outside the migration code,
  its tests, and the superseded ADR.
- For every `LlmBackend` variant, the string shown by the tray, `fono use show`
  and doctor is byte-identical to the value serialised into `config.toml`.
- `backend = "local"` provably issues no HTTP request; `backend = "network"`
  provably loads no GGUF. Both directions covered by tests.
- Loading the user's current file (`/root/.config/fono/config.toml:70-94`)
  yields `assistant.backend = "local"`, an unchanged embedded-Gemma runtime
  behaviour, and `version = 2` after the next save.
- The web settings page renders exactly four reachable assistant states (off,
  local, cloud+provider, network) with no state that shows an empty provider
  grid, and `config_coverage_ui_or_allowlist` passes.
- Setting a bogus `network.url` produces a clear failure in doctor and in the
  page's Test-connection control, not a silent fallback to the embedded model.

## Potential Risks and Mitigations

1. **~20 non-exhaustive `matches!` sites will compile clean while silently
   changing behaviour** (enumerated in Tasks 13, 15, 20).
   Mitigation: work from the site list rather than from compiler errors; where
   practical convert `matches!` to exhaustive `match` so the compiler guards
   future variant changes.
2. **A stale `backend = "ollama"` could hard-fail `Config::load` and stop the
   daemon** if the legacy token is dropped outright.
   Mitigation: Task 2 keeps the variant parseable purely so Task 6's migration
   can run; Task 29 tests it.
3. **The polish migration is genuinely ambiguous** — `PolishBackend::Ollama`
   always meant a server, but the URL may be absent from
   `polish.cloud.api_key_ref`.
   Mitigation: default to `http://localhost:11434/v1/chat/completions` (today's
   effective behaviour at `crates/fono-polish/src/factory.rs:106-113`) and log
   an informational line naming the file and key.
4. **Web UI and Rust drift again.** The old marker convention was duplicated in
   JS by hand.
   Mitigation: after this change the JS only reads/writes `backend` and one
   typed sub-table — no derived logic to mirror. The coverage test (Task 27)
   remains the tripwire.
5. **The Test-connection endpoint is a new authenticated network surface** that
   causes the daemon to fetch a user-supplied URL.
   Mitigation: reuse the existing web-settings auth, allow only the model-list
   route, cap the timeout and response size, and never echo response bodies
   into logs.
6. **Deleting `cloud.provider` loses information** for anyone who set a provider
   there that disagrees with `backend`.
   Mitigation: `backend` was already the value the factories dispatched on, so
   the discarded field was never authoritative; note it in the changelog.
7. **Feature-gated builds** (`llama-local` off, `openai-compat` off) have their
   own stub matrix (`crates/fono-assistant/src/factory.rs:590-665`,
   `crates/fono-polish/src/factory.rs:132-251`).
   Mitigation: keep one actionable stub per new variant and check the
   non-default feature combinations compile before pushing.

## Alternative Approaches

1. **Keep two enums, just add `Local` to `AssistantBackend`.** Smallest diff and
   it fixes the overloading, but leaves `assistant_backend_str` free to keep
   lying, keeps the two roles' vocabularies independently driftable, and leaves
   the redundant `cloud.provider`. Rejected: fixes one of three defects.
2. **Introduce a separate `mode` key (`none`/`local`/`cloud`/`network`) with the
   provider under `cloud.provider`.** Mirrors the web UI's segments most
   literally, but makes cloud selection a two-step and puts the LLM roles out of
   step with STT/TTS, where `backend` already holds the full selection.
   Rejected in favour of keeping one `backend` key everywhere.
3. **Fold `network` into the cloud provider list as a pseudo-provider
   `"custom"`.** Fewer states, but the cloud grid then contains one card that
   needs a URL and no API key — exactly the shape confusion being removed.
4. **Refactor STT and TTS in the same pass** for total consistency (they share
   the redundant `cloud.provider`). Better end state, but roughly doubles the
   blast radius and mixes an unrelated migration into this one. Recommended as
   the immediate follow-up instead.
5. **Hard-fail on legacy values with a printed fix-up hint** rather than
   migrating (Tasks 2 and 6 deleted). Honest to the "no backward compatibility"
   directive and ~40 lines lighter, but a failed `Config::load` takes the daemon
   down on upgrade. Rejected on UX grounds.
