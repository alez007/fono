// SPDX-License-Identifier: GPL-3.0-only
//! Minimal MCP **client** — enough to discover what a tool server offers.
//!
//! Distinct from `fono-mcp-server`, which exposes *Fono* to coding agents.
//! This is the other direction: Fono asking a server (Home Assistant's
//! `mcp_server` integration, and anything else speaking MCP) "what tools do
//! you have?" so the catalogue store can record them.
//!
//! ## Transport: SSE only
//!
//! Home Assistant 2026.7 serves the **SSE** MCP transport, not streamable
//! HTTP: `GET …/sse` opens an event stream whose first event carries a POST
//! endpoint, JSON-RPC requests go to that endpoint, and the *responses arrive
//! back on the stream*. That asymmetry is the whole reason this module exists
//! rather than a few `reqwest::post` calls. Streamable HTTP is deliberately
//! not implemented until a server we care about needs it.
//!
//! Discovery is a short-lived, one-shot exchange — open, `initialize`,
//! `tools/list`, drop the stream. Nothing here holds a session open.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use fono_core::tool_catalog::Device;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::sse::SseBuffer;

/// Who answered, for the UI and the logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
}

/// One tool exactly as the server advertised it. No interpretation happens
/// here — classification is the catalogue's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// The result of one discovery pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    pub server: ServerInfo,
    pub tools: Vec<RawTool>,
    /// The rooms this server knows about, when it can say. Empty otherwise.
    pub places: Vec<String>,
    /// The names of things that can actually be operated — lights, switches,
    /// blinds and so on, each with the kind of thing it is. Sensors are left
    /// out: they cannot be commanded, and they are the bulk of a typical home.
    pub devices: Vec<Device>,
}

/// Everything needed to reach one MCP server.
#[derive(Debug, Clone)]
pub struct McpEndpoint {
    /// Full SSE URL, e.g. `http://homeassistant.local:8123/mcp_server/sse`.
    pub url: String,
    /// Bearer token, when the server wants one.
    pub token: Option<String>,
    /// Ceiling on the whole exchange.
    pub timeout: Duration,
}

/// Scheme + authority of `url` (`http://host:port`), used to resolve the
/// relative POST endpoint the server hands back.
fn origin_of(url: &str) -> Result<&str> {
    let after_scheme =
        url.find("://").map(|i| i + 3).ok_or_else(|| anyhow!("not an absolute URL: {url}"))?;
    Ok(url[after_scheme..].find('/').map_or(url, |i| &url[..after_scheme + i]))
}

/// What a server said when we ran one of its tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolResult {
    /// The text blocks of the result, joined. This is what the model sees.
    pub text: String,
    /// The server's own verdict. Note that `false` means only "the server
    /// did not object" — Home Assistant returns a perfectly successful
    /// result for a command that matched nothing and did nothing, so this
    /// is never on its own proof that anything happened.
    pub is_error: bool,
}

type ByteStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + Sync>>;

/// An open exchange with one server: the event stream we read replies from,
/// the endpoint we post requests to, and the handshake already done.
struct Session {
    http: reqwest::Client,
    token: Option<String>,
    endpoint: String,
    stream: ByteStream,
    buf: SseBuffer,
    next_id: u64,
    server: ServerInfo,
}

impl Session {
    async fn open(ep: &McpEndpoint) -> Result<Self> {
        let http = reqwest::Client::builder()
            // The SSE stream idles between our requests; no total-response
            // timeout, only the outer deadline.
            .build()
            .context("build http client")?;

        let mut req = http.get(&ep.url).header("Accept", "text/event-stream");
        if let Some(tok) = &ep.token {
            req = req.bearer_auth(tok);
        }
        let resp = req.send().await.with_context(|| format!("connect to {}", ep.url))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("{} rejected the access token ({status})", ep.url);
        }
        if !status.is_success() {
            bail!("MCP server returned {status} for {}", ep.url);
        }
        // Catch the common mistake early: pointing at a web UI rather than at
        // the MCP endpoint. That answers 200 with HTML and then just closes,
        // which would otherwise surface much later as a baffling "stream
        // closed".
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !content_type.starts_with("text/event-stream") {
            bail!(
                "{} answered with {} instead of an event stream — is this the MCP \
                 address? (Home Assistant uses …:8123/mcp_server/sse)",
                ep.url,
                if content_type.is_empty() { "no content type" } else { &content_type }
            );
        }

