# ADR 0039 — One LLM backend vocabulary: none / local / network / provider

- **Status:** Accepted
- **Date:** 2026-07-27
- **Supersedes:** the `ollama-server` / `openai-compatible-local` marker-string
  convention used by the assistant factory (introduced alongside
  [ADR 0036](0036-local-llm-server-openai-ollama.md))
- **Related:** [ADR 0004 — Default models](0004-default-models.md),
  [ADR 0036 — Local LLM server](0036-local-llm-server-openai-ollama.md)
- **Plan:** [`plans/2026-07-27-llm-backend-consolidation-v1.md`](../../plans/2026-07-27-llm-backend-consolidation-v1.md)

## Context

Fono has two LLM roles — `[polish]` (transcript cleanup) and `[assistant]`
(the F8 voice assistant). They had two separate backend enums with
identical provider sets but *contradictory* semantics for the same word,
and neither could express "a server I run" as a first-class choice.

Three concrete defects, all user-visible:

1. **`ollama` meant two different things.** `AssistantBackend::Ollama` was
   simultaneously the only route to the embedded llama.cpp engine *and* the
   route to a network server. The two were disambiguated at runtime by a
   marker string (`"ollama-server"`, `"openai-compatible-local"`) plus a URL
   smuggled into `api_key_ref` — a field whose name says "the name of a
   secret". A config reading `backend = "ollama"` with no `[assistant.cloud]`
   block silently ran embedded Gemma. Correct by design, undiscoverable in
   practice.

2. **The tray contradicted the config file.** The assistant's display helper
   mapped `Ollama` to the string `"local"` while serde wrote `"ollama"`; the
   polish helper mapped the same variant to `"ollama"`. One word, two roles,
   two vocabularies — and the asymmetry was pinned by a passing test.

3. **Two sources of truth for the provider.** Both `backend` and
   `[<role>.cloud].provider` carried a provider id and could silently
   disagree. The web settings UI wrote both, and reimplemented the Rust
   marker hack by hand in JavaScript — which is precisely *why* the web UI
   looked correct: it was compensating in JS for a schema that could not
   express what it was rendering.

The engine-specific naming was also wrong on the merits. Fono never needed
Ollama specifically; it needs an OpenAI-compatible `/v1/chat/completions`
endpoint. `llama-server`, LM Studio, vLLM, LiteLLM and others all qualify,
and naming one vendor in the schema implied a compatibility guarantee Fono
neither makes nor requires.

## Decision

1. **One enum, `LlmBackend`, shared by both roles.** `PolishBackend` and
   `AssistantBackend` are merged; their provider sets were already
   identical. Per-role differences (default model, default backend) move
   into the `Default` impls where they belong. A single enum means a single
   set of helpers in `providers.rs`, so the two roles cannot drift apart
   again.

2. **Four mutually exclusive kinds of backend**, each with its own typed
   config table and no overloaded token:

   | `backend`         | Runs where                              | Table              |
   |-------------------|-----------------------------------------|--------------------|
   | `none`            | nowhere — role disabled                 | —                  |
   | `local`           | embedded llama.cpp, in-process          | `[<role>.local]`   |
   | `network`         | a server the user runs                  | `[<role>.network]` |
   | *provider name*   | that vendor's cloud API                 | `[<role>.cloud]`   |

   `local` never opens a socket; `network` never loads a GGUF. This is
   enforced by tests, not convention.

3. **`network` is engine-agnostic and names no vendor.** The contract is the
   OpenAI chat wire format, nothing more:

   ```toml
   [assistant.network]
   url         = "http://192.168.0.200:11434/v1/chat/completions"
   model       = "gemma4:12b"
   api_key_ref = ""          # optional bearer — never a URL
   ```

   A bare origin is completed to `/v1/chat/completions` automatically. This
   follows the existing `[stt.wyoming]` precedent for "a server on my
   network", so STT and the LLM roles stay structurally consistent.

4. **`api_key_ref` always names a secret.** The URL-in-`api_key_ref`
   smuggling and both marker strings are deleted outright. A field's name
   now matches its contents everywhere in the schema.

5. **`cloud.provider` is deleted.** `backend` is the single source of truth
   for which provider is active; `[<role>.cloud]` carries only the key
   reference and model. Two fields can disagree, one cannot.

6. **The tray, the CLI, and the TOML share one vocabulary.** The display
   string for a variant is now, by construction, the string serde writes. A
   round-trip invariant test over every variant replaces the test that
   previously pinned the asymmetry.

7. **`none` is a choice, not a fallback.** With one enum covering both
   "off" and "not yet decided", the empty state had to be disambiguated,
   and `enabled` already does it: `none` on a **disabled** role is
   obeyed, while `none` on an **enabled** role means nobody has chosen
   yet and is resolved by `resolve_llm_backend` to the best available
   option — a configured `network` server, else a cloud provider whose
   key is in `secrets.toml` (in a fixed preference order), else `local`,
   which needs no key and no network and is therefore always a working
   answer. The daemon resolves once at startup and **persists** the
   result, so the config file, the tray and the settings page never
   disagree about what is running.

   Only `secrets.toml` counts, never the process environment, matching
   `configured_llm_backends` — a key exported in one shell must not
   silently relocate a role. The preference order is duplicated in the
   settings page's JavaScript (the browser must predict the same answer
   the daemon would); a test in `fono-net` parses the JS constant and
   asserts it matches the Rust one, so the two cannot drift.

8. **No backward compatibility.** The `ollama` token is removed rather than
   migrated. Serde runs before `migrate()`, so a config still holding it
   fails to load with serde's own `unknown variant` message listing the
   valid tokens — actionable, and the schema stays free of a disposable
   compatibility path. Sanctioned by the maintainer on the grounds that the
   token had no real users; the config `version` is bumped to 2 to mark the
   break.

## Consequences

- **Breaking config change.** Any config with `backend = "ollama"` in
  `[polish]` or `[assistant]` fails to load until the owner picks `local` or
  `network`. This is the intended blast radius of dropping compatibility;
  the error names the valid tokens.
- **The web UI stops compensating.** The JS no longer reimplements a Rust
  marker convention, because the schema now expresses what the UI renders.
  The provider/segment derivation reads `backend` directly.
- **Better UX became cheap** once the schema was honest: a **Test
  connection** button that probes the server's `/v1/models` and turns the
  result into a dropdown (the endpoint is often LAN-side, so the probe runs
  daemon-side, not in the browser); a model dropdown for the local GGUF; the
  network host shown in the tray label; and the auto-resolution above, so a
  role that is switched on always runs somewhere and the settings page never
  renders an empty provider grid.
- **A disabled role no longer downloads a model.** Auto-download keyed on
  `backend == Local` alone, so `[polish]` could pull a multi-gigabyte GGUF
  for cleanup that was switched off. Both roles are now gated on `enabled`
  as well as `backend`.
- **Exhaustiveness is now the compiler's job** where practical. Roughly
  twenty sites used `matches!` rather than an exhaustive `match` and would
  have silently changed behaviour; they were converted so future variants
  fail to compile instead of failing quietly.
- **No new default models and no new dependencies.** This ADR changes how a
  backend is *selected*, not which models ship (ADR 0004 still governs), and
  adds no crates — binary size is unaffected.
- **ADR 0036 is unaffected in substance.** That ADR governs the *inbound*
  `[server.llm]` API, which still serves both the OpenAI and Ollama-native
  wire formats on port 11434. Only the outbound client-side marker
  convention it introduced is superseded here.
