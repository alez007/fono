// SPDX-License-Identifier: GPL-3.0-only
//! OpenAI STT backend.
//!
//! Covers three generations of the `/v1/audio/transcriptions` endpoint,
//! which differ enough on the wire that a capability table
//! ([`caps_for`]) is cheaper than three backends:
//!
//! | model | `response_format` | language field | `keywords[]` | per-segment scores |
//! |---|---|---|---|---|
//! | `gpt-transcribe`, `gpt-live-transcribe` | omit | `languages[]` (plural) | yes | no |
//! | `gpt-4o-transcribe`, `gpt-4o-mini-transcribe` | omit | `language` | no | no |
//! | `whisper-1` | `verbose_json` | `language` | no | yes |
//!
//! The plural `languages[]` field is why this backend no longer needs a
//! rerun / rerank lane: the model accepts the whole allow-list up front
//! and code-switches inside a single utterance, so a Romanian/English
//! speaker cannot be mis-tagged by a first-pass guess. Sending both
//! `language` and `languages` is a hard API error, hence the table.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;

use crate::lang::LanguageSelection;
use crate::lang_cache::LanguageCache;
use crate::traits::{SpeechToText, TranscribeOptions, Transcription};

const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const OPENAI_MODELS_ENDPOINT: &str = "https://api.openai.com/v1/models";
const DEFAULT_MODEL: &str = "gpt-transcribe";
pub(crate) const BACKEND_KEY: &str = "openai";

/// Mean per-segment `avg_logprob` below which we stop believing the
/// `whisper-1` language echo and report `None` instead. A wrong
/// language tag is worse than no tag: downstream it selects the TTS
/// voice and injects "Reply in X." into the assistant's system block,
/// so one bad guess steers the whole conversation. Threshold matches
/// `whisper_local.rs`'s `set_logprob_thold(-1.0)` and
/// [`crate::groq::is_hallucinated_segment`].
const LANG_LOGPROB_FLOOR: f32 = -1.0;

/// Wire differences between the OpenAI transcription model generations.
/// See the module docstring for the resolved table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelCaps {
    /// Value for the `response_format` form field, or `None` to omit it
    /// (the default `json` shape). `whisper-1` needs `verbose_json`
    /// because that is the only shape carrying `language` and the
    /// per-segment scores; every `gpt-*-transcribe` model **rejects**
    /// `verbose_json` outright.
    response_format: Option<&'static str>,
    /// `true` → plural `languages[]`, which lets the model code-switch
    /// between every configured language. `false` → a single
    /// `language` field, so an allow-list of two or more degrades to
    /// unconstrained auto-detect.
    plural_languages: bool,
    /// `true` → the model accepts a `keywords[]` list of literal terms
    /// (names, device labels, shell fragments). On models without it,
    /// keywords are folded into `prompt` instead.
    keywords: bool,
}

const CAPS_WHISPER: ModelCaps =
    ModelCaps { response_format: Some("verbose_json"), plural_languages: false, keywords: false };
const CAPS_GPT_4O: ModelCaps =
    ModelCaps { response_format: None, plural_languages: false, keywords: false };
const CAPS_GPT: ModelCaps =
    ModelCaps { response_format: None, plural_languages: true, keywords: true };

/// Resolve the wire capabilities for a model name.
///
/// Unknown names fall through to [`CAPS_GPT`] on the assumption that
/// future OpenAI transcription models follow the current generation.
/// `whisper*` and `*4o*` are pinned explicitly because both predate
/// `languages[]`.
fn caps_for(model: &str) -> ModelCaps {
    let m = model.trim().to_ascii_lowercase();
    if m.starts_with("whisper") {
        CAPS_WHISPER
    } else if m.contains("4o") {
        CAPS_GPT_4O
    } else {
        CAPS_GPT
    }
}

/// Drop keyword entries the API refuses. A single `<`, `>`, CR or LF in
/// any entry makes the API reject the **entire request**, so silently
/// dropping the offender is far better than failing the transcription —
/// keywords are only hints. Blank entries are dropped too.
fn sanitize_keywords(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty() && !k.contains(['<', '>', '\r', '\n']))
        .map(str::to_string)
        .collect()
}

pub struct OpenAiStt {
    api_key: String,
    model: String,
    client: reqwest::Client,
    languages: Vec<String>,
    lang_cache: Arc<LanguageCache>,
    prompts: HashMap<String, String>,
}

