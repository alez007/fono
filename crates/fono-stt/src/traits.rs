// SPDX-License-Identifier: GPL-3.0-only
//! STT trait definition.

use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct Transcription {
    pub text: String,
    pub language: Option<String>,
    pub duration_ms: Option<u64>,
}

/// Per-call transcription options. Passed to [`SpeechToText::transcribe_with_opts`].
///
/// Added in Phase D (hover-context injection). The default value produces
/// identical behaviour to calling [`SpeechToText::transcribe`] directly, so
/// backends that don't override `transcribe_with_opts` are unaffected.
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    /// Per-call language override. `None` defers to the backend's configured
    /// allow-list / auto-detect behaviour (same as passing `None` to
    /// `transcribe()`).
    pub lang_override: Option<String>,
    /// Short vocabulary hint for Whisper's `initial_prompt` (or the cloud
    /// equivalent). Backends that don't support it silently ignore this field.
    pub context_hint: Option<String>,
    /// Literal terms the speaker is likely to say — proper nouns, room and
    /// device names, product names, shell fragments. Recognisers bias
    /// towards these spellings without being required to emit them.
    ///
    /// Backends with a dedicated field on the wire (OpenAI's `keywords[]`)
    /// send them there; the rest fold them into the prompt, which is where
    /// this kind of hint lived before a dedicated field existed. Backends
    /// with neither ignore the list.
    pub keywords: Vec<String>,
}

impl TranscribeOptions {
    /// Free-text prompt payload for backends with **no** dedicated
    /// keyword field on the wire: the context hint with the literal
    /// terms appended, comma-joined.
    ///
    /// Whisper's `prompt` / `initial_prompt` is exactly a spelling-bias
    /// channel, so a comma-joined term list is the idiomatic payload —
    /// it is what the window classifier has always put there. Backends
    /// that do have a keyword field (OpenAI `gpt-transcribe`) send
    /// [`Self::context_hint`] alone and pass the terms separately.
    ///
    /// Returns `None` only when there is no hint and no keywords.
    #[must_use]
    pub fn folded_hint(&self) -> Option<String> {
        let joined = self
            .keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        match (self.context_hint.as_deref(), joined.is_empty()) {
            (Some(h), true) => Some(h.to_string()),
            (Some(h), false) => Some(format!("{h} {joined}")),
            (None, true) => None,
            (None, false) => Some(joined),
        }
    }
}

#[async_trait]
pub trait SpeechToText: Send + Sync {
    /// One-shot transcription of a full PCM buffer (mono f32 @ `sample_rate`).
    async fn transcribe(
        &self,
        pcm: &[f32],
        sample_rate: u32,
        lang: Option<&str>,
    ) -> Result<Transcription>;

    /// Context-aware transcription. The default implementation forwards to
    /// [`Self::transcribe`] ignoring the `context_hint` so existing backends
    /// that don't override this method behave identically to before.
    ///
    /// Override in backends that support prompt injection (WhisperLocal,
    /// OpenAI, Groq) to route `opts.context_hint` into the appropriate field.
    async fn transcribe_with_opts(
        &self,
        pcm: &[f32],
        sample_rate: u32,
        opts: &TranscribeOptions,
    ) -> Result<Transcription> {
        self.transcribe(pcm, sample_rate, opts.lang_override.as_deref()).await
    }

    /// Backend identifier for history / logging.
    fn name(&self) -> &'static str;

    fn supports_streaming(&self) -> bool {
        false
    }

    /// Optional best-effort warmup. Cloud backends should fire a cheap
    /// HEAD/GET to pay TCP+TLS+DNS off the hot path; local backends
    /// should mmap their model. Default impl is a no-op so most
    /// implementors don't need to override.
    ///
    /// Errors are non-fatal — callers log + continue. See latency
    /// plan task L2/L3.
    async fn prewarm(&self) -> Result<()> {
        Ok(())
    }

    /// True for backends that run entirely on the local machine
    /// (whisper.cpp, local-only Wyoming, future Vosk). Cloud backends
    /// (OpenAI, Groq, OpenRouter) leave this at the `false` default.
    ///
    /// Used by the orchestrator to decide whether the post-release
    /// "polishing" overlay should run the synthetic thinking
    /// animation: local backends take 1–3 s and benefit from active
    /// feedback; cloud backends finish sub-second and would just
    /// flash.
    fn is_local(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(hint: Option<&str>, keywords: &[&str]) -> TranscribeOptions {
        TranscribeOptions {
            lang_override: None,
            context_hint: hint.map(str::to_string),
            keywords: keywords.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    #[test]
    fn folded_hint_covers_every_combination() {
        assert_eq!(opts(None, &[]).folded_hint(), None);
        assert_eq!(
            opts(Some("git commit, ls -la"), &[]).folded_hint().as_deref(),
            Some("git commit, ls -la")
        );
        assert_eq!(
            opts(None, &["Kitchen", "Hallway"]).folded_hint().as_deref(),
            Some("Kitchen, Hallway")
        );
        assert_eq!(
            opts(Some("Shell commands:"), &["Kitchen", "Hallway"]).folded_hint().as_deref(),
            Some("Shell commands: Kitchen, Hallway")
        );
    }

    #[test]
    fn folded_hint_ignores_blank_keywords() {
        // A list that is entirely whitespace must not turn an absent
        // hint into an empty prompt field on the wire.
        assert_eq!(opts(None, &["  ", ""]).folded_hint(), None);
        assert_eq!(opts(None, &[" Kitchen ", " "]).folded_hint().as_deref(), Some("Kitchen"));
    }

    #[test]
    fn default_options_are_inert() {
        // Backends receiving a default `TranscribeOptions` must send no
        // language, no prompt, and no keywords.
        let d = TranscribeOptions::default();
        assert!(d.lang_override.is_none());
        assert!(d.context_hint.is_none());
        assert!(d.keywords.is_empty());
        assert_eq!(d.folded_hint(), None);
    }
}
