// SPDX-License-Identifier: GPL-3.0-only
//! OpenAI streaming STT — a native WebSocket client against the
//! Realtime *transcription session* on `wss://api.openai.com/v1/realtime`,
//! driving `gpt-live-transcribe`.
//!
//! This is the live-preview twin of [`crate::openai`]: the batch backend
//! POSTs a finished WAV to `/v1/audio/transcriptions`, this one holds a
//! socket open and paints transcript deltas into the overlay as you
//! speak. It is only constructed when live preview is on (see
//! [`crate::factory::build_streaming_stt`]); with preview off the daemon
//! keeps using the cheaper batch path.
//!
//! Lifecycle:
//!
//! 1. Connect with `Authorization: Bearer <key>`.
//! 2. Send one `session.update` declaring `type: "transcription"`,
//!    24 kHz PCM input, the transcription model, the prompt, the
//!    configured language allow-list, and `turn_detection: null` — Fono
//!    has its own VAD and commits turns itself, so server-side turn
//!    detection would fight it.
//! 3. Stream `input_audio_buffer.append` events carrying base64 s16le
//!    PCM, resampled from the capture rate to 24 kHz.
//! 4. Map `conversation.item.input_audio_transcription.delta` to
//!    [`crate::streaming::UpdateLane::Preview`] and `.completed` to
//!    [`crate::streaming::UpdateLane::Finalize`].
//! 5. On EOF — the user released the hotkey — send one
//!    `input_audio_buffer.commit` so the server closes the turn and emits
//!    its final transcript. A mid-utterance [`StreamFrame::SegmentBoundary`]
//!    deliberately does *not* commit: the model transcribes each turn in
//!    isolation, so one commit per VAD pause splits a single dictation
//!    into unrelated fragments. One dictation is one turn.
//!
//! Two documented gaps versus the batch backend, both handled here:
//!
//! - `gpt-live-transcribe` returns **no** detected language, no
//!   timestamps and no confidence scores. Every update therefore carries
//!   `language: None`, which downstream means "let the text speak for
//!   itself" — no "Reply in X." and no language-pinned TTS voice. Only
//!   `gpt-transcribe` reports `languages[]`, and it is honoured when the
//!   user has pinned that model — but, as on the batch path, only
//!   because we let it detect freely instead of handing it the
//!   configured list.
//! - The realtime price is roughly 4× the batch price per minute, which
//!   is why this path is opt-in behind live preview rather than the
//!   default for every dictation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use futures::stream::{BoxStream, StreamExt};
use futures::SinkExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::lang::LanguageSelection;
use crate::lang_cache::LanguageCache;
use crate::streaming::{StreamFrame, StreamingStt, TranscriptUpdate};

/// Realtime input sample rate. The transcription session declares its
/// own input format and the documented PCM rate is 24 kHz, so capture
/// audio (16 kHz) is resampled on the way out rather than declaring the
/// capture rate and hoping the server accepts it.
const WIRE_RATE: u32 = 24_000;

/// Default realtime transcription model.
pub const LIVE_MODEL: &str = "gpt-live-transcribe";

/// Candidate handshake URLs, tried in order. A transcription session
/// declares its model inside `session.update`, and both query-string
/// spellings appear in OpenAI's own material, so rather than bet the
/// whole live path on one guess we fall through to the second on a
/// handshake error.
fn handshake_urls(model: &str) -> [String; 2] {
    [
        "wss://api.openai.com/v1/realtime?intent=transcription".to_string(),
        format!("wss://api.openai.com/v1/realtime?model={model}"),
    ]
}

/// Map Fono's preview cadence onto the model's `delay` knob, which
/// trades transcript quality for how early partial text appears. A
/// tighter cadence means the user asked for snappier repainting.
#[must_use]
pub fn delay_for(cadence: Option<Duration>) -> &'static str {
    match cadence {
        // Finalize-only: nothing repaints mid-utterance, so spend the
        // extra audio context on accuracy.
        None => "high",
        Some(d) if d <= Duration::from_millis(300) => "minimal",
        Some(d) if d <= Duration::from_millis(600) => "low",
        Some(d) if d <= Duration::from_millis(1200) => "medium",
        Some(_) => "high",
    }
}

