// SPDX-License-Identifier: GPL-3.0-only
//! Canonical provider metadata: enum-string mapping + default API-key
//! environment-variable name. Single source of truth shared by:
//!
//! * `fono-stt` / `fono-polish` factories (resolving `cloud=None`),
//! * `fono` CLI (`fono use`, `fono keys`),
//! * `fono` wizard (key prompts, multi-provider opt-in),
//! * `fono` doctor (per-provider reachability).
//!
//! Provider-switching plan S2 — keep changes here in sync with the
//! `defaults` modules in `fono-stt` / `fono-polish` for model strings.

use crate::config::{LlmBackend, SttBackend, TtsBackend};

/// Canonical lower-case identifier for a STT backend (matches serde
/// rename and what users type on the CLI: `fono use stt groq`).
#[must_use]
pub const fn stt_backend_str(b: &SttBackend) -> &'static str {
    match b {
        SttBackend::Local => "local",
        SttBackend::Groq => "groq",
        SttBackend::Deepgram => "deepgram",
        SttBackend::OpenAI => "openai",
        SttBackend::Cartesia => "cartesia",
        SttBackend::AssemblyAI => "assemblyai",
        SttBackend::Azure => "azure",
        SttBackend::Speechmatics => "speechmatics",
        SttBackend::Google => "google",
        SttBackend::Nemotron => "nemotron",
        SttBackend::ElevenLabs => "elevenlabs",
        SttBackend::Gemini => "gemini",
        SttBackend::OpenRouter => "openrouter",
        SttBackend::Wyoming => "wyoming",
    }
}

/// Parse a CLI-style provider string into a `SttBackend`. Returns `None`
/// for unknown strings; caller surfaces a clear error.
#[must_use]
pub fn parse_stt_backend(s: &str) -> Option<SttBackend> {
    match s.to_ascii_lowercase().as_str() {
        "local" => Some(SttBackend::Local),
        "groq" => Some(SttBackend::Groq),
        "deepgram" => Some(SttBackend::Deepgram),
        "openai" => Some(SttBackend::OpenAI),
        "cartesia" => Some(SttBackend::Cartesia),
        "assemblyai" => Some(SttBackend::AssemblyAI),
        "azure" => Some(SttBackend::Azure),
        "speechmatics" => Some(SttBackend::Speechmatics),
        "google" => Some(SttBackend::Google),
        "nemotron" => Some(SttBackend::Nemotron),
        "elevenlabs" => Some(SttBackend::ElevenLabs),
        "gemini" => Some(SttBackend::Gemini),
        "openrouter" => Some(SttBackend::OpenRouter),
        "wyoming" => Some(SttBackend::Wyoming),
        _ => None,
    }
}

/// Canonical lower-case identifier for a language-model backend, shared
/// by dictation cleanup and assistant chat.
///
/// This is deliberately identical to the serde representation, so the
/// string in the tray, in `fono use show`, in `fono doctor` and in
/// `config.toml` is always the same word. (Before v2 the assistant
/// printed "local" for a backend the file called "ollama" — the two
/// must never drift again; see the `backend_str_matches_serde` test.)
#[must_use]
pub const fn llm_backend_str(b: &LlmBackend) -> &'static str {
    match b {
        LlmBackend::None => "none",
        LlmBackend::Local => "local",
        LlmBackend::Network => "network",
        LlmBackend::OpenAI => "openai",
        LlmBackend::Anthropic => "anthropic",
        LlmBackend::Gemini => "gemini",
        LlmBackend::Groq => "groq",
        LlmBackend::Cerebras => "cerebras",
        LlmBackend::OpenRouter => "openrouter",
    }
}

/// Parse a CLI-style backend string. Beyond the canonical names this
/// accepts a few things people reasonably type: `off`/`skip` for `none`,
/// and the name of a server engine (`ollama`, `llamacpp`, `lmstudio`,
/// `vllm`, `localai`) for `network` — Fono does not care which engine is
/// behind the URL, only that it speaks the OpenAI chat-completions API.
#[must_use]
pub fn parse_llm_backend(s: &str) -> Option<LlmBackend> {
    match s.to_ascii_lowercase().as_str() {
        "none" | "off" | "skip" => Some(LlmBackend::None),
        "local" | "embedded" => Some(LlmBackend::Local),
        "network" | "server" | "ollama" | "llamacpp" | "llama.cpp" | "lmstudio" | "lm-studio"
        | "vllm" | "localai" | "litellm" => Some(LlmBackend::Network),
        "openai" => Some(LlmBackend::OpenAI),
        "anthropic" => Some(LlmBackend::Anthropic),
        "gemini" => Some(LlmBackend::Gemini),
        "groq" => Some(LlmBackend::Groq),
        "cerebras" => Some(LlmBackend::Cerebras),
        "openrouter" => Some(LlmBackend::OpenRouter),
        _ => None,
    }
}

