// SPDX-License-Identifier: GPL-3.0-only
//! Cloud pass-through proxy for the OpenAI surface (ADR 0036).
//!
//! When the served assistant backend is an OpenAI-compatible cloud
//! provider, the OpenAI-surface handlers forward the client's request
//! **verbatim** to the upstream provider instead of adapting it through
//! the `Assistant` trait. This preserves full wire fidelity — every
//! model, tool/function-calling, vision, and request parameter passes
//! through untouched — for near-zero code, and unlocks cloud
//! tool-calling (the Home Assistant device-control path).
//!
//! The only mutation Fono makes to the request body is defaulting the
//! `model` field when the client omits it; the API key is injected on
//! the outbound leg (never exposed to the client). The adapter remains
//! the universal floor for everything that is not proxyable (local
//! llama.cpp, Anthropic, realtime, and the Ollama-native surface).

use std::convert::Infallible;
use std::sync::OnceLock;

use bytes::Bytes;
use fono_assistant::CloudUpstream;
use futures::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::{Response, StatusCode};

use super::access_log::ReqLog;
use super::{error_response, full, ResBody};

/// Shared outbound client (connection pool reused across requests).
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Apply the *only* mutation Fono makes to a proxied request body:
/// default `model` when the client omitted or blanked it. Returns the
/// effective model, for the access log.
///
/// Split out from `forward_chat` so the scope limit can be asserted
/// without a network call. Fono suppresses hidden reasoning on its *own*
/// assistant requests, because thinking dominates time-to-first-token on
/// the action path — but a client pointing at Fono's LLM server is not
/// Fono's assistant, and forcing that preference on it would silently
/// degrade a caller that wanted the model to think.
fn default_model(json: &mut serde_json::Value, fallback: &str) -> String {
    let client_model =
        json.get("model").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
    let Some(m) = client_model else {
        if let Some(obj) = json.as_object_mut() {
            obj.insert("model".to_string(), serde_json::Value::String(fallback.to_owned()));
        }
        return fallback.to_owned();
    };
    m.to_owned()
}

/// Forward a `/v1/chat/completions` request body to the cloud upstream
/// and relay the response (SSE stream or single JSON) back verbatim.
pub async fn forward_chat(
    upstream: &CloudUpstream,
    body: Bytes,
    log: &mut ReqLog,
) -> Response<ResBody> {
    let mut json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}"));
        }
    };
    let stream = json.get("stream").and_then(serde_json::Value::as_bool).unwrap_or(false);
    log.set_model(default_model(&mut json, &upstream.model));

    let mut req = client().post(&upstream.chat_url).json(&json);
    if !upstream.api_key.is_empty() {
        req = req.bearer_auth(&upstream.api_key);
    }
    if stream {
        req = req.header(reqwest::header::ACCEPT, "text/event-stream");
    }
    match req.send().await {
        Ok(resp) => relay(resp, stream, log).await,
        Err(e) => {
            tracing::warn!(target: "fono::llm::server", "proxy upstream request failed: {e:#}");
            error_response(StatusCode::BAD_GATEWAY, &format!("upstream request failed: {e}"))
        }
    }
}

/// Proxy `GET /v1/models` to the provider's `/models` endpoint so
/// clients discover the full catalogue. Returns `None` (caller falls
/// back to a single-model list) when no URL is derivable or the upstream
/// call fails.
pub async fn forward_models(upstream: &CloudUpstream) -> Option<Response<ResBody>> {
    let url = upstream.models_url.as_ref()?;
    let mut req = client().get(url);
    if !upstream.api_key.is_empty() {
        req = req.bearer_auth(&upstream.api_key);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(full(body))
            .expect("proxy models response builder"),
    )
}

/// Relay a `reqwest` response into a hyper response, streaming the body
/// through unchanged (SSE) or buffering it (single JSON). The upstream
/// status code and content-type are preserved. For streaming responses
/// the access line is emitted from the relay task (ttft = first frame);
/// non-streaming responses are finalised by `route()`.
async fn relay(resp: reqwest::Response, stream: bool, log: &mut ReqLog) -> Response<ResBody> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);

    if stream {
        let mut slog = log.defer(false);
        slog.set_status(status.as_u16());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(32);
        tokio::spawn(async move {
            let mut s = resp.bytes_stream();
            while let Some(item) = s.next().await {
                match item {
                    Ok(b) => {
                        slog.on_frame();
                        if tx.send(b).await.is_err() {
                            break; // client hung up
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "fono::llm::server", "proxy stream error: {e:#}");
                        break;
                    }
                }
            }
            slog.emit();
        });
        let body_stream = futures::stream::poll_fn(move |cx| {
            rx.poll_recv(cx).map(|opt| opt.map(|b| Ok::<Frame<Bytes>, Infallible>(Frame::data(b))))
        });
        let body: ResBody = BodyExt::boxed(StreamBody::new(body_stream));
        return Response::builder()
            .status(status)
            .header(
                hyper::header::CONTENT_TYPE,
                content_type.unwrap_or_else(|| "text/event-stream".to_string()),
            )
            .header(hyper::header::CACHE_CONTROL, "no-cache")
            .body(body)
            .expect("proxy stream response builder");
    }

    match resp.bytes().await {
        Ok(b) => Response::builder()
            .status(status)
            .header(
                hyper::header::CONTENT_TYPE,
                content_type.unwrap_or_else(|| "application/json".to_string()),
            )
            .body(full(b))
            .expect("proxy response builder"),
        Err(e) => {
            error_response(StatusCode::BAD_GATEWAY, &format!("reading upstream response: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::default_model;

    /// A client's request body reaches the upstream provider carrying
    /// exactly what the client wrote, plus a model when it omitted one.
    ///
    /// The counterpart test in `fono-assistant` asserts that Fono's *own*
    /// assistant requests suppress hidden reasoning, because thinking is
    /// the largest single cost on the action path. This one draws the line
    /// around that: someone pointing their editor or their agent at Fono's
    /// LLM server is not Fono's assistant. Forcing `reasoning_effort` or
    /// `enable_thinking` on them here would quietly degrade a caller who
    /// wanted the model to think, and they would have no way to see why.
    #[test]
    fn a_proxied_request_keeps_the_thinking_settings_the_client_chose() {
        let mut json = serde_json::json!({
            "model": "some-reasoning-model",
            "reasoning_effort": "high",
            "messages": [],
        });
        let effective = default_model(&mut json, "fono-default");

        assert_eq!(effective, "some-reasoning-model", "an explicit model is honoured");
        assert_eq!(json["reasoning_effort"], "high", "Fono must not lower the client's effort");
        assert!(json.get("think").is_none(), "Fono must not inject a switch the client omitted");
        assert!(json.get("chat_template_kwargs").is_none());
    }

    /// The one mutation Fono is allowed to make (ADR 0036), and only when
    /// the client left the field out or blank.
    #[test]
    fn a_missing_model_is_filled_in_and_nothing_else_is() {
        for absent in [serde_json::json!({"messages": []}), serde_json::json!({"model": ""})] {
            let mut json = absent;
            let effective = default_model(&mut json, "fono-default");
            assert_eq!(effective, "fono-default");
            assert_eq!(json["model"], "fono-default");
            assert!(json.get("reasoning_effort").is_none());
        }
    }
}