/// Build the one `session.update` event that configures the session.
/// Pure, so the wire shape is unit-testable without a socket.
///
/// `languages` is the configured allow-list, sent verbatim. The docs
/// recommend it precisely for audio that contains "more than one
/// expected language", and it is what stops the model wandering into
/// scripts the user never speaks. Unlike the batch path, naming the
/// languages here cannot produce a bogus verdict, because
/// `gpt-live-transcribe` returns no detected language at all.
/// An empty slice omits the field, leaving the model to auto-detect.
#[must_use]
pub fn session_update(
    prompt: Option<&str>,
    languages: &[String],
    delay: &str,
) -> serde_json::Value {
    let mut transcription = serde_json::Map::new();
    transcription.insert("model".into(), json!(LIVE_MODEL));
    transcription.insert("delay".into(), json!(delay));
    if let Some(p) = prompt {
        transcription.insert("prompt".into(), json!(p));
    }
    if !languages.is_empty() {
        // Plural only: the model rejects the singular `language` field,
        // and a rejected `session.update` discards the whole config.
        transcription.insert("languages".into(), json!(languages));
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": WIRE_RATE },
                    "transcription": serde_json::Value::Object(transcription),
                    // Fono's own VAD decides when a turn ends and sends
                    // `input_audio_buffer.commit`; server-side turn
                    // detection would cut turns underneath it.
                    "turn_detection": serde_json::Value::Null,
                }
            }
        }
    })
}

/// Subset of the realtime server-event envelope. Every field is
/// `serde(default)` and unknown fields are ignored, so additive schema
/// drift cannot break the parser.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct ServerEvent {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub item_id: String,
    /// Incremental text on a `.delta` event.
    #[serde(default)]
    pub delta: String,
    /// Whole-turn text on a `.completed` event.
    #[serde(default)]
    pub transcript: String,
    /// `gpt-transcribe` only, and empty when it could not make a
    /// reliable prediction. `gpt-live-transcribe` never sends it.
    #[serde(default)]
    pub languages: Option<Vec<DetectedLang>>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct DetectedLang {
    #[serde(default)]
    pub code: String,
}

impl ServerEvent {
    /// Reportable language for a completed turn, or `None` whenever the
    /// model gave us no verdict we trust. Mirrors
    /// [`crate::openai`]'s `resolve_language`: a single confident code
    /// is reported, an empty array (no reliable prediction) and genuine
    /// code-switching both report nothing.
    #[must_use]
    pub fn reported_language(&self) -> Option<String> {
        match self.languages.as_deref()? {
            [one] => Some(crate::lang::whisper_lang_to_code(&one.code)),
            _ => None,
        }
    }
}

/// Streaming OpenAI client. Implements [`StreamingStt`] over the
/// realtime transcription session.
pub struct OpenAiStreaming {
    api_key: String,
    model: String,
    languages: Vec<String>,
    prompts: HashMap<String, String>,
    cadence: Option<Duration>,
    lang_cache: Arc<LanguageCache>,
}