/// Canonical environment-variable name where the API key for a cloud
/// language-model backend is read from. Empty for the backends that
/// need no key (`none`, `local`, `network`); check [`llm_requires_key`]
/// first. Cleanup and the assistant share these names on purpose, so a
/// single stored key serves both and the wizard never prompts twice.
#[must_use]
pub const fn llm_key_env(b: &LlmBackend) -> &'static str {
    match b {
        LlmBackend::None | LlmBackend::Local | LlmBackend::Network => "",
        LlmBackend::OpenAI => "OPENAI_API_KEY",
        LlmBackend::Anthropic => "ANTHROPIC_API_KEY",
        LlmBackend::Gemini => "GEMINI_API_KEY",
        LlmBackend::Groq => "GROQ_API_KEY",
        LlmBackend::Cerebras => "CEREBRAS_API_KEY",
        LlmBackend::OpenRouter => "OPENROUTER_API_KEY",
    }
}

#[must_use]
pub const fn llm_requires_key(b: &LlmBackend) -> bool {
    b.is_cloud()
}

/// Canonical environment-variable name where the API key for a given
/// STT backend is read from. Returned even for `Local` (where it's
/// unused) to keep callers branch-free; check `requires_key` first.
#[must_use]
pub const fn stt_key_env(b: &SttBackend) -> &'static str {
    match b {
        SttBackend::Local => "",
        SttBackend::Groq => "GROQ_API_KEY",
        SttBackend::Deepgram => "DEEPGRAM_API_KEY",
        SttBackend::OpenAI => "OPENAI_API_KEY",
        SttBackend::Cartesia => "CARTESIA_API_KEY",
        SttBackend::AssemblyAI => "ASSEMBLYAI_API_KEY",
        SttBackend::Azure => "AZURE_API_KEY",
        SttBackend::Speechmatics => "SPEECHMATICS_API_KEY",
        SttBackend::Google => "GOOGLE_API_KEY",
        SttBackend::Nemotron => "NEMOTRON_API_KEY",
        SttBackend::ElevenLabs => "ELEVENLABS_API_KEY",
        SttBackend::Gemini => "GEMINI_API_KEY",
        SttBackend::OpenRouter => "OPENROUTER_API_KEY",
        // Wyoming v1 has no in-band auth; an optional pre-shared token
        // is configured via `[stt.wyoming].auth_token_ref` instead.
        SttBackend::Wyoming => "",
    }
}

#[must_use]
pub const fn stt_requires_key(b: &SttBackend) -> bool {
    !matches!(b, SttBackend::Local | SttBackend::Wyoming)
}

/// Canonical lower-case identifier for a TTS backend.
#[must_use]
pub const fn tts_backend_str(b: &TtsBackend) -> &'static str {
    match b {
        TtsBackend::None => "none",
        TtsBackend::Wyoming => "wyoming",
        TtsBackend::OpenAI => "openai",
        TtsBackend::Groq => "groq",
        TtsBackend::OpenRouter => "openrouter",
        TtsBackend::Cartesia => "cartesia",
        TtsBackend::Deepgram => "deepgram",
        TtsBackend::Speechmatics => "speechmatics",
        TtsBackend::ElevenLabs => "elevenlabs",
        TtsBackend::Gemini => "gemini",
        TtsBackend::Local => "local",
    }
}

#[must_use]
pub fn parse_tts_backend(s: &str) -> Option<TtsBackend> {
    match s.to_ascii_lowercase().as_str() {
        "none" | "off" | "skip" => Some(TtsBackend::None),
        "wyoming" => Some(TtsBackend::Wyoming),
        "openai" => Some(TtsBackend::OpenAI),
        "groq" => Some(TtsBackend::Groq),
        "openrouter" => Some(TtsBackend::OpenRouter),
        "cartesia" => Some(TtsBackend::Cartesia),
        "deepgram" => Some(TtsBackend::Deepgram),
        "speechmatics" => Some(TtsBackend::Speechmatics),
        "elevenlabs" => Some(TtsBackend::ElevenLabs),
        "gemini" => Some(TtsBackend::Gemini),
        "local" => Some(TtsBackend::Local),
        _ => None,
    }
}

