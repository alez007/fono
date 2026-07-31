// SPDX-License-Identifier: GPL-3.0-only
//! One benchmark utterance, taken through the production assistant turn.
//!
//! The whole point of this module is how little it does. Everything that
//! decides whether a command works — the prompt composition, the room hint,
//! the blank-argument trimming, the schema check, the retry ladder, the vendor
//! admission rungs, the readback — belongs to
//! [`crate::assistant::run_assistant_turn`] and to [`crate::actions`], and none
//! of it is re-created here. A second rendering of any of it would be a
//! benchmark that measures something Fono does not do, which is the failure
//! this harness exists to avoid.
//!
//! What is different from a spoken turn is only what a benchmark cannot have:
//! there is no microphone, so the text arrives on
//! [`AssistantTurnInputs::pre_transcribed`] — the same field the live-streaming
//! path uses when it has already transcribed the audio itself — and there is no
//! speaker, so `tts` is `None`, which the pump already understands as a
//! text-only turn. Both are existing, exercised paths rather than a bypass.
//!
//! **If a text turn ever needs behaviour the voice turn does not have, add it
//! to the pump, not here.** The moment this file grows its own request
//! building, the numbers stop being about Fono.

use std::sync::Arc;

use anyhow::{Context, Result};
use fono_assistant::ConversationHistory;
use fono_core::config::Config;
use fono_core::paths::Paths;
use fono_core::Secrets;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::assistant::{run_assistant_turn, AssistantSessionState, AssistantTurnInputs};

/// What one utterance produced, as observed from outside the pump.
///
/// Everything here comes from the pump's own end-of-turn record
/// ([`AssistantSessionState::last_turn`]), written under the same lock as the
/// history push it mirrors. It is deliberately **not** read back out of
/// conversation history: a turn that used a tool clears history when it ends
/// (`forget_after_action`), so scraping the buffer afterwards found nothing
/// and reported every run as zero tool calls — which silently zeroed the
/// routing rate, disabled the forbidden-argument check and left the reply
/// unjudged, on traces that plainly showed a call per turn.
#[derive(Debug, Clone)]
pub struct TurnObservation {
    /// The spoken reply, as the user would have heard it.
    pub reply: String,
    /// Every tool call the model issued, in order. The first is what scores
    /// routing; the rest are the recovery ladder at work.
    pub calls: Vec<ObservedCall>,
    /// Wall-clock time for the whole turn.
    pub elapsed: std::time::Duration,
    /// Whether the pump reported having produced anything at all.
    pub produced: bool,
    /// Whether the reply was cut off part way through.
    pub aborted: bool,
}

/// One tool call and what the executor made of it.
#[derive(Debug, Clone)]
pub struct ObservedCall {
    pub name: String,
    /// Raw JSON string exactly as the model emitted it — never re-serialised,
    /// because a model that emits invalid JSON is a finding, not a parse error.
    pub arguments: String,
    /// The executor's own prose verdict, which is also what the model read
    /// before deciding whether to try again.
    pub outcome: Option<String>,
    /// Whether the executor called it a failure.
    pub failed: bool,
}

impl TurnObservation {
    /// The model's opening move. This is what "did it route correctly on the
    /// first try" is scored against, and it must never be confused with the
    /// last call — the gap between the two is the measured value of the
    /// recovery ladder.
    #[must_use]
    pub fn first_call(&self) -> Option<&ObservedCall> {
        self.calls.first()
    }

    /// True when a repeat could have doubled an effect in the world: the model
    /// called again after a call that did something.
    ///
    /// A repeat after a refusal is not this. Asked to make the office two
    /// degrees warmer, a model wrote the temperature against a device Home
    /// Assistant could not find, was told so, and tried once more. Nothing
    /// moved either time, and scoring that as four degrees would charge the
    /// model for the one thing the retry ladder exists to do.
    #[must_use]
    pub fn doubled(&self) -> bool {
        let all_but_last = self.calls.len().saturating_sub(1);
        self.calls.iter().take(all_but_last).any(|c| !c.failed)
    }
}

/// Everything the driver needs that does not change between utterances.
///
/// Held across a whole run so the assistant backend — which for a local model
/// means a loaded set of weights — is built once. Rebuilding it per utterance
/// would make every measured latency a model-load time.
pub struct TurnDriver {
    config: Config,
    paths: Paths,
    assistant: Arc<dyn fono_assistant::Assistant>,
    /// Kept alive so the pump's sends do not fail; a benchmark has no FSM to
    /// drive, and the pump does not require anyone to be listening.
    action_tx: mpsc::UnboundedSender<fono_hotkey::HotkeyAction>,
    _action_rx: mpsc::UnboundedReceiver<fono_hotkey::HotkeyAction>,
}

impl TurnDriver {
    /// Build the driver from the user's real configuration, with the
    /// assistant backend and model optionally overridden.
    ///
    /// The overrides are applied to a clone and never written back: comparing
    /// five models must not leave the user's configuration on the fifth.
    pub fn new(config: Config, paths: Paths, secrets: &Secrets) -> Result<Self> {
        let assistant = fono_assistant::build_assistant(
            &config.assistant,
            secrets,
            &paths.polish_models_dir(),
        )
        .context("build assistant backend")?
        .context(
            "the assistant is disabled or set to `none` — a tool-use benchmark needs a model \
             that can call tools (`fono use assistant <backend>`)",
        )?;
        let (action_tx, _action_rx) = mpsc::unbounded_channel();
        Ok(Self { config, paths, assistant, action_tx, _action_rx })
    }