impl OpenAiStreaming {
    #[must_use]
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            languages: Vec::new(),
            prompts: HashMap::new(),
            cadence: None,
            lang_cache: LanguageCache::global(),
        }
    }

    /// Builder: language allow-list. See [`LanguageSelection`].
    #[must_use]
    pub fn with_languages(mut self, codes: Vec<String>) -> Self {
        self.languages = codes;
        self
    }

    /// Builder: per-language initial-prompt map, sent as the session's
    /// `prompt` when exactly one language is pinned.
    #[must_use]
    pub fn with_prompts(mut self, prompts: HashMap<String, String>) -> Self {
        self.prompts = prompts;
        self
    }

    /// Accepted for builder parity with the other cloud backends and
    /// ignored — the model takes the whole allow-list natively, so
    /// there is no mismatch to rerun on.
    #[must_use]
    pub const fn with_cloud_rerun_on_mismatch(self, _on: bool) -> Self {
        self
    }

    /// Builder: preview cadence, mapped onto the model's `delay` knob.
    #[must_use]
    pub const fn with_preview_cadence(mut self, cadence: Option<Duration>) -> Self {
        self.cadence = cadence;
        self
    }

    /// Builder: inject a specific language cache (tests + bench).
    #[must_use]
    pub fn with_lang_cache(mut self, cache: Arc<LanguageCache>) -> Self {
        self.lang_cache = cache;
        self
    }

    fn effective_selection(&self, lang_override: Option<&str>) -> LanguageSelection {
        LanguageSelection::from_config(&self.languages).with_override(lang_override)
    }
}

/// Minimum audio the server accepts in one commit: 100 ms at the wire
/// rate. Committing less earns an `input_audio_buffer_commit_empty`
/// error and no transcript.
const MIN_COMMIT_SAMPLES: usize = (WIRE_RATE / 10) as usize;

/// Silence to append before committing `pending` samples, or `None` when
/// the buffer already clears the server's 100 ms floor.
///
/// Very short utterances are real — "yes", "stop" — so padding them up to
/// the floor keeps a transcript we would otherwise have to discard.
#[must_use]
pub const fn silence_padding(pending: usize) -> Option<usize> {
    if pending >= MIN_COMMIT_SAMPLES {
        None
    } else {
        Some(MIN_COMMIT_SAMPLES - pending)
    }
}

/// How long the reader waits for the last committed turn's transcript
/// after the audio has run out. A rejected commit or a dropped turn
/// would otherwise leave the overlay showing the last preview frame for
/// ever; this bounds the worst case at a visible-but-tolerable pause.
const FINAL_TRANSCRIPT_GRACE: Duration = Duration::from_secs(5);

/// Stand-in for "wait indefinitely" while audio is still arriving. A
/// concrete duration keeps both socket reads in one `select!` arm, which
/// the borrow checker requires.
const NO_DEADLINE: Duration = Duration::from_secs(86_400);