/// Canonical environment-variable name for the API key of a cloud
/// TTS backend. Returned even for `None`/`Wyoming` (where it's
/// unused) for branch-free callers; check [`tts_requires_key`] first.
#[must_use]
pub const fn tts_key_env(b: &TtsBackend) -> &'static str {
    match b {
        TtsBackend::None | TtsBackend::Wyoming | TtsBackend::Local => "",
        TtsBackend::OpenAI => "OPENAI_API_KEY",
        TtsBackend::Groq => "GROQ_API_KEY",
        TtsBackend::OpenRouter => "OPENROUTER_API_KEY",
        TtsBackend::Cartesia => "CARTESIA_API_KEY",
        TtsBackend::Deepgram => "DEEPGRAM_API_KEY",
        TtsBackend::Speechmatics => "SPEECHMATICS_API_KEY",
        TtsBackend::ElevenLabs => "ELEVENLABS_API_KEY",
        TtsBackend::Gemini => "GEMINI_API_KEY",
    }
}

#[must_use]
pub const fn tts_requires_key(b: &TtsBackend) -> bool {
    matches!(
        b,
        TtsBackend::OpenAI
            | TtsBackend::Groq
            | TtsBackend::OpenRouter
            | TtsBackend::Cartesia
            | TtsBackend::Deepgram
            | TtsBackend::Speechmatics
            | TtsBackend::ElevenLabs
            | TtsBackend::Gemini
    )
}

/// Paired cloud preset for `fono use cloud <name>`. Returns `(stt, llm)`
/// for the preset, or `None` if the name isn't a known pair.
///
/// Looks up [`crate::provider_catalog::CLOUD_PROVIDERS`] as the source of
/// truth for which provider id offers which capabilities. When the
/// catalogue entry lacks an STT capability (e.g. Cerebras, Anthropic,
/// OpenRouter), the pair falls back to Groq's whisper-turbo — the
/// de-facto fast cloud STT today. When the entry lacks an LLM
/// capability (Deepgram, AssemblyAI), the pair falls back to Cerebras
/// for cleanup.
#[must_use]
pub fn cloud_pair(name: &str) -> Option<(SttBackend, LlmBackend)> {
    let id = name.to_ascii_lowercase();
    let entry = crate::provider_catalog::find(&id)?;
    // Resolve STT: prefer the entry's own STT capability, otherwise
    // fall back to Groq's whisper-turbo.
    let stt = if entry.stt.is_some() { parse_stt_backend(entry.id)? } else { SttBackend::Groq };
    // Resolve LLM: prefer the entry's own LLM capability, otherwise
    // fall back to Cerebras for cleanup (for STT-only providers like
    // Deepgram and AssemblyAI).
    let llm =
        if entry.polish.is_some() { parse_llm_backend(entry.id)? } else { LlmBackend::Cerebras };
    Some((stt, llm))
}

/// Iterator over every STT backend (for doctor enumeration etc.).
#[must_use]
pub fn all_stt_backends() -> [SttBackend; 14] {
    [
        SttBackend::Local,
        SttBackend::Groq,
        SttBackend::OpenAI,
        SttBackend::Deepgram,
        SttBackend::AssemblyAI,
        SttBackend::Cartesia,
        SttBackend::Azure,
        SttBackend::Speechmatics,
        SttBackend::Google,
        SttBackend::Nemotron,
        SttBackend::ElevenLabs,
        SttBackend::Gemini,
        SttBackend::OpenRouter,
        SttBackend::Wyoming,
    ]
}

/// Canonical lower-case identifier for an assistant chat backend.
#[must_use]
pub fn all_llm_backends() -> [LlmBackend; 9] {
    [
        LlmBackend::None,
        LlmBackend::Local,
        LlmBackend::Network,
        LlmBackend::Cerebras,
        LlmBackend::Groq,
        LlmBackend::OpenAI,
        LlmBackend::Anthropic,
        LlmBackend::OpenRouter,
        LlmBackend::Gemini,
    ]
}

