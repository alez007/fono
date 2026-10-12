// SPDX-License-Identifier: GPL-3.0-only
//! Anthropic Messages API client.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::traits::{looks_like_clarification, user_prompt, FormatContext, TextFormatter};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

pub struct AnthropicLlm {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl AnthropicLlm {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            client: crate::openai_compat::warm_client(),
        }
    }
}

#[derive(Serialize)]
struct Req<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<&'a str>,
    system: &'a str,
    messages: Vec<Message<'a>>,
}

impl<'a> Req<'a> {
    /// Older Claude models get a low temperature and a tight budget.
    /// Newer ones reject `temperature` and think before answering, so they
    /// get `low` effort and room for the thinking tokens instead.
    fn new(model: &'a str, system: &'a str, user: &'a str) -> Self {
        let sampling = fono_core::provider_catalog::claude_takes_sampling(model);
        Self {
            model,
            max_tokens: if sampling { 512 } else { 2048 },
            temperature: sampling.then_some(0.2),
            output_config: (!sampling).then(|| serde_json::json!({ "effort": "low" })),
            // Anthropic's Messages API rejects whitespace-only stop
            // sequences with a 400, so no `"\n\n"` guard here.
            stop_sequences: vec![],
            system,
            messages: vec![Message { role: "user", content: user }],
        }
    }
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct Resp {
    content: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(default)]
    text: String,
}

#[async_trait]
impl TextFormatter for AnthropicLlm {
    async fn format(&self, raw: &str, ctx: &FormatContext) -> Result<String> {
        let system = ctx.system_prompt();
        let user = user_prompt(raw);
        let req = Req::new(&self.model, &system, &user);
        let res = self
            .client
            .post(ENDPOINT)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&req)
            .send()
            .await
            .context("anthropic POST failed")?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("anthropic LLM {status}: {body}");
        }
        let parsed: Resp = serde_json::from_str(&body)
            .with_context(|| format!("parse anthropic response: {body}"))?;
        let out = parsed.content.into_iter().map(|b| b.text).collect::<String>().trim().to_string();
        if looks_like_clarification(&out) {
            anyhow::bail!(
                "anthropic LLM returned a clarification reply instead of a cleaned transcript; \
                 falling back to raw text. response: {out:?}"
            );
        }
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn prewarm(&self) -> Result<()> {
        // Anthropic's `/v1/models` endpoint exists; cheap GET warms the
        // TLS+HTTP/2 connection. Failures are non-fatal.
        let res = self
            .client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .context("anthropic prewarm")?;
        let _ = res.bytes().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Messages API rejects whitespace-only `stop_sequences` entries
    /// with a 400; the serialized request body must omit the field
    /// entirely (it's `skip_serializing_if = "Vec::is_empty"`).
    #[test]
    fn request_body_omits_stop_sequences() {
        let req = Req::new("claude-haiku-5-5", "sys", "hi");
        let body = serde_json::to_string(&req).expect("serialize");
        assert!(
            !body.contains("stop_sequences"),
            "stop_sequences must be omitted from the wire body, got: {body}"
        );
    }

    #[test]
    fn sampling_follows_the_model() {
        let new = serde_json::to_string(&Req::new("claude-haiku-5-5", "s", "u")).unwrap();
        assert!(!new.contains("temperature"), "{new}");
        assert!(new.contains(r#""output_config":{"effort":"low"}"#), "{new}");
        let old = serde_json::to_string(&Req::new("claude-haiku-4-5", "s", "u")).unwrap();
        assert!(old.contains(r#""temperature":0.2"#), "{old}");
        assert!(!old.contains("output_config"), "{old}");
    }
}