    /// The backend actually in use, for the report row.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.assistant.name()
    }

    /// Whether this backend can invoke tools at all.
    ///
    /// Worth checking before a run rather than discovering it fixture by
    /// fixture: a backend that cannot act is told so and answers honestly
    /// (see [`crate::actions::for_backend`]), which would score as a uniform
    /// failure and look like a routing collapse.
    #[must_use]
    pub fn can_run_actions(&self) -> bool {
        self.assistant.can_run_actions()
    }

    /// Run one utterance through the production turn and observe the result.
    ///
    /// `language` is what the user's configuration would have supplied; the
    /// utterance's own language is not declared to the model, because guessing
    /// it is part of what is being measured.
    pub async fn run(&self, utterance: &str) -> Result<TurnObservation> {
        // A fresh history per utterance. Fixtures are independent by
        // construction, and carrying context between them would make the
        // score depend on fixture order.
        let window = std::time::Duration::from_secs(
            60 * u64::from(self.config.assistant.history_window_minutes.max(1)),
        );
        let history =
            ConversationHistory::new(window, self.config.assistant.history_max_turns as usize);
        let state = Arc::new(Mutex::new(AssistantSessionState::new(history)));

        // Built exactly as `session.rs` builds it for a spoken turn, including
        // the withhold-and-tell-the-model path for a backend that cannot act.
        // No speaker was identified: a benchmark run is nobody's voice, and a
        // run recorded against an enrolled name would put a person's name on
        // a measurement they did not make.
        let actions = crate::actions::build(&self.config, &self.paths, None);
        let (actions, tools_note) = crate::actions::for_backend(
            actions,
            self.assistant.can_run_actions(),
            self.assistant.name(),
        );
        let system_prompt = crate::session::assistant_prompt_context(tools_note.as_deref());

        let inputs = AssistantTurnInputs {
            // No microphone. The pump's `pre_transcribed` branch skips STT
            // entirely, so an empty buffer is never read.
            pcm: Vec::new(),
            sample_rate: fono_core::config::AUDIO_SAMPLE_RATE_HZ,
            // Required by the struct but unreachable on this path.
            stt: Arc::new(SilentStt),
            assistant: self.assistant.clone(),
            // No speaker: the pump treats this as a text-only turn.
            tts: None,
            system_prompt,
            instructions: Some(self.config.assistant.prompt_main.trim().to_string()),
            // Speaker verification has no meaning without audio.
            speaker_note: None,
            speaker: None,
            language: self.config.general.language_override().map(str::to_string),
            action_tx: self.action_tx.clone(),
            overlay: None,
            pre_transcribed: Some(utterance.to_string()),
            prefer_vision: false,
            screen_capture_fn: None,
            actions,
            active_window_context: None,
            // There is no audio to trim, and correcting spellings would edit
            // the very sentence the fixture is measuring.
            trim_silence: false,
            vocabulary: fono_core::correction::VocabularyTable::default(),
        };

        let notify = Arc::new(Notify::new());
        let started = std::time::Instant::now();
        let produced = run_assistant_turn(state.clone(), inputs, notify)
            .await
            .context("assistant turn failed")?;
        let elapsed = started.elapsed();

        // Take the pump's own record of the turn. Written at the same instant
        // as the history push, so it says exactly what the model was told —
        // and, unlike history, it is still there after a turn that acted
        // clears the buffer.
        //
        // The lock is released before returning: the next utterance builds its
        // own state, but holding a guard across a return is how a deadlock
        // starts the day someone adds a second caller.
        let record = {
            let s = state.lock().await;
            s.last_turn.clone()
        };
        Ok(TurnObservation {
            reply: record.reply,
            aborted: record.aborted,
            calls: record
                .calls
                .into_iter()
                .map(|c| ObservedCall {
                    name: c.name,
                    arguments: c.arguments,
                    outcome: c.outcome,
                    failed: c.failed,
                })
                .collect(),
            elapsed,
            produced,
        })
    }
}

/// Stands in for the STT the pump never reaches on a pre-transcribed turn.
///
/// Returns an error rather than an empty string on purpose: if a refactor ever
/// makes the pump call STT on this path, the benchmark should fail loudly
/// instead of quietly scoring every fixture against silence.
struct SilentStt;

#[async_trait::async_trait]
impl fono_stt::traits::SpeechToText for SilentStt {
    async fn transcribe(
        &self,
        _pcm: &[f32],
        _sample_rate: u32,
        _lang: Option<&str>,
    ) -> Result<fono_stt::traits::Transcription> {
        anyhow::bail!(
            "the benchmark supplies text directly and must never reach speech-to-text; \
             the assistant turn ignored `pre_transcribed`"
        )
    }
    fn name(&self) -> &'static str {
        "bench-no-stt"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(calls: &[bool]) -> TurnObservation {
        TurnObservation {
            reply: String::new(),
            calls: calls
                .iter()
                .map(|failed| ObservedCall {
                    name: "HassClimateSetTemperature".into(),
                    arguments: "{}".into(),
                    outcome: Some("…".into()),
                    failed: *failed,
                })
                .collect(),
            elapsed: std::time::Duration::ZERO,
            produced: true,
            aborted: false,
        }
    }

    /// Only a repeat that could double something counts. Asked to make a room
    /// two degrees warmer, a model whose first attempt the house refused tried
    /// again and moved nothing twice — charging it four degrees for that would
    /// punish the recovery the ladder exists to provide.
    #[test]
    fn a_repeat_after_a_refusal_cannot_double_anything() {
        assert!(!turn(&[true, true]).doubled(), "neither attempt moved anything");
        assert!(!turn(&[true]).doubled(), "one call is not a repeat");
        assert!(!turn(&[]).doubled());
        assert!(turn(&[false, false]).doubled(), "the first one landed, so the second doubled it");
        assert!(!turn(&[true, false]).doubled(), "only the last one landed");
    }
}
