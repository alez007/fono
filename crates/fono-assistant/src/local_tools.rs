// SPDX-License-Identifier: GPL-3.0-only
//! Tool calling for the embedded llama.cpp backend.
//!
//! The cloud backends get tool calling for free: they send OpenAI-style
//! descriptors and the server hands back a parsed `tool_calls` array. The
//! embedded backend has neither. It hand-rolls the chat markers rather than
//! rendering the GGUF's own Jinja template — that is what makes the pinned
//! prompt-cache prefix possible — and a model's trained tool syntax lives
//! *in* that template. Bypassing the template loses tools with it.
//!
//! So we ask in the prompt and parse the answer out of the text. For Gemma
//! that is not a downgrade: Gemma has no tool tokens at all, and its own
//! template does exactly this. For families that do have tokens (Qwen's
//! `<tool_call>` tags, most notably) we ask for the syntax they were trained
//! on, which is why that is the shape we request.
//!
//! The parser is deliberately more tolerant than the instructions: models
//! wander into fenced code blocks or drop the wrapper and emit bare JSON, and
//! a reply that was *meant* to switch a light must not be read out loud as
//! prose because of a missing tag.

use serde_json::Value;

/// The wrapper we ask for, and the one Qwen/Hermes-family models already emit.
const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// Openers a reply may legitimately begin with when it is a tool call.
/// Used to decide whether a partly-generated reply is still possibly a call.
///
/// `<` is deliberately broad. Models prefix calls with markers of their own —
/// `gemma-4-26b` emits a `<|channel>thought` preamble before a perfectly good
/// call — and the two mistakes are not equally cheap. Holding a reply that
/// turns out to be prose costs a moment of streaming; releasing one that turns
/// out to be a call means the light stays off and the machinery is read aloud.
/// A spoken reply never opens with an angle bracket, so nothing conversational
/// is delayed by this.
const OPENERS: [&str; 3] = ["<", "{", "```"];

/// The steady head of the system prompt: the caller's context, the tool block,
/// then how to behave.
///
/// The one place the tool block is rendered. The reply path and the cache
/// warm-up must produce the same bytes or the pinned checkpoint can never be
/// restored, and two renderings that had to agree by convention have drifted
/// twice before — each time costing a local model tens of seconds re-reading a
/// device list that had not changed. Ordering rationale lives on
/// [`crate::compose_head`].
#[must_use]
pub fn head_with_tools(
    context: &str,
    descriptors: &[Value],
    instructions_suffix: Option<&str>,
) -> String {
    let tools = (!descriptors.is_empty()).then(|| instructions(descriptors));
    crate::traits::compose_head(context, tools.as_deref(), instructions_suffix)
}

/// Renders the tool block appended to the system prompt.
///
/// Kept terse on purpose. Every line here is prefilled on the request path of
/// every turn, and on CPU prefill is the dominant cost — so the schema is
/// summarised to names and types rather than pasted as JSON.
#[must_use]
pub fn instructions(descriptors: &[Value]) -> String {
    let mut s = String::from(
        "You can operate the user's devices by calling a tool.\n\
         To call one, reply with EXACTLY this and nothing else:\n",
    );
    s.push_str(OPEN);
    s.push_str("{\"name\": \"ToolName\", \"arguments\": {\"key\": \"value\"}}");
    s.push_str(CLOSE);
    s.push_str(
        "\nOtherwise reply normally. Never write a tool call as prose, and never say you have \
         done something unless a tool result says so.\n\nTools:\n",
    );
    for d in descriptors {
        let f = d.get("function").unwrap_or(d);
        let Some(name) = f.get("name").and_then(Value::as_str) else { continue };
        s.push_str("- ");
        s.push_str(name);
        s.push('(');
        let props =
            f.get("parameters").and_then(|p| p.get("properties")).and_then(Value::as_object);
        if let Some(props) = props {
            let mut first = true;
            for (k, v) in props {
                if !first {
                    s.push_str(", ");
                }
                first = false;
                s.push_str(k);
                if v.get("type").and_then(Value::as_str) == Some("array") {
                    s.push_str("[]");
                }
            }
        }
        s.push(')');
        if let Some(desc) = f.get("description").and_then(Value::as_str) {
            let desc = desc.trim();
            if !desc.is_empty() {
                s.push_str(": ");
                s.push_str(desc.lines().next().unwrap_or(desc));
            }
        }
        s.push('\n');
    }
    s
}