impl OpenAiStt {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_model(api_key, DEFAULT_MODEL)
    }
    pub fn with_model(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            client: crate::groq::warm_client(),
            languages: Vec::new(),
            lang_cache: LanguageCache::global(),
            prompts: HashMap::new(),
        }
    }

    #[must_use]
    pub fn with_languages(mut self, codes: Vec<String>) -> Self {
        self.languages = codes;
        self
    }

    /// Accepted for builder parity with the other cloud backends and
    /// ignored. `general.cloud_rerun_on_language_mismatch` described a
    /// rerun lane this backend no longer has: `gpt-transcribe` takes
    /// the whole allow-list natively via `languages[]`.
    #[must_use]
    pub const fn with_cloud_rerun_on_mismatch(self, _on: bool) -> Self {
        self
    }

    #[must_use]
    pub fn with_lang_cache(mut self, cache: Arc<LanguageCache>) -> Self {
        self.lang_cache = cache;
        self
    }

    /// Builder: per-language initial-prompt map. The prompt for the
    /// resolved language (if any) is included as the `prompt` form
    /// field on every request.
    #[must_use]
    pub fn with_prompts(mut self, prompts: HashMap<String, String>) -> Self {
        self.prompts = prompts;
        self
    }

    fn effective_selection(&self, lang_override: Option<&str>) -> LanguageSelection {
        LanguageSelection::from_config(&self.languages).with_override(lang_override)
    }

    fn prompt_for(&self, lang: Option<&str>) -> Option<&str> {
        lang.and_then(|l| self.prompts.get(l)).map(String::as_str)
    }

    /// One multipart POST. The only network path in this backend —
    /// both trait methods funnel through [`Self::run`] into here, so
    /// the two can no longer drift apart.
    async fn post(
        &self,
        wav: &[u8],
        caps: ModelCaps,
        selection: &LanguageSelection,
        prompt: Option<&str>,
        keywords: &[String],
    ) -> Result<Resp> {
        let part =
            multipart::Part::bytes(wav.to_vec()).file_name("audio.wav").mime_str("audio/wav")?;
        let mut form = multipart::Form::new().text("model", self.model.clone()).part("file", part);
        if let Some(rf) = caps.response_format {
            form = form.text("response_format", rf);
        }
        let codes = selection.codes();
        if caps.plural_languages {
            // Plural lane: hand the model every configured language at
            // once. Empty (auto) sends nothing.
            for code in codes {
                form = form.text("languages[]", code.clone());
            }
        } else if let [only] = codes {
            // Singular lane: one code is expressible, an allow-list is
            // not — fall through to unconstrained auto-detect.
            form = form.text("language", only.clone());
        }
        if let Some(p) = prompt {
            form = form.text("prompt", p.to_string());
        }
        for kw in keywords {
            form = form.text("keywords[]", kw.clone());
        }
        let res = self
            .client
            .post(OPENAI_ENDPOINT)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .context("openai POST failed")?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("openai STT {status}: {body}");
        }
        serde_json::from_str(&body).with_context(|| format!("parse openai response: {body}"))
    }

    /// Shared body for both trait methods.
    async fn run(
        &self,
        pcm: &[f32],
        sample_rate: u32,
        opts: &TranscribeOptions,
    ) -> Result<Transcription> {
        let wav = crate::groq::encode_wav(pcm, sample_rate);
        let selection = self.effective_selection(opts.lang_override.as_deref());
        let caps = caps_for(&self.model);

        // A per-language prompt is only meaningful once a single
        // language is pinned; sending one during auto-detect biases
        // the language classifier.
        let forced = match &selection {
            LanguageSelection::Forced(c) => Some(c.clone()),
            LanguageSelection::Auto | LanguageSelection::AllowList(_) => None,
        };
        let lang_prompt = self.prompt_for(forced.as_deref()).map(str::to_string);

        // Keywords ride their own field on the current generation and
        // fold into `prompt` on the older ones (which is where this
        // kind of hint lived before `keywords[]` existed).
        let keywords = sanitize_keywords(&opts.keywords);
        let (wire_keywords, hint): (&[String], Option<String>) = if caps.keywords {
            (&keywords, opts.context_hint.clone())
        } else {
            (&[], opts.folded_hint())
        };
        let prompt = crate::groq::merge_prompt(lang_prompt.as_deref(), hint.as_deref());

        if !caps.plural_languages && matches!(selection, LanguageSelection::AllowList(_)) {
            self.warn_no_plural_support();
        }

        let parsed = self.post(&wav, caps, &selection, prompt.as_deref(), wire_keywords).await?;

        let language = resolve_language(&parsed, forced.as_deref(), &selection);
        if let Some(code) = language.as_deref() {
            self.lang_cache.record(BACKEND_KEY, code);
        }
        Ok(Transcription { text: parsed.text, language, duration_ms: None })
    }

    /// One-shot nudge for users on a pre-`languages[]` model with two
    /// or more configured languages — the exact configuration where
    /// mid-sentence language switches get mis-tagged.
    fn warn_no_plural_support(&self) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if WARNED.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            "openai model {:?} predates the `languages[]` field, so the allow-list {:?} \
             degrades to unconstrained auto-detect; set `[stt.cloud].model = \
             \"gpt-transcribe\"` for reliable multi-language input",
            self.model,
            self.languages,
        );
    }
}