/// Convert f32 PCM in [-1.0, 1.0] to little-endian s16 bytes — the
/// `audio/pcm` wire format. Shared with the Deepgram streaming client
/// in spirit; kept local to avoid a cross-feature dependency.
fn f32_to_s16le_bytes(pcm: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for &s in pcm {
        #[allow(clippy::cast_possible_truncation)]
        let i = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
        out.extend_from_slice(&i.to_le_bytes());
    }
    out
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the transcription socket, trying each candidate URL in turn.
async fn connect(api_key: &str, model: &str) -> Result<WsStream> {
    let mut last: Option<anyhow::Error> = None;
    for url in handshake_urls(model) {
        let mut request = url
            .as_str()
            .into_client_request()
            .with_context(|| format!("building OpenAI realtime WS request for {url}"))?;
        let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .context("OpenAI API key contains non-ASCII bytes")?;
        request.headers_mut().insert("Authorization", value);
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(e) => {
                tracing::debug!("openai realtime handshake failed for {url}: {e:#}");
                last = Some(anyhow::Error::new(e).context(format!("connecting to {url}")));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no OpenAI realtime handshake URL to try")))
}

#[async_trait]
impl StreamingStt for OpenAiStreaming {
    #[allow(clippy::too_many_lines)]
    async fn stream_transcribe(
        &self,
        mut frames: BoxStream<'static, StreamFrame>,
        sample_rate: u32,
        lang: Option<String>,
    ) -> Result<BoxStream<'static, TranscriptUpdate>> {
        let selection = self.effective_selection(lang.as_deref());
        let codes = selection.codes().to_vec();
        // A per-language prompt is only meaningful once a single
        // language is pinned; while the model auto-detects, or across an
        // allow-list, it would bias the classifier.
        let prompt = match codes.as_slice() {
            [only] => self.prompts.get(only).cloned(),
            _ => None,
        };
        if !self.model.trim().eq_ignore_ascii_case(LIVE_MODEL) {
            tracing::info!(
                "live transcript preview runs {LIVE_MODEL:?}; batch dictation still uses {:?}",
                self.model,
            );
        }

        let ws = connect(&self.api_key, LIVE_MODEL).await?;
        let (mut ws_write, mut ws_read) = ws.split();

        let update = session_update(prompt.as_deref(), &codes, delay_for(self.cadence));
        ws_write
            .send(Message::Text(update.to_string()))
            .await
            .context("sending OpenAI realtime session.update")?;

        let (tx, rx) = mpsc::unbounded_channel::<TranscriptUpdate>();
        let lang_cache = Arc::clone(&self.lang_cache);
        let started = Instant::now();
        // Reader shutdown handshake. The server never closes a
        // transcription session on its own, so the reader cannot simply
        // loop until the socket ends — it has to be told how many turns
        // were committed and stop once it has seen them all. The writer
        // sends that count when the audio stream is exhausted.
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<usize>();

        // Reader task: translate server events into overlay updates.
        let tx_reader = tx.clone();
        let read_handle = tokio::spawn(async move {
            // Turns are identified by `item_id` and their completion
            // order is not guaranteed, so segment indices are assigned
            // per item in arrival order and the accumulated delta text
            // is kept alongside.
            let mut segments: HashMap<String, (u32, String)> = HashMap::new();
            let mut next_index: u32 = 0;
            // Committed turns still owed a `.completed` event, known only
            // once the writer has seen EOF.
            let mut expected: Option<usize> = None;
            let mut completed: usize = 0;
            loop {
                if expected.is_some_and(|n| completed >= n) {
                    break;
                }
                // While audio is still flowing there is no deadline: the
                // user may pause mid-sentence for as long as they like.
                // Once the writer has reported the commit count, the tail
                // is bounded.
                let wait = if expected.is_some() { FINAL_TRANSCRIPT_GRACE } else { NO_DEADLINE };
                let next = tokio::select! {
                    biased;
                    r = &mut done_rx, if expected.is_none() => {
                        expected = Some(r.unwrap_or(0));
                        continue;
                    }
                    m = tokio::time::timeout(wait, ws_read.next()) => if let Ok(m) = m {
                        m
                    } else {
                        // A rejected commit or a dropped turn: let the
                        // overlay move on rather than hang.
                        tracing::warn!(
                            "openai realtime: no final transcript within {}s; \
                             giving up on {} outstanding turn(s)",
                            FINAL_TRANSCRIPT_GRACE.as_secs(),
                            expected.unwrap_or(0).saturating_sub(completed),
                        );
                        break;
                    },
                };
                let Some(msg) = next else { break };
                let payload = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(reason)) => {
                        tracing::info!("openai realtime WS closed: {reason:?}");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!("openai realtime WS read error: {e:#}");
                        break;
                    }
                };
                let event: ServerEvent = match serde_json::from_str(&payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::debug!(
                            "openai realtime: ignoring unparseable event: {e}; body={payload}"
                        );
                        continue;
                    }
                };
                let elapsed = started.elapsed();
                match event.kind.as_str() {
                    "conversation.item.input_audio_transcription.delta" => {
                        let entry = segments.entry(event.item_id.clone()).or_insert_with(|| {
                            let idx = next_index;
                            next_index = next_index.saturating_add(1);
                            (idx, String::new())
                        });
                        entry.1.push_str(&event.delta);
                        let upd = TranscriptUpdate::preview(entry.0, &entry.1, elapsed);
                        if tx_reader.send(upd).is_err() {
                            break;
                        }
                    }
                    "conversation.item.input_audio_transcription.completed" => {
                        // Count the turn, not the text: an empty
                        // transcript still settles the commit we are
                        // waiting on, and treating it otherwise would
                        // stall the shutdown until the grace expires.
                        completed = completed.saturating_add(1);
                        let (index, buffered) =
                            segments.remove(&event.item_id).unwrap_or_else(|| {
                                let idx = next_index;
                                next_index = next_index.saturating_add(1);
                                (idx, String::new())
                            });
                        let text = if event.transcript.is_empty() {
                            buffered
                        } else {
                            event.transcript.clone()
                        };
                        if text.is_empty() {
                            continue;
                        }
                        let language = event.reported_language();
                        if let Some(code) = language.as_deref() {
                            lang_cache.record(crate::openai::BACKEND_KEY, code);
                        }
                        let upd = TranscriptUpdate::finalize(index, &text, elapsed)
                            .with_language(language);
                        if tx_reader.send(upd).is_err() {
                            break;
                        }
                    }
                    "error" => {
                        tracing::warn!("openai realtime error event: {:?}", event.error);
                    }
                    other => {
                        tracing::trace!("openai realtime: ignoring event {other:?}");
                    }
                }
            }
        });

        // Writer task: resample, encode, and pump audio; commit turns.
        let mut resampler = if sample_rate == WIRE_RATE {
            None
        } else {
            Some(
                fono_audio::resample::Resampler::new(sample_rate, WIRE_RATE)
                    .with_context(|| format!("resampler {sample_rate} -> {WIRE_RATE}"))?,
            )
        };
        tokio::spawn(async move {
            let commit = json!({ "type": "input_audio_buffer.commit" }).to_string();
            // Samples appended so far. The server rejects a commit
            // carrying less than 100 ms of audio, which happens whenever
            // the hotkey is released without anything being said.
            let mut pending: usize = 0;
            // Turns committed, so the reader knows how many `.completed`
            // events it is still owed once the audio runs out.
            let mut commits: usize = 0;
            while let Some(frame) = frames.next().await {
                match frame {
                    StreamFrame::Pcm(chunk) => {
                        let wire = match resampler.as_mut() {
                            Some(r) => r.process(&chunk),
                            None => chunk,
                        };
                        if wire.is_empty() {
                            continue;
                        }
                        pending += wire.len();
                        let audio = BASE64_STANDARD.encode(f32_to_s16le_bytes(&wire));
                        let event = json!({ "type": "input_audio_buffer.append", "audio": audio })
                            .to_string();
                        if let Err(e) = ws_write.send(Message::Text(event)).await {
                            tracing::warn!("openai realtime WS send error: {e:#}");
                            break;
                        }
                    }
                    StreamFrame::SegmentBoundary => {
                        // Deliberately NOT a commit. A commit closes the
                        // turn, and the model transcribes each turn in
                        // isolation — so committing on every VAD pause
                        // chopped one dictation into several independent
                        // fragments with no shared context, which showed
                        // up as broken agreement, restarted capitalisation
                        // and duplicated words at the seams. Partial text
                        // keeps arriving as `.delta` without any commit,
                        // so the overlay loses nothing by waiting.
                        tracing::trace!("openai realtime: segment boundary, holding the turn open");
                    }
                    StreamFrame::Eof => {
                        // Turn detection is off, so this commit is the only
                        // thing that makes the server finish the turn and
                        // emit its final transcript.
                        if pending == 0 {
                            // Nothing was said — committing would only
                            // earn an `input_audio_buffer_commit_empty`.
                            tracing::trace!("openai realtime: skipping commit, no pending audio");
                            break;
                        }
                        if let Some(pad) = silence_padding(pending) {
                            // Real but very short utterance. Topping it
                            // up with silence keeps the transcript we
                            // would otherwise have to throw away.
                            let audio = BASE64_STANDARD.encode(f32_to_s16le_bytes(&vec![0.0; pad]));
                            let event =
                                json!({ "type": "input_audio_buffer.append", "audio": audio })
                                    .to_string();
                            if let Err(e) = ws_write.send(Message::Text(event)).await {
                                tracing::warn!("openai realtime WS send error: {e:#}");
                                break;
                            }
                        }
                        // No reset needed: EOF is terminal, one turn only.
                        if let Err(e) = ws_write.send(Message::Text(commit.clone())).await {
                            tracing::warn!("openai realtime commit failed: {e:#}");
                            break;
                        }
                        commits = commits.saturating_add(1);
                        break;
                    }
                }
            }
            // Hand the reader its stopping condition, then let it drain
            // the final transcripts. Without this the reader would block
            // on a socket the server never closes and the overlay would
            // sit on the last preview frame for ever.
            let _ = done_tx.send(commits);
            let _ = read_handle.await;
            let _ = ws_write.close().await;
            drop(tx);
        });

        let out: BoxStream<'static, TranscriptUpdate> = UnboundedReceiverStream::new(rx).boxed();
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "openai"
    }

    fn is_local(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_tracks_the_preview_cadence() {
        assert_eq!(delay_for(Some(Duration::from_millis(250))), "minimal");
        assert_eq!(delay_for(Some(Duration::from_millis(500))), "low");
        assert_eq!(delay_for(Some(Duration::from_millis(700))), "medium");
        assert_eq!(delay_for(Some(Duration::from_secs(2))), "high");
        // Finalize-only repaints nothing mid-utterance, so buy accuracy.
        assert_eq!(delay_for(None), "high");
    }

    #[test]
    fn session_update_declares_a_transcription_session() {
        let v = session_update(None, &[], "low");
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["type"], "transcription");
        let input = &v["session"]["audio"]["input"];
        assert_eq!(input["format"]["type"], "audio/pcm");
        assert_eq!(input["format"]["rate"], 24_000);
        // Fono commits turns itself; server VAD would fight it.
        assert!(input["turn_detection"].is_null());
        let t = &input["transcription"];
        assert_eq!(t["model"], LIVE_MODEL);
        assert_eq!(t["delay"], "low");
        assert!(t.get("prompt").is_none());
        // No allow-list configured ⇒ let the model auto-detect.
        assert!(t.get("languages").is_none());
    }

    #[test]
    fn session_update_always_names_the_live_model() {
        // The batch model is configured separately and rejects `delay`
        // with `invalid_value` — and the API rejects the *whole*
        // session.update when it does, taking the prompt down with it.
        // Live preview therefore always runs the live model, whatever is
        // pinned for committed turns.
        let v = session_update(Some("Dictation."), &[], "low");
        let t = &v["session"]["audio"]["input"]["transcription"];
        assert_eq!(t["model"], LIVE_MODEL);
        assert_eq!(t["delay"], "low");
        assert_eq!(t["prompt"], "Dictation.");
    }

    #[test]
    fn session_update_sends_the_configured_allow_list() {
        // Regression guard for transcripts wandering into Arabic and CJK
        // mid-Romanian: the model needs to be told which languages the
        // speaker actually uses. Order is the user's config order.
        let codes = vec!["en".to_string(), "ro".to_string()];
        let v = session_update(None, &codes, "low");
        let t = &v["session"]["audio"]["input"]["transcription"];
        assert_eq!(t["languages"], json!(["en", "ro"]));
        // Plural only: sending both fields is rejected outright, and a
        // rejected session.update discards the whole configuration.
        assert!(t.get("language").is_none());
    }

    #[test]
    fn session_update_sends_a_single_pinned_language_as_a_list() {
        let codes = vec!["ro".to_string()];
        let v = session_update(None, &codes, "high");
        let t = &v["session"]["audio"]["input"]["transcription"];
        assert_eq!(t["languages"], json!(["ro"]));
        assert!(t.get("language").is_none());
    }

    #[test]
    fn parses_delta_event() {
        let body = r#"{
            "type": "conversation.item.input_audio_transcription.delta",
            "item_id": "item_003",
            "content_index": 0,
            "delta": "Hello,"
        }"#;
        let e: ServerEvent = serde_json::from_str(body).expect("parse");
        assert_eq!(e.kind, "conversation.item.input_audio_transcription.delta");
        assert_eq!(e.item_id, "item_003");
        assert_eq!(e.delta, "Hello,");
        assert_eq!(e.reported_language(), None);
    }

    #[test]
    fn parses_completed_event_with_detected_language() {
        let body = r#"{
            "type": "conversation.item.input_audio_transcription.completed",
            "item_id": "item_003",
            "transcript": "Bonjour, pouvez-vous m'entendre ?",
            "languages": [{ "code": "fr" }]
        }"#;
        let e: ServerEvent = serde_json::from_str(body).expect("parse");
        assert_eq!(e.transcript, "Bonjour, pouvez-vous m'entendre ?");
        assert_eq!(e.reported_language().as_deref(), Some("fr"));
    }

    #[test]
    fn empty_language_array_reports_nothing() {
        // Documented signal for "no reliable prediction". Reporting a
        // guess here is what pins a conversation to the wrong language.
        let body = r#"{"type":"x","transcript":"hi","languages":[]}"#;
        let e: ServerEvent = serde_json::from_str(body).expect("parse");
        assert_eq!(e.reported_language(), None);
    }

    #[test]
    fn code_switching_reports_nothing() {
        let body = r#"{"type":"x","languages":[{"code":"en"},{"code":"ro"}]}"#;
        let e: ServerEvent = serde_json::from_str(body).expect("parse");
        assert_eq!(e.reported_language(), None);
    }

    #[test]
    fn missing_language_field_reports_nothing() {
        // `gpt-live-transcribe` never sends it at all.
        let body = r#"{"type":"x","transcript":"hi"}"#;
        let e: ServerEvent = serde_json::from_str(body).expect("parse");
        assert_eq!(e.reported_language(), None);
    }

    #[test]
    fn tolerates_unknown_events_and_extra_fields() {
        let body = r#"{"type":"session.updated","session":{"id":"sess_1"},"extra":42}"#;
        let e: ServerEvent = serde_json::from_str(body).expect("parse");
        assert_eq!(e.kind, "session.updated");
        assert!(e.transcript.is_empty());
    }

    #[test]
    fn handshake_urls_cover_both_spellings() {
        let urls = handshake_urls("gpt-live-transcribe");
        assert!(urls[0].starts_with("wss://api.openai.com/v1/realtime?"));
        assert!(urls[0].contains("intent=transcription"));
        assert!(urls[1].contains("model=gpt-live-transcribe"));
    }

    #[test]
    fn silence_padding_tops_up_short_buffers() {
        // 2400 samples = 100 ms at 24 kHz.
        assert_eq!(MIN_COMMIT_SAMPLES, 2400);
        assert_eq!(silence_padding(0), Some(2400));
        assert_eq!(silence_padding(2399), Some(1));
        assert_eq!(silence_padding(2400), None);
        assert_eq!(silence_padding(48_000), None);
    }

    #[test]
    fn f32_to_s16le_known_samples() {
        let bytes = f32_to_s16le_bytes(&[0.0, 1.0, -1.0, 2.5]);
        assert_eq!(&bytes[0..2], &[0x00, 0x00]);
        assert_eq!(&bytes[2..4], &[0xFF, 0x7F]);
        assert_eq!(&bytes[4..6], &[0x01, 0x80]);
        // Out of range must clamp, not wrap.
        assert_eq!(&bytes[6..8], &[0xFF, 0x7F]);
    }

    #[test]
    fn builder_captures_state() {
        let s = OpenAiStreaming::new("sk-test", "whisper-1")
            .with_languages(vec!["en".into(), "ro".into()])
            .with_cloud_rerun_on_mismatch(true)
            .with_preview_cadence(Some(Duration::from_millis(700)));
        assert_eq!(s.languages, vec!["en", "ro"]);
        assert_eq!(s.name(), "openai");
        assert!(!s.is_local());
        assert_eq!(delay_for(s.cadence), "medium");
    }
}