/// Whether `text` — a reply generated so far — could still turn out to be a
/// tool call.
///
/// This is what lets the backend keep streaming. Tokens are held back only
/// while the answer is ambiguous; the moment the reply is plainly prose it is
/// released and streams as normal. Without it every turn would have to be
/// buffered whole, and a conversational answer would lose its head start
/// purely because the user happens to own a lamp.
#[must_use]
pub fn could_be_call(text: &str) -> bool {
    let t = text.trim_start();
    if t.is_empty() {
        return true;
    }
    OPENERS.iter().any(|o| t.starts_with(o) || o.starts_with(t))
}

/// Pulls the tool call out of a finished reply, as `(name, arguments_json)`.
///
/// `None` means the reply was prose after all — the caller must then say it,
/// not swallow it.
#[must_use]
pub fn parse_call(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    // Scan for the wrapper rather than requiring it first: models prefix calls
    // with channel or thinking markers, and a call announced late is still a
    // call. Anything before the opener is the model talking to itself and is
    // dropped, which also keeps those markers out of the user's ears.
    let mut body = t;
    if let Some(at) = t.find(OPEN) {
        let rest = &t[at + OPEN.len()..];
        body = rest.split(CLOSE).next().unwrap_or(rest);
    } else if let Some(rest) = t.strip_prefix("```") {
        // ```json\n{...}\n```
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        body = rest.split("```").next().unwrap_or(rest);
    }
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    // Some models nest it under the wrapper name they were shown.
    let v = v.get("tool_call").unwrap_or(&v);
    let name = v.get("name").and_then(Value::as_str)?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let args = v.get("arguments").or_else(|| v.get("parameters"));
    let args = match args {
        // Some emit the arguments as a JSON *string* rather than an object.
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "{}".to_string(),
    };
    Some((name, args))
}

/// Drops a model's own channel or thinking header from the front of a reply.
///
/// `gemma-4-26b` opens even a one-line answer with `<|channel>thought
/// <channel|>`, and spoken aloud that is noise the user has to listen through.
/// Only a *closing* marker counts — `|>` or `</think>` — because those end a
/// header rather than starting one, so ordinary prose containing an angle
/// bracket is left alone.
#[must_use]
pub fn strip_preamble(text: &str) -> &str {
    let cut = ["</think>", "|>"].iter().filter_map(|m| text.rfind(m).map(|i| i + m.len())).max();
    cut.map_or(text, |i| text[i..].trim_start())
}