/// Response shape covering all three generations. `languages` is the
/// current generation's detected-language report; `language` is the
/// `whisper-1` echo; `segments` only ever arrives with `verbose_json`.
#[derive(Deserialize, Default)]
struct Resp {
    #[serde(default)]
    text: String,
    #[serde(default)]
    language: Option<String>,
    /// `Some(vec![])` is meaningful and distinct from `None`: the
    /// current-generation models return an **empty array** when they
    /// cannot make a reliable language prediction.
    #[serde(default)]
    languages: Option<Vec<DetectedLang>>,
    #[serde(default)]
    segments: Vec<crate::groq::GroqSegment>,
}

#[derive(Deserialize)]
struct DetectedLang {
    #[serde(default)]
    code: String,
}

impl Resp {
    /// Mean per-segment `avg_logprob`, or `None` when the response
    /// carries no scores (every generation except `whisper-1`).
    fn mean_logprob(&self) -> Option<f32> {
        let scored: Vec<f32> = self.segments.iter().filter_map(|s| s.avg_logprob).collect();
        if scored.is_empty() {
            None
        } else {
            Some(scored.iter().sum::<f32>() / scored.len() as f32)
        }
    }
}

/// Decide the language to report, returning `None` whenever the
/// provider did not give us a verdict we trust.
///
/// `None` is a first-class answer here, not a failure. Downstream it
/// means "let the text speak for itself": no "Reply in X." in the
/// assistant's system block, no language-specific TTS voice, no entry
/// written to the language cache. Guessing instead — which is what the
/// old `fallback_hint()` tail did — is how a single mis-detection used
/// to pin a whole conversation to the wrong language.
fn resolve_language(
    resp: &Resp,
    forced: Option<&str>,
    selection: &LanguageSelection,
) -> Option<String> {
    // We pinned the language ourselves, so there is nothing to detect.
    if let Some(code) = forced {
        return Some(code.to_ascii_lowercase());
    }
    if let Some(list) = resp.languages.as_deref() {
        return match list {
            [one] => Some(crate::lang::whisper_lang_to_code(&one.code)),
            [] => {
                tracing::debug!("openai reported no reliable language; leaving it unset");
                None
            }
            many => {
                // Genuine code-switching. Any single label would be
                // wrong, and the text already carries both languages.
                tracing::debug!(
                    "openai detected {} languages in one utterance; leaving it unset",
                    many.len()
                );
                None
            }
        };
    }
    let raw = resp.language.as_deref()?;
    let detected = crate::lang::whisper_lang_to_code(raw);
    // `verbose_json` gives us a confidence signal; use it. No segments
    // (very short clip) means no signal, so keep the echo.
    if let Some(mean) = resp.mean_logprob() {
        if mean < LANG_LOGPROB_FLOOR {
            tracing::debug!(
                "openai language {detected:?} came back with mean avg_logprob {mean:.2} \
                 (< {LANG_LOGPROB_FLOOR}); treating it as undecided"
            );
            return None;
        }
    }
    if !selection.contains(&detected) {
        tracing::info!(
            "openai detected {raw:?} (normalised {detected:?}) outside the configured \
             allow-list; leaving the language unset rather than guessing a peer"
        );
        return None;
    }
    Some(detected)
}

#[async_trait]
impl SpeechToText for OpenAiStt {
    async fn transcribe(
        &self,
        pcm: &[f32],
        sample_rate: u32,
        lang: Option<&str>,
    ) -> Result<Transcription> {
        let opts = TranscribeOptions {
            lang_override: lang.map(str::to_string),
            ..TranscribeOptions::default()
        };
        self.run(pcm, sample_rate, &opts).await
    }

    async fn transcribe_with_opts(
        &self,
        pcm: &[f32],
        sample_rate: u32,
        opts: &TranscribeOptions,
    ) -> Result<Transcription> {
        self.run(pcm, sample_rate, opts).await
    }