#[must_use]
pub fn all_tts_backends() -> [TtsBackend; 11] {
    [
        TtsBackend::None,
        TtsBackend::Wyoming,
        TtsBackend::OpenAI,
        TtsBackend::Groq,
        TtsBackend::OpenRouter,
        TtsBackend::Cartesia,
        TtsBackend::Deepgram,
        TtsBackend::Speechmatics,
        TtsBackend::ElevenLabs,
        TtsBackend::Gemini,
        TtsBackend::Local,
    ]
}

/// Subset of [`all_stt_backends`] the user can actually pick today,
/// given the loaded `Secrets`. `Local` is always included; cloud
/// backends are included iff their API key is **explicitly listed in
/// `secrets.toml`**. The process environment is intentionally
/// ignored so a stray `OPENAI_API_KEY` exported in the user's shell
/// doesn't clutter the tray submenu — to surface a backend the user
/// must run `fono keys add <NAME>`. `active` is always included even
/// if its key is missing, so the tray reflects the current selection.
#[must_use]
pub fn configured_stt_backends(secrets: &crate::Secrets, active: &SttBackend) -> Vec<SttBackend> {
    all_stt_backends()
        .into_iter()
        .filter(|b| {
            if std::mem::discriminant(b) == std::mem::discriminant(active) {
                return true;
            }
            // Wyoming has no API key — its opt-in is `[stt.wyoming].uri`
            // (manual config) or mDNS discovery (Slice 4 will inject
            // discovered peers separately). Hide it from the menu
            // until then to avoid a dead row.
            if matches!(b, SttBackend::Wyoming) {
                return false;
            }
            if !stt_requires_key(b) {
                return true;
            }
            secrets.has_in_file(stt_key_env(b))
        })
        .collect()
}

/// Same idea as [`configured_stt_backends`] but for the language-model
/// roles (cleanup and assistant chat). Always includes `None` and
/// `Local` (neither needs a key or a server). `Network` appears only
/// once the role actually has a server URL configured — a bare
/// "network" row that points nowhere is a dead end, so
/// `has_network_url` gates it, exactly as `[tts.wyoming]` gates the
/// Wyoming row. Like its STT cousin, the process environment is
/// ignored: only keys saved in `secrets.toml` count.
#[must_use]
pub fn configured_llm_backends(
    secrets: &crate::Secrets,
    active: &LlmBackend,
    has_network_url: bool,
) -> Vec<LlmBackend> {
    all_llm_backends()
        .into_iter()
        .filter(|b| {
            if b == active {
                return true;
            }
            if matches!(b, LlmBackend::Network) {
                return has_network_url;
            }
            if !llm_requires_key(b) {
                return true;
            }
            secrets.has_in_file(llm_key_env(b))
        })
        .collect()
}

/// Preference order used by [`resolve_llm_backend`] when a role has no
/// explicit backend. Cloud first, cheapest-and-fastest first, because a
/// user who has saved a key has already told us where they want to run.
/// Only providers that actually serve both LLM roles appear here.
const LLM_AUTOSELECT_ORDER: [LlmBackend; 6] = [
    LlmBackend::Groq,
    LlmBackend::Cerebras,
    LlmBackend::OpenAI,
    LlmBackend::Anthropic,
    LlmBackend::Gemini,
    LlmBackend::OpenRouter,
];

/// The auto-select preference order, for callers that must reproduce it
/// (the web settings page duplicates it in JavaScript, and a test in
/// `fono-net` asserts the two stay identical).
#[must_use]
pub const fn llm_autoselect_order() -> [LlmBackend; 6] {
    LLM_AUTOSELECT_ORDER
}

/// Decide what a role should actually run when its configured backend
/// is `None`.
///
/// `None` used to mean two different things: "the user switched this
/// off" and "nobody has chosen yet". Those deserve opposite treatment —
/// the first must be obeyed, the second should just work. Fono now
/// distinguishes them with `enabled`: a role that is **off** stays off,
/// and `None` is only ever inferred for a role the user has switched
/// **on** without saying where it should run.
///
/// Resolution order, best-available first:
///
/// 1. an explicit backend — always wins, including an explicit `None`
///    on a disabled role,
/// 2. a self-hosted server, if one is configured,
/// 3. a cloud provider whose key is in `secrets.toml`, in
///    [`LLM_AUTOSELECT_ORDER`],
/// 4. `Local` — the on-device model, which needs no key and no network
///    and is therefore always a working answer.
///
/// The process environment is deliberately ignored (only `secrets.toml`
/// counts), matching [`configured_llm_backends`], so a stray exported
/// key in one shell cannot silently change where a role runs.
#[must_use]
pub fn resolve_llm_backend(
    configured: LlmBackend,
    enabled: bool,
    secrets: &crate::Secrets,
    has_network_url: bool,
) -> LlmBackend {
    if !matches!(configured, LlmBackend::None) || !enabled {
        return configured;
    }
    if has_network_url {
        return LlmBackend::Network;
    }
    LLM_AUTOSELECT_ORDER
        .into_iter()
        .find(|b| secrets.has_in_file(llm_key_env(b)))
        .unwrap_or(LlmBackend::Local)
}