/// Whether the reply so far could still be an unfinished channel header.
///
/// Used while streaming, where the header arrives a token at a time. It only
/// ever holds text that opens with `<` and has not yet closed, and gives up
/// after a short run so a stray bracket cannot swallow a whole answer.
#[must_use]
pub fn maybe_preamble(sofar: &str) -> bool {
    let t = sofar.trim_start();
    t.starts_with('<') && t.len() < 64 && strip_preamble(t) == t
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "HassTurnOn",
                "description": "Turns on a device.\nSecond line ignored.",
                "parameters": {"type": "object", "properties": {
                    "area": {"type": "string"},
                    "domain": {"type": "array", "items": {"type": "string"}}
                }}
            }
        })]
    }

    /// Verbatim from `gemma-4-26b`: it opened even a one-line confirmation
    /// with its own channel header, which the user then heard read aloud.
    #[test]
    fn a_channel_header_is_never_spoken() {
        assert_eq!(strip_preamble("<|channel>thought\n<channel|>Lights are on."), "Lights are on.");
        assert_eq!(strip_preamble("<think>hm</think>\nDone."), "Done.");
        // Ordinary prose is left exactly as it is.
        assert_eq!(strip_preamble("The lights are on."), "The lights are on.");
        assert_eq!(strip_preamble("5 > 3 and 2 < 4."), "5 > 3 and 2 < 4.");
    }

    /// While streaming, the header arrives a token at a time; holding must
    /// stop the moment it is plainly prose, or the answer never starts.
    #[test]
    fn holding_stops_as_soon_as_it_is_prose() {
        assert!(maybe_preamble("<"));
        assert!(maybe_preamble("<|chan"));
        assert!(!maybe_preamble("<|channel>thought\n<channel|>"));
        assert!(!maybe_preamble("Lights"));
    }

    #[test]
    fn the_tool_block_names_each_tool_and_its_arguments() {
        let s = instructions(&tools());
        assert!(s.contains("HassTurnOn(area, domain[])"), "{s}");
        assert!(s.contains("Turns on a device."), "{s}");
        // Only the first line of a description is worth the prefill.
        assert!(!s.contains("Second line"), "{s}");
    }

    /// The shape we ask for.
    #[test]
    fn reads_the_wrapper_we_asked_for() {
        let (n, a) = parse_call("<tool_call>{\"name\": \"HassTurnOn\", \"arguments\": {\"area\": \"Kitchen\"}}</tool_call>").unwrap();
        assert_eq!(n, "HassTurnOn");
        assert_eq!(a, "{\"area\":\"Kitchen\"}");
    }

    /// The shapes models actually produce when they drift. Each of these was
    /// a light that would otherwise have been read out loud instead of switched.
    #[test]
    fn reads_the_shapes_models_drift_into() {
        for raw in [
            "```json\n{\"name\": \"HassTurnOn\", \"arguments\": {\"area\": \"Kitchen\"}}\n```",
            "{\"name\": \"HassTurnOn\", \"arguments\": {\"area\": \"Kitchen\"}}",
            "{\"tool_call\": {\"name\": \"HassTurnOn\", \"parameters\": {\"area\": \"Kitchen\"}}}",
            // Arguments as a string, not an object.
            "{\"name\": \"HassTurnOn\", \"arguments\": \"{\\\"area\\\": \\\"Kitchen\\\"}\"}",
            // Verbatim from gemma-4-26b: a thinking-channel preamble in front
            // of an otherwise perfect call. Requiring the wrapper first read
            // this as prose and left the light off.
            "<|channel>thought\n<channel|><tool_call>{\"name\": \"HassTurnOn\", \"arguments\": {\"area\": \"Kitchen\"}}</tool_call>",
        ] {
            let (n, a) = parse_call(raw).unwrap_or_else(|| panic!("did not parse: {raw}"));
            assert_eq!(n, "HassTurnOn", "{raw}");
            assert!(a.contains("Kitchen"), "{raw} -> {a}");
        }
    }

    #[test]
    fn prose_is_not_mistaken_for_a_call() {
        assert!(parse_call("I will turn on the light in the master bedroom.").is_none());
        assert!(parse_call("<tool_call>not json</tool_call>").is_none());
        assert!(parse_call("{\"arguments\": {}}").is_none());
    }

    /// Holding back only while ambiguous is what keeps ordinary conversation
    /// streaming for anyone who owns a lamp.
    #[test]
    fn prose_is_released_as_soon_as_it_is_recognisable() {
        assert!(could_be_call(""));
        assert!(could_be_call("<to"));
        assert!(could_be_call("<tool_call>{\"nam"));
        assert!(could_be_call("{\"na"));
        assert!(could_be_call("``"));
        // A model's own preamble must not look like prose.
        assert!(could_be_call("<|channel>thought"));
        assert!(!could_be_call("I"));
        assert!(!could_be_call("Sure,"));
        assert!(!could_be_call("The kitchen light is on."));
    }
}