        let mut stream: ByteStream = Box::pin(resp.bytes_stream());
        let mut buf = SseBuffer::new();

        // The server's first event names the POST endpoint.
        let endpoint = read_endpoint(&mut stream, &mut buf, origin_of(&ep.url)?).await?;

        let mut s = Self {
            http,
            token: ep.token.clone(),
            endpoint,
            stream,
            buf,
            next_id: 1,
            server: ServerInfo::default(),
        };

        // Handshake. The response comes back over the stream, not the POST.
        let init = s.request("initialize", init_params()).await?;
        s.server = server_info(&init);
        // Servers may refuse to answer anything before this notification.
        s.notify("notifications/initialized").await?;
        Ok(s)
    }

    /// Post a JSON-RPC request and wait for its reply on the stream.
    /// Returns the whole message, so callers can read `result` or `error`.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.post(&rpc(id, method, params)).await?;
        read_response(&mut self.stream, &mut self.buf, id).await
    }

    async fn notify(&self, method: &str) -> Result<()> {
        self.post(&json!({"jsonrpc": "2.0", "method": method})).await
    }

    async fn post(&self, body: &Value) -> Result<()> {
        let mut req = self.http.post(&self.endpoint).json(body);
        if let Some(tok) = &self.token {
            req = req.bearer_auth(tok);
        }
        let resp = req.send().await.with_context(|| format!("POST {}", self.endpoint))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("MCP server returned {status} for POST {}", self.endpoint);
        }
        Ok(())
    }
}

/// Open the stream, `initialize`, `tools/list`, close.
///
/// Returns the tools sorted by name: the catalogue render is byte-stable
/// (which keeps a pinned prompt-cache prefix valid), so discovery must not
/// leak server-side ordering into it.
pub async fn discover(ep: &McpEndpoint) -> Result<Discovery> {
    tokio::time::timeout(ep.timeout, discover_inner(ep))
        .await
        .map_err(|_| anyhow!("MCP discovery timed out after {:?}", ep.timeout))?
}

async fn discover_inner(ep: &McpEndpoint) -> Result<Discovery> {
    let mut s = Session::open(ep).await?;
    let listed = s.request("tools/list", json!({})).await?;
    let mut tools = parse_tools(&listed)?;
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    // While we are here, ask the server what rooms it has. Doing it now
    // rather than per command is the whole point: the model can be told the
    // real names without anybody waiting for a round-trip to find them.
    let mut places = Vec::new();
    let mut devices = Vec::new();
    if tools.iter().any(|t| t.name == LIVE_CONTEXT) {
        match s.request("tools/call", json!({"name": LIVE_CONTEXT, "arguments": {}})).await {
            Ok(msg) => {
                let text = parse_tool_result(&msg).text;
                places = parse_places(&text);
                devices = parse_devices(&text);
            }
            // A server that will not describe itself is not a broken server.
            // Discovery still succeeded; we simply have no room names.
            Err(e) => tracing::debug!(target: "fono.assistant", "no place names: {e}"),
        }
    }
    Ok(Discovery { server: s.server, tools, places, devices })
}

/// The tool a Home Assistant offers for describing itself.
const LIVE_CONTEXT: &str = "GetLiveContext";

/// Pull room names out of a Home Assistant live-context dump.
///
/// Deliberately a text scrape rather than a schema: the dump is prose-ish
/// YAML meant for a model to read, and every entity carries the area it is
/// in as `areas: <name>`. Anything unrecognised yields nothing, which
/// simply means the model is not told any names — the state we were in
/// before, never a wrong answer.
fn parse_places(text: &str) -> Vec<String> {
    let inner = inner_dump(text);
    let mut names: Vec<String> = inner
        .lines()
        .filter_map(|l| l.trim().strip_prefix("areas:").map(str::trim))
        // An entity in a room with aliases lists them all on one line
        // ("areas: Kitchen, bucătărie"), so each has to be split out — kept
        // whole it would be offered to the model as a single unusable name.
        .flat_map(|l| l.split(','))
        .map(|n| n.trim().trim_matches(['\'', '"']).trim().to_string())
        .filter(|n| !n.is_empty() && n.len() < 64)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Unwrap a live-context payload.
///
/// The dump arrives as JSON with the text under `result`, but a bare dump is
/// accepted too so a differently-shaped server still works. Getting this
/// wrong is invisible: nothing errors, we simply find no names.
fn inner_dump(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("result").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| text.to_string())
}