/// Same idea as [`configured_stt_backends`] but for TTS backends.
/// Order: backends whose API key is already present in `secrets.toml`
/// come first (so the tray's "(cloud, key already set)" entries lead),
/// followed by Wyoming if the user has configured a `[tts.wyoming]`
/// peer (or it's the active backend), followed by every remaining
/// cloud backend (which would prompt for a key). Always includes the
/// currently-active backend so the tray reflects reality even if its
/// key isn't in `secrets.toml`. Like its STT cousin, the process
/// environment is ignored — only keys saved in `secrets.toml` count.
///
/// `None` is intentionally excluded — it is not a real switchable
/// option. Always includes the currently-active backend so the tray
/// reflects reality even if its key isn't in `secrets.toml`. Like its
/// STT cousin, the process environment is ignored — only keys saved
/// in `secrets.toml` count.
#[must_use]
pub fn configured_tts_backends(
    secrets: &crate::Secrets,
    active: &TtsBackend,
    has_wyoming_block: bool,
) -> Vec<TtsBackend> {
    let mut with_key: Vec<TtsBackend> = Vec::new();
    let mut without_key: Vec<TtsBackend> = Vec::new();
    let mut wyoming: Option<TtsBackend> = None;
    for b in all_tts_backends() {
        if matches!(b, TtsBackend::None) {
            // None is not a real entry; only include when active.
            if std::mem::discriminant(&b) == std::mem::discriminant(active) {
                without_key.push(b);
            }
            continue;
        }
        if matches!(b, TtsBackend::Wyoming) {
            let active_match = std::mem::discriminant(&b) == std::mem::discriminant(active);
            if has_wyoming_block || active_match {
                wyoming = Some(b);
            }
            continue;
        }
        let active_match = std::mem::discriminant(&b) == std::mem::discriminant(active);
        if tts_requires_key(&b) && secrets.has_in_file(tts_key_env(&b)) {
            with_key.push(b);
        } else if active_match {
            // Active backend without a stored key — still show.
            without_key.push(b);
        } else {
            without_key.push(b);
        }
    }
    let mut out = with_key;
    if let Some(w) = wyoming {
        out.push(w);
    }
    out.extend(without_key);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_roundtrip() {
        for b in all_stt_backends() {
            let s = stt_backend_str(&b);
            assert_eq!(parse_stt_backend(s).unwrap(), b);
        }
    }

    #[test]
    fn llm_roundtrip() {
        for b in all_llm_backends() {
            let s = llm_backend_str(&b);
            assert_eq!(parse_llm_backend(s).unwrap(), b);
        }
    }

    /// The string the user READS (tray, `fono use show`, doctor) must be
    /// byte-identical to the string WRITTEN to `config.toml`. Before v2
    /// the assistant displayed "local" for a backend serialised as
    /// "ollama", so the tray and the config file disagreed on screen.
    /// This test is the guard that stops that class of bug returning.
    #[test]
    fn backend_str_matches_serde() {
        for b in all_llm_backends() {
            let serialised = toml::Value::try_from(b).expect("backend serialises");
            assert_eq!(
                serialised.as_str().expect("backend serialises to a string"),
                llm_backend_str(&b),
                "display string and config value must not drift for {b:?}"
            );
        }
    }

    /// `ollama` is no longer a backend value: an embedded model is
    /// `local` and a self-hosted server is `network`. The CLI still
    /// accepts engine names as a convenience, all pointing at `network`,
    /// because which engine serves the URL is irrelevant to Fono.
    #[test]
    fn engine_names_parse_to_network() {
        for name in ["ollama", "llamacpp", "lmstudio", "vllm", "localai", "server"] {
            assert_eq!(parse_llm_backend(name), Some(LlmBackend::Network), "{name}");
        }
        assert_eq!(parse_llm_backend("local"), Some(LlmBackend::Local));
        assert_eq!(parse_llm_backend("embedded"), Some(LlmBackend::Local));
        // ...but the canonical spelling is what gets written back.
        assert_eq!(llm_backend_str(&LlmBackend::Network), "network");
    }

    #[test]
    fn unknown_returns_none() {
        assert!(parse_stt_backend("nope").is_none());
        assert!(parse_llm_backend("nope").is_none());
    }

    #[test]
    fn key_env_matches_provider() {
        assert_eq!(stt_key_env(&SttBackend::Groq), "GROQ_API_KEY");
        assert_eq!(llm_key_env(&LlmBackend::Cerebras), "CEREBRAS_API_KEY");
        assert!(stt_key_env(&SttBackend::Local).is_empty());
        assert!(llm_key_env(&LlmBackend::None).is_empty());
    }

    #[test]
    fn requires_key_flags() {
        assert!(!stt_requires_key(&SttBackend::Local));
        assert!(stt_requires_key(&SttBackend::Groq));
        // Off, embedded and self-hosted all need no API key.
        assert!(!llm_requires_key(&LlmBackend::None));
        assert!(!llm_requires_key(&LlmBackend::Local));
        assert!(!llm_requires_key(&LlmBackend::Network));
        assert!(llm_requires_key(&LlmBackend::Cerebras));
    }

    #[test]
    fn cloud_pairs() {
        let (s, l) = cloud_pair("groq").unwrap();
        assert!(matches!(s, SttBackend::Groq));
        assert!(matches!(l, LlmBackend::Groq));
        let (s, l) = cloud_pair("cerebras").unwrap();
        assert!(matches!(s, SttBackend::Groq));
        assert!(matches!(l, LlmBackend::Cerebras));
        assert!(cloud_pair("nope").is_none());
    }

    #[test]
    fn configured_filter_ignores_env() {
        // Env-fallback would have leaked OPENAI_API_KEY into the menu;
        // the new filter must read secrets.toml only.
        std::env::set_var("OPENAI_API_KEY", "leaky-env-value");
        std::env::set_var("CEREBRAS_API_KEY", "leaky-env-value");
        let secrets = crate::Secrets::default(); // empty file
        let stt = configured_stt_backends(&secrets, &SttBackend::Local);
        let llm = configured_llm_backends(&secrets, &LlmBackend::None, false);
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("CEREBRAS_API_KEY");
        // Only key-free backends + the active one should be present.
        assert_eq!(stt, vec![SttBackend::Local], "env vars should not expand the STT menu");
        assert!(
            !llm.iter().any(|b| matches!(b, LlmBackend::OpenAI)),
            "env-only OPENAI_API_KEY should not show OpenAI in the LLM menu"
        );
        assert!(
            !llm.iter().any(|b| matches!(b, LlmBackend::Cerebras)),
            "env-only CEREBRAS_API_KEY should not show Cerebras in the LLM menu"
        );
    }

    #[test]
    fn configured_filter_includes_explicit_keys() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("GROQ_API_KEY", "gsk-explicit");
        secrets.insert("CEREBRAS_API_KEY", "cs-explicit");
        let stt = configured_stt_backends(&secrets, &SttBackend::Local);
        let llm = configured_llm_backends(&secrets, &LlmBackend::None, false);
        assert!(stt.iter().any(|b| matches!(b, SttBackend::Groq)));
        assert!(llm.iter().any(|b| matches!(b, LlmBackend::Cerebras)));
        // Backends without explicit keys must remain hidden.
        assert!(!stt.iter().any(|b| matches!(b, SttBackend::OpenAI)));
        assert!(!llm.iter().any(|b| matches!(b, LlmBackend::Anthropic)));
    }

    /// `network` is only offered once a server URL exists — otherwise the
    /// menu row leads nowhere. Embedded `local` is always offered.
    #[test]
    fn configured_filter_gates_network_on_url() {
        let secrets = crate::Secrets::default();
        let without = configured_llm_backends(&secrets, &LlmBackend::None, false);
        assert!(
            !without.iter().any(|b| matches!(b, LlmBackend::Network)),
            "network must be hidden until a server URL is configured"
        );
        assert!(without.iter().any(|b| matches!(b, LlmBackend::Local)));

        let with = configured_llm_backends(&secrets, &LlmBackend::None, true);
        assert!(
            with.iter().any(|b| matches!(b, LlmBackend::Network)),
            "network must appear once a server URL is configured"
        );
    }

    /// The active backend is always listed, even when it would otherwise
    /// be filtered out, so the tray never misrepresents the live state.
    #[test]
    fn configured_filter_always_includes_active() {
        let secrets = crate::Secrets::default();
        let llm = configured_llm_backends(&secrets, &LlmBackend::Network, false);
        assert!(llm.contains(&LlmBackend::Network));
        let llm = configured_llm_backends(&secrets, &LlmBackend::Anthropic, false);
        assert!(llm.contains(&LlmBackend::Anthropic));
    }

    /// A role the user switched OFF stays off. `None` on a disabled
    /// role is an explicit choice and must never be second-guessed.
    #[test]
    fn resolve_respects_a_disabled_role() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("GROQ_API_KEY", "gsk-explicit");
        assert_eq!(
            resolve_llm_backend(LlmBackend::None, false, &secrets, true),
            LlmBackend::None,
            "a disabled role must stay off even with a key and a server available"
        );
    }

    /// An explicitly chosen backend always wins over auto-selection.
    #[test]
    fn resolve_never_overrides_an_explicit_choice() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("GROQ_API_KEY", "gsk-explicit");
        for b in [LlmBackend::Local, LlmBackend::Network, LlmBackend::Anthropic] {
            assert_eq!(resolve_llm_backend(b, true, &secrets, true), b);
        }
    }

    /// With nothing configured, an enabled role falls back to the
    /// on-device model — the one answer that always works.
    #[test]
    fn resolve_falls_back_to_local() {
        let secrets = crate::Secrets::default();
        assert_eq!(resolve_llm_backend(LlmBackend::None, true, &secrets, false), LlmBackend::Local);
    }

    /// A saved cloud key beats the on-device model: the user has already
    /// said where they want to run.
    #[test]
    fn resolve_prefers_a_saved_cloud_key_over_local() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("ANTHROPIC_API_KEY", "sk-ant");
        assert_eq!(
            resolve_llm_backend(LlmBackend::None, true, &secrets, false),
            LlmBackend::Anthropic
        );
    }

    /// A self-hosted server beats every cloud key: it is the more
    /// deliberate act of configuration.
    #[test]
    fn resolve_prefers_a_configured_server_over_cloud() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("GROQ_API_KEY", "gsk-explicit");
        assert_eq!(
            resolve_llm_backend(LlmBackend::None, true, &secrets, true),
            LlmBackend::Network
        );
    }

    /// Ties are broken by `LLM_AUTOSELECT_ORDER`, not by chance.
    #[test]
    fn resolve_is_deterministic_across_several_keys() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("OPENAI_API_KEY", "sk-openai");
        secrets.insert("ANTHROPIC_API_KEY", "sk-ant");
        secrets.insert("GROQ_API_KEY", "gsk-groq");
        assert_eq!(
            resolve_llm_backend(LlmBackend::None, true, &secrets, false),
            LlmBackend::Groq,
            "must follow LLM_AUTOSELECT_ORDER, which puts Groq first"
        );
    }

    /// Auto-selection must only ever land on something buildable —
    /// never `None` (which would leave an enabled role dead) and never
    /// `Network` without a URL.
    #[test]
    fn resolve_never_yields_an_unusable_backend() {
        let mut secrets = crate::Secrets::default();
        for env in all_llm_backends().iter().filter(|b| llm_requires_key(b)) {
            secrets.insert(llm_key_env(env), "k");
        }
        for has_url in [false, true] {
            for enabled in [false, true] {
                let got = resolve_llm_backend(LlmBackend::None, enabled, &secrets, has_url);
                if !enabled {
                    assert_eq!(got, LlmBackend::None);
                    continue;
                }
                assert_ne!(got, LlmBackend::None, "an enabled role must resolve to something");
                if got == LlmBackend::Network {
                    assert!(has_url, "must not pick network without a server URL");
                }
            }
        }
    }

    /// Every entry in the auto-select order must be a cloud provider
    /// with a key env — otherwise the `has_in_file` probe is nonsense.
    #[test]
    fn autoselect_order_is_all_keyed_cloud() {
        for b in LLM_AUTOSELECT_ORDER {
            assert!(b.is_cloud(), "{b:?} is not a cloud provider");
            assert!(!llm_key_env(&b).is_empty(), "{b:?} has no key env");
        }
    }

    /// Phase F regression: every TTS backend variant must round-trip
    /// through `parse_tts_backend` / `tts_backend_str`.
    #[test]
    fn tts_roundtrip() {
        for b in all_tts_backends() {
            let s = tts_backend_str(&b);
            assert_eq!(parse_tts_backend(s).unwrap(), b);
        }
        // New Phase F variants explicitly:
        assert_eq!(parse_tts_backend("groq"), Some(TtsBackend::Groq));
        assert_eq!(parse_tts_backend("openrouter"), Some(TtsBackend::OpenRouter));
        assert_eq!(parse_tts_backend("cartesia"), Some(TtsBackend::Cartesia));
        assert_eq!(parse_tts_backend("deepgram"), Some(TtsBackend::Deepgram));
    }

    /// Phase F: every new cloud TTS backend reports the canonical
    /// env-var name. Mirrors `key_env_matches_provider` for STT/LLM.
    #[test]
    fn tts_key_env_matches_provider() {
        assert_eq!(tts_key_env(&TtsBackend::Groq), "GROQ_API_KEY");
        assert_eq!(tts_key_env(&TtsBackend::OpenRouter), "OPENROUTER_API_KEY");
        assert_eq!(tts_key_env(&TtsBackend::Cartesia), "CARTESIA_API_KEY");
        assert_eq!(tts_key_env(&TtsBackend::Deepgram), "DEEPGRAM_API_KEY");
        assert_eq!(tts_key_env(&TtsBackend::OpenAI), "OPENAI_API_KEY");
        assert!(tts_key_env(&TtsBackend::None).is_empty());
        assert!(tts_key_env(&TtsBackend::Wyoming).is_empty());
    }

    /// `configured_tts_backends` ordering: stored-key cloud first,
    /// then Wyoming when the user has a `[tts.wyoming]` block, then
    /// every remaining cloud backend (omitting `None`).
    #[test]
    fn configured_tts_ordering() {
        let mut secrets = crate::Secrets::default();
        secrets.insert("GROQ_API_KEY", "gsk-x");
        secrets.insert("OPENAI_API_KEY", "sk-x");
        let backends = configured_tts_backends(&secrets, &TtsBackend::None, true);
        // First, every cloud backend whose key is in secrets.toml.
        // Order is the canonical `all_tts_backends` order, which
        // places OpenAI before Groq.
        let cloud_present: Vec<_> =
            backends.iter().take_while(|b| !matches!(b, TtsBackend::Wyoming)).cloned().collect();
        assert_eq!(
            cloud_present,
            vec![TtsBackend::OpenAI, TtsBackend::Groq],
            "stored-key cloud backends must lead, in catalogue order"
        );
        // Wyoming next, because the caller asserted a [tts.wyoming]
        // block exists.
        assert!(
            backends.contains(&TtsBackend::Wyoming),
            "Wyoming must appear when has_wyoming_block = true"
        );
        let wyoming_pos = backends
            .iter()
            .position(|b| matches!(b, TtsBackend::Wyoming))
            .expect("wyoming must be present");
        // Every entry after wyoming is a cloud backend with no stored key
        // (or `None` if it happened to be active — but in this
        // test the active backend is `None`, which placed `None` after
        // Wyoming as a disable affordance).
        for b in &backends[wyoming_pos + 1..] {
            if matches!(b, TtsBackend::None | TtsBackend::Local) {
                // `None` is the disable affordance; `Local` is a keyless
                // on-device backend — neither requires an API key.
                continue;
            }
            assert!(tts_requires_key(b));
            assert!(!secrets.has_in_file(tts_key_env(b)));
        }
    }

    /// Wyoming is hidden when neither `[tts.wyoming]` is configured
    /// nor it's the active backend.
    #[test]
    fn configured_tts_hides_wyoming_without_block() {
        let secrets = crate::Secrets::default();
        let backends = configured_tts_backends(&secrets, &TtsBackend::None, false);
        assert!(!backends.contains(&TtsBackend::Wyoming));
        // Active is None, so None ends up in the list (only when active).
        assert!(backends.contains(&TtsBackend::None));
    }

    /// Active backend is always present even when its key is missing
    /// and it would otherwise be filtered out.
    #[test]
    fn configured_tts_always_includes_active() {
        let secrets = crate::Secrets::default();
        let backends = configured_tts_backends(&secrets, &TtsBackend::Cartesia, false);
        assert!(backends.contains(&TtsBackend::Cartesia));
    }
}