    fn name(&self) -> &'static str {
        "openai"
    }

    async fn prewarm(&self) -> Result<()> {
        let res = self
            .client
            .get(OPENAI_MODELS_ENDPOINT)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .context("openai prewarm")?;
        let _ = res.bytes().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detected(codes: &[&str]) -> Resp {
        Resp {
            languages: Some(
                codes.iter().map(|c| DetectedLang { code: (*c).to_string() }).collect(),
            ),
            ..Resp::default()
        }
    }

    fn whisper_resp(language: &str, logprobs: &[f32]) -> Resp {
        Resp {
            language: Some(language.to_string()),
            segments: logprobs
                .iter()
                .map(|lp| crate::groq::GroqSegment {
                    text: String::new(),
                    avg_logprob: Some(*lp),
                    no_speech_prob: Some(0.1),
                })
                .collect(),
            ..Resp::default()
        }
    }

    #[test]
    fn caps_split_by_model_generation() {
        // whisper-1 is the only model that both needs and tolerates
        // `verbose_json` — sending it to a gpt-* model is a hard error.
        assert_eq!(caps_for("whisper-1").response_format, Some("verbose_json"));
        assert_eq!(caps_for("gpt-transcribe").response_format, None);
        assert_eq!(caps_for("gpt-4o-transcribe").response_format, None);
        assert_eq!(caps_for("gpt-4o-mini-transcribe").response_format, None);
        // Only the current generation takes plural languages + keywords.
        assert!(caps_for("gpt-transcribe").plural_languages);
        assert!(caps_for("gpt-live-transcribe").plural_languages);
        assert!(!caps_for("gpt-4o-transcribe").plural_languages);
        assert!(!caps_for("whisper-1").plural_languages);
        assert!(caps_for("gpt-transcribe").keywords);
        assert!(!caps_for("whisper-1").keywords);
        // Case and whitespace are not a trap.
        assert_eq!(caps_for(" WHISPER-1 "), CAPS_WHISPER);
        // Unknown future models get the current generation's shape.
        assert_eq!(caps_for("gpt-6-transcribe"), CAPS_GPT);
    }

    #[test]
    fn keywords_that_would_reject_the_request_are_dropped() {
        // The API rejects the whole request on any of these, so a
        // hint is not worth a failed transcription.
        let raw = vec![
            "Living Room".to_string(),
            "  ".to_string(),
            "bad<angle".to_string(),
            "bad>angle".to_string(),
            "two\nlines".to_string(),
            "carriage\rreturn".to_string(),
            "  Kitchen  ".to_string(),
        ];
        assert_eq!(sanitize_keywords(&raw), vec!["Living Room", "Kitchen"]);
    }

    #[test]
    fn single_detected_language_is_trusted() {
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        assert_eq!(resolve_language(&detected(&["ro"]), None, &sel), Some("ro".to_string()));
        // Full names and alpha-3 normalise on the way out.
        assert_eq!(resolve_language(&detected(&["ron"]), None, &sel), Some("ro".to_string()));
    }

    #[test]
    fn empty_detected_languages_yields_none() {
        // The documented "I could not make a reliable prediction"
        // signal. The old code guessed the first allow-list entry here.
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        assert_eq!(resolve_language(&detected(&[]), None, &sel), None);
    }

    #[test]
    fn code_switched_utterance_yields_none() {
        // Both labels are correct, so neither is usable as *the*
        // language; the text carries the mixture already.
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        assert_eq!(resolve_language(&detected(&["ro", "en"]), None, &sel), None);
    }

    #[test]
    fn forced_language_is_reported_verbatim() {
        // Nothing was detected — we pinned it — so confidence gating
        // must not second-guess the caller.
        let sel = LanguageSelection::Forced("ro".into());
        assert_eq!(resolve_language(&Resp::default(), Some("RO"), &sel), Some("ro".to_string()));
    }

    #[test]
    fn whisper_echo_survives_confident_scores() {
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        let resp = whisper_resp("romanian", &[-0.2, -0.3]);
        assert_eq!(resolve_language(&resp, None, &sel), Some("ro".to_string()));
    }

    #[test]
    fn whisper_echo_dropped_on_low_confidence() {
        // Silence-shaped decode: the language tag is as unreliable as
        // the text, so don't let it pick a TTS voice.
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        let resp = whisper_resp("romanian", &[-1.6, -1.4]);
        assert_eq!(resolve_language(&resp, None, &sel), None);
    }

    #[test]
    fn whisper_echo_kept_when_no_segments_to_score() {
        // Conservative: no signal is not the same as a bad signal.
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        let resp = whisper_resp("english", &[]);
        assert_eq!(resolve_language(&resp, None, &sel), Some("en".to_string()));
    }

    #[test]
    fn out_of_allow_list_detection_yields_none() {
        // Previously this triggered a per-peer rerun; now we simply
        // decline to report a language we don't believe.
        let sel = LanguageSelection::AllowList(vec!["en".into(), "ro".into()]);
        let resp = whisper_resp("bulgarian", &[-0.3]);
        assert_eq!(resolve_language(&resp, None, &sel), None);
    }

    #[test]
    fn auto_selection_accepts_any_detection() {
        let resp = whisper_resp("bulgarian", &[-0.3]);
        assert_eq!(resolve_language(&resp, None, &LanguageSelection::Auto), Some("bg".to_string()));
    }
}