/// Domains worth naming to the model: things a command can act on.
///
/// Sensors are excluded deliberately — they are the bulk of a house (74 of
/// 155 on the home this was built against) and no command can switch one, so
/// listing them would triple the cost for nothing.
const ACTIONABLE: &[&str] = &[
    "light",
    "switch",
    "cover",
    "climate",
    "media_player",
    "lock",
    "vacuum",
    "fan",
    "button",
    "scene",
    "script",
    "todo",
];

/// Pull the actionable devices out of a live-context dump, each with the kind
/// of thing it is.
///
/// Same scrape as [`parse_places`], but it walks entity blocks rather than
/// filtering lines because the kind follows the name. A block looks like:
///
/// ```text
/// - names: Office outdoor light
///   domain: light
///   state: 'on'
///   areas: Yard
/// ```
///
/// The kind is kept rather than discarded because it is what lets Fono offer
/// the model the kinds this home actually contains.
fn parse_devices(text: &str) -> Vec<Device> {
    let inner = inner_dump(text);
    let mut found = Vec::new();
    let mut pending: Option<String> = None;
    for line in inner.lines() {
        let t = line.trim();
        if let Some(n) = t.strip_prefix("- names:") {
            // A block without a domain line is dropped, not guessed at.
            pending = Some(n.trim().trim_matches(['\'', '"']).trim().to_string());
        } else if let Some(d) = t.strip_prefix("domain:") {
            let d = d.trim().trim_matches(['\'', '"']).trim();
            if let Some(n) = pending.take() {
                if ACTIONABLE.contains(&d) && !n.is_empty() && n.len() < 64 {
                    found.push(Device::new(n, d));
                }
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Run one tool and return what the server said.
///
/// Each call is its own short-lived session. Holding a stream open between
/// utterances would be faster, but a stale session fails at the worst
/// possible moment — mid-command — and reconnecting costs a fraction of a
/// second against a model turn measured in seconds.
pub async fn call_tool(ep: &McpEndpoint, name: &str, args: &Value) -> Result<ToolResult> {
    tokio::time::timeout(ep.timeout, call_tool_inner(ep, name, args))
        .await
        .map_err(|_| anyhow!("{name} did not finish within {:?}", ep.timeout))?
}

async fn call_tool_inner(ep: &McpEndpoint, name: &str, args: &Value) -> Result<ToolResult> {
    let mut s = Session::open(ep).await?;
    let msg = s.request("tools/call", json!({"name": name, "arguments": args})).await?;
    Ok(parse_tool_result(&msg))
}

fn parse_tool_result(msg: &Value) -> ToolResult {
    let result = msg.get("result");
    let text = result
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    ToolResult {
        text,
        is_error: result.and_then(|r| r.get("isError")).and_then(Value::as_bool).unwrap_or(false),
    }
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn init_params() -> Value {
    json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": {"name": "fono", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Pull events until the `endpoint` event arrives, resolving it against
/// `origin` when the server sends a bare path (Home Assistant does).
async fn read_endpoint(
    stream: &mut (impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    buf: &mut SseBuffer,
    origin: &str,
) -> Result<String> {
    loop {
        if let Some(ev) = buf.next_event() {
            if ev.event.as_deref() == Some("endpoint") {
                let d = ev.data.trim();
                return Ok(if d.starts_with("http") {
                    d.to_owned()
                } else {
                    format!("{origin}{d}")
                });
            }
            continue;
        }
        let chunk = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("MCP stream closed before naming its endpoint"))?
            .context("read MCP stream")?;
        buf.push(&chunk);
    }
}

/// Pull events until a JSON-RPC message with `id` arrives.
async fn read_response(
    stream: &mut (impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    buf: &mut SseBuffer,
    id: u64,
) -> Result<Value> {
    loop {
        if let Some(ev) = buf.next_event() {
            let Ok(msg) = serde_json::from_str::<Value>(&ev.data) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                bail!("MCP server error: {err}");
            }
            return Ok(msg);
        }
        let chunk = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("MCP stream closed while awaiting response {id}"))?
            .context("read MCP stream")?;
        buf.push(&chunk);
    }
}

fn server_info(init: &Value) -> ServerInfo {
    let result = init.get("result");
    let info = result.and_then(|r| r.get("serverInfo"));
    let s = |v: Option<&Value>, k: &str| {
        v.and_then(|v| v.get(k)).and_then(Value::as_str).unwrap_or_default().to_owned()
    };
    ServerInfo {
        name: s(info, "name"),
        version: s(info, "version"),
        protocol_version: s(result, "protocolVersion"),
    }
}

fn parse_tools(listed: &Value) -> Result<Vec<RawTool>> {
    let arr = listed
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tools/list response had no result.tools array"))?;
    Ok(arr
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_owned();
            Some(RawTool {
                name,
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_strips_path() {
        assert_eq!(
            origin_of("http://homeassistant.local:8123/mcp_server/sse").unwrap(),
            "http://homeassistant.local:8123"
        );
        assert_eq!(origin_of("https://example.org").unwrap(), "https://example.org");
        assert!(origin_of("example.org/sse").is_err());
    }

    #[test]
    fn parses_the_home_assistant_shape() {
        let listed = json!({"result": {"tools": [
            {"name": "HassTurnOn", "description": "Turns on a device",
             "inputSchema": {"type": "object", "properties": {"area": {"type": "string"}}}},
            {"name": "GetLiveContext"},
        ]}});
        let tools = parse_tools(&listed).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "HassTurnOn");
        // A tool with no schema still parses — it just takes no arguments.
        assert_eq!(tools[1].schema, json!({}));
    }

    #[test]
    fn rejects_a_response_without_tools() {
        assert!(parse_tools(&json!({"result": {}})).is_err());
    }

    /// The real dump shape: JSON envelope, prose-ish YAML inside, one
    /// `areas:` line per entity — so the same room appears many times.
    #[test]
    fn scrapes_room_names_from_a_live_context_dump() {
        let dump = "Live Context:\n- names: Kitchen lights\n  domain: light\n  \
                    areas: Kitchen\n  state: 'off'\n- names: Kitchen lights (2)\n  \
                    domain: light\n  areas: Kitchen\n- names: Hall lamp\n  \
                    domain: light\n  areas: Hall\n- names: Some sensor\n  \
                    domain: sensor\n  areas: 'Yard'\n";
        let text = serde_json::to_string(&json!({"result": dump})).unwrap();
        // Deduplicated and sorted, so the same house always renders the same
        // bytes — a shifting list would invalidate the cached prompt prefix.
        assert_eq!(parse_places(&text), vec!["Hall", "Kitchen", "Yard"]);
        // A bare dump works too, for a server that does not wrap it.
        assert_eq!(parse_places(dump), vec!["Hall", "Kitchen", "Yard"]);
    }

    /// A room with aliases lists them all on one line. Each is a name the
    /// house will answer to, so each must reach the model separately —
    /// offered whole, "Kitchen, bucătărie" is a room that does not exist.
    #[test]
    fn a_room_with_aliases_yields_each_name_separately() {
        let dump = "- names: Kitchen lights\n  domain: light\n  areas: Kitchen, bucătărie\n\
                    - names: Hall light\n  domain: light\n  areas: 'Hallway , hol'\n";
        assert_eq!(parse_places(dump), vec!["Hallway", "Kitchen", "bucătărie", "hol"]);
    }

    /// Anything we do not recognise must yield no names at all. Telling the
    /// model a wrong room name is worse than telling it none: it would aim
    /// confidently at something that does not exist.
    #[test]
    fn an_unrecognised_dump_yields_no_room_names() {
        assert!(parse_places("").is_empty());
        assert!(parse_places("{\"result\": \"nothing useful here\"}").is_empty());
        assert!(parse_places("areas:").is_empty());
    }

    /// Only things that can be operated are worth naming. Sensors and their
    /// binary cousins are two thirds of a real house and nothing can be done
    /// to them, so listing them would spend tokens teaching the model names
    /// it must never act on.
    #[test]
    fn scrapes_only_operable_device_names() {
        let dump = "Live Context:\n- names: Office outdoor light\n  domain: light\n  \
                    areas: Yard\n  state: 'on'\n- names: Alocasia Soil humidity\n  \
                    domain: sensor\n  areas: Living room\n- names: Front door\n  \
                    domain: lock\n  areas: Hallway\n- names: Back door open\n  \
                    domain: binary_sensor\n  areas: Hallway\n";
        let text = serde_json::to_string(&json!({"result": dump})).unwrap();
        let want =
            vec![Device::new("Front door", "lock"), Device::new("Office outdoor light", "light")];
        assert_eq!(parse_devices(&text), want);
        // A bare dump works too, for a server that does not wrap it.
        assert_eq!(parse_devices(dump), want);
    }

    /// The exact shape that failed against a real house: the lamp is named
    /// after the room it lights, not the one it sits in. Its name must reach
    /// the model verbatim, because the house matches names exactly — neither
    /// "outdoor light" nor "outdoor office light" finds it.
    #[test]
    fn keeps_a_device_name_that_disagrees_with_its_room() {
        let dump = "- names: Office outdoor light\n  domain: light\n  areas: Yard\n";
        assert_eq!(parse_devices(dump), vec![Device::new("Office outdoor light", "light")]);
    }

    /// The kind of each device is carried through discovery, because it is what
    /// lets Fono offer the model only the kinds this home actually has.
    #[test]
    fn keeps_the_kind_of_each_device() {
        let dump = "- names: Kitchen lights\n  domain: light\n- names: Living room blind\n  \
                    domain: cover\n- names: Kitchen speaker\n  domain: media_player\n";
        let found = parse_devices(dump);
        let kinds: Vec<&str> = found.iter().map(|d| d.domain.as_str()).collect();
        // Sorted by name, so: Kitchen lights, Kitchen speaker, Living room blind.
        assert_eq!(kinds, vec!["light", "media_player", "cover"]);
    }

    /// A block with no domain line is dropped rather than guessed at, and an
    /// unrecognised dump yields nothing: a wrong name is worse than no name,
    /// because the model would aim confidently at something absent.
    #[test]
    fn an_unrecognised_dump_yields_no_device_names() {
        assert!(parse_devices("").is_empty());
        assert!(parse_devices("- names: Mystery lamp\n  areas: Yard\n").is_empty());
        assert!(parse_devices("{\"result\": \"nothing useful here\"}").is_empty());
    }

    #[test]
    fn reads_server_info() {
        let init = json!({"result": {
            "protocolVersion": "2025-03-26",
            "serverInfo": {"name": "Home Assistant", "version": "2026.7.3"},
        }});
        let info = server_info(&init);
        assert_eq!(info.name, "Home Assistant");
        assert_eq!(info.version, "2026.7.3");
        assert_eq!(info.protocol_version, "2025-03-26");
    }

    /// Live check against a real MCP server — the only thing that proves the
    /// SSE handshake is right, since the transport quirk (responses arrive on
    /// the stream, not the POST) is exactly what a mocked test would get
    /// wrong. Set `FONO_TEST_MCP_URL` and, if the server wants one,
    /// `FONO_TEST_MCP_TOKEN`.
    #[tokio::test]
    #[ignore = "needs a reachable MCP server"]
    async fn discovers_a_live_server() {
        let Ok(url) = std::env::var("FONO_TEST_MCP_URL") else { return };
        let ep = McpEndpoint {
            url,
            token: std::env::var("FONO_TEST_MCP_TOKEN").ok(),
            timeout: Duration::from_secs(30),
        };
        let d = discover(&ep).await.expect("discovery failed");
        eprintln!("server: {} {}", d.server.name, d.server.version);
        for t in &d.tools {
            eprintln!("  {}", t.name);
        }
        assert!(!d.server.name.is_empty(), "server did not identify itself");
        assert!(!d.tools.is_empty(), "server advertised no tools");
        // Sorted, so the catalogue render stays byte-stable.
        assert!(d.tools.windows(2).all(|w| w[0].name <= w[1].name));
    }
}
