// SPDX-License-Identifier: GPL-3.0-only
//! Turns the user's tool catalogue into something the model can call.
//!
//! Everything here is assembled once per turn from data that already
//! exists: the servers in the config, the rows the user left switched on,
//! and the secrets file. Nothing is discovered on the request path.
//!
//! The one rule this module exists to enforce is honesty about outcomes.
//! A server can answer cheerfully and have done nothing at all — Home
//! Assistant does exactly that when a command names a room it does not
//! have — so the wording of every summary is capped by how well the
//! effect could actually be checked. See [`fono_core::tool_catalog::VerifyClass`].
//!
//! Deciding *how well* means reading a server's own payloads, which is
//! knowledge of that particular software. All of it lives in [`vendor`]; this
//! module knows the ladder, never a vendor's name.

pub mod vendor;

use std::sync::Arc;

use fono_assistant::mcp_client::{self, McpEndpoint};
use fono_assistant::{ActionTools, ToolCall, ToolOutcome};
use fono_core::config::Config;
use fono_core::paths::Paths;
use fono_core::secrets::Secrets;
use fono_core::tool_catalog::{ToolCatalogStore, VerifyClass};
use tracing::{debug, info, warn};
use vendor::{Vendor, Verdict};

/// How long one tool call may take before we give up and say so.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Everything needed to run one stored tool.
#[derive(Clone)]
struct Runnable {
    endpoint: McpEndpoint,
    verify: VerifyClass,
    /// The tool whose output observes this one's effect, when there is one.
    readback: Option<String>,
}

/// Build the tool set for this turn, or `None` when the user has no tools
/// switched on — in which case the turn stays conversation-only and costs
/// nothing extra.
pub fn build(cfg: &Config, paths: &Paths) -> Option<Arc<ActionTools>> {
    if !cfg.assistant.tools.enabled || cfg.assistant.tools.mcp.is_empty() {
        return None;
    }
    let store = match ToolCatalogStore::open(&paths.tool_catalog_db()) {
        Ok(s) => s,
        Err(e) => {
            warn!("actions: cannot open tool catalogue: {e}");
            return None;
        }
    };
    let rows = store.active_tools().ok()?;
    if rows.is_empty() {
        return None;
    }

    let secrets = Secrets::load(&paths.secrets_file()).unwrap_or_default();
    let mut endpoints = std::collections::HashMap::new();
    for s in &cfg.assistant.tools.mcp {
        endpoints.insert(
            s.name.clone(),
            McpEndpoint {
                url: s.sse_url(),
                token: secrets.keys.get(&s.token_ref()).cloned(),
                timeout: CALL_TIMEOUT,
            },
        );
    }

    let mut descriptors = Vec::with_capacity(rows.len());
    let mut runnable = std::collections::HashMap::new();
    for r in rows {
        let Some(endpoint) = endpoints.get(&r.source).cloned() else { continue };
        descriptors.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": r.name,
                "description": r.description,
                "parameters": r.schema,
            }
        }));
        runnable.insert(
            r.name.clone(),
            Runnable { endpoint, verify: r.verify_class, readback: r.readback_tool },
        );
    }
    if descriptors.is_empty() {
        return None;
    }
    info!("actions: {} tools offered to the assistant", descriptors.len());

    let runnable = Arc::new(runnable);
    let execute: fono_assistant::ToolExecFn = Arc::new(move |call: ToolCall| {
        let runnable = runnable.clone();
        Box::pin(async move { run_one(&runnable, call).await })
    });
    let hint = cfg.assistant.tools.place_names.then(|| room_hint(&store)).flatten();
    Some(Arc::new(ActionTools { descriptors, execute, hint }))
}

/// What the user's tools amount to once the chosen backend is taken into
/// account, plus anything extra the model needs told.
///
/// A backend that cannot invoke tools does not fail — it replies fluently,
/// having quietly ignored them, so the model says "I'll turn the light on"
/// and no light moves. So the tools are withheld and the model is told, in
/// one line, that it cannot act. Better a plain "I can't do that" than a
/// promise nothing keeps.
pub(crate) fn for_backend(
    actions: Option<Arc<ActionTools>>,
    backend_can_act: bool,
    backend: &str,
) -> (Option<Arc<ActionTools>>, Option<String>) {
    let Some(actions) = actions else { return (None, None) };
    if backend_can_act {
        let hint = actions.hint.clone();
        return (Some(actions), hint);
    }
    warn!(
        "actions: {} tools are switched on, but the {backend} assistant cannot run them — \
         telling the model it cannot act rather than letting it promise",
        actions.descriptors.len()
    );
    (None, Some(CANNOT_ACT.to_string()))
}

/// Said to the model when tools exist but the backend cannot reach them.
const CANNOT_ACT: &str = "You cannot control any devices or run any tools in this conversation. \
     If asked to, say plainly that you are unable to, and do not claim to have done it.";

/// The one line that stops the model inventing a room name.
///
/// Without it a Romanian command asks for `bucătărie` in a house whose
/// rooms are all named in English, Home Assistant matches nothing, and
/// nothing happens. Naming the rooms turns an open guess into a closed
/// choice — which is a translation, the one thing a model is reliably good
/// at — and does so in every language at once, including ones nobody
/// anticipated. Aliases in the house can only widen what it accepts; they
/// cannot stop the guessing.
///
/// Read from the catalogue, learned when the server was connected, so this
/// costs no network on the request path.
///
/// The second sentence exists because of a failure that looked like a
/// missing device and was not. Asked to switch off a lamp whose name began
/// with a room, the model searched only that room and found nothing — the
/// lamp was named after the place it lights, not the one it sits in. It then
/// reported the lamp unavailable while it was on. Device names routinely
/// mention somewhere they are not, so narrowing the search by room hides the
/// very thing being looked for.
///
/// The third sentence exists because "act on the room in one call", left on
/// its own, is dangerous advice. Asked in Romanian to turn on *the light* in
/// the office, the model asked for the whole office — and a room-wide switch-on
/// reaches everything switchable in it, so the air conditioning came on while
/// the one lamp that was actually wanted failed. A room plus a kind of device
/// is still one call; saying which kind is what keeps the room from being a
/// blunt instrument.
fn room_hint(store: &ToolCatalogStore) -> Option<String> {
    let names = store.place_names().ok()?;
    if names.is_empty() {
        return None;
    }
    let mut hint = format!(
        "Rooms in this home, named exactly as they must be used: {}. \
         Never translate or invent a room name — pick the closest one from this list. \
         When the user asks for a room, act on the room in one call and do not name \
         devices individually. When the user says which kind of device they mean — the \
         lights, the heating, the blinds — pass that kind as the domain alongside the \
         area, for example {{\"area\": \"Office\", \"domain\": [\"light\"]}}: a room \
         without a domain acts on everything switchable in it, so asking for a room \
         when the user asked for its lights will also start the air conditioning. \
         Only leave the domain out when the user really did mean the whole room. \
         When the user names a device rather than a room, act on \
         it by that name and do not narrow the search to a room: a device's name often \
         mentions somewhere it is not.",
        names.join(", ")
    );

    // The device names, when there are few enough to state without crowding
    // out the conversation. A truncated list would be worse than none: the
    // model would conclude a real device does not exist and say so.
    if let Ok(devices) = store.device_names() {
        if !devices.is_empty() && devices.len() <= MAX_LISTED_DEVICES {
            use std::fmt::Write as _;
            let _ = write!(
                hint,
                " Devices in this home, named exactly as they must be used: {}. \
                 Use one of these names verbatim — the home matches a name only exactly, \
                 so a shortened name or one with the words in a different order finds nothing.",
                devices.join(", ")
            );
        } else if devices.len() > MAX_LISTED_DEVICES {
            debug!(
                "actions: {} devices is too many to name in the prompt; \
                 the model will have to look them up",
                devices.len()
            );
        }
    }
    Some(hint)
}

/// How many device names may go in the prompt.
///
/// Measured on a real home: 77 devices cost about 400 tokens, which is a
/// fair price for the model never having to guess a name. Beyond a few
/// hundred the list would dominate the prompt, and the lookup tool is the
/// better answer.
const MAX_LISTED_DEVICES: usize = 200;

/// Run one call the model asked for and describe what happened.
///
/// Never returns an error: a tool that failed is the news, not a fault in
/// the turn, and the user has to hear it.
async fn run_one(
    runnable: &std::collections::HashMap<String, Runnable>,
    call: ToolCall,
) -> ToolOutcome {
    let bad = |s: String| ToolOutcome { summary: s, failed: true };
    let ok = |s: String| ToolOutcome { summary: s, failed: false };

    let Some(r) = runnable.get(&call.name) else {
        return bad(format!("There is no tool called {}.", call.name));
    };
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    let res = match mcp_client::call_tool(&r.endpoint, &call.name, &args).await {
        Err(e) => return bad(format!("{} could not be run: {e}", call.name)),
        // The server objected. Its own words are the most useful thing we
        // have, and they are what tells the user why.
        Ok(res) if res.is_error => {
            return bad(format!("{} failed: {}", call.name, brief(&res.text)));
        }
        Ok(res) => res,
    };

    // Which software answered decides what its answer means, and the answer
    // itself is the only thing that can say. Anything unrecognised gets no
    // opinion, so the rungs below simply do not fire.
    let vendor = vendor::for_result(&res.text);

    // Second rung. A server can answer "fine" and mean nothing of the sort:
    // Home Assistant returns an ordinary, error-free result for a command that
    // matched no device. Only the vendor can read that admission.
    //
    // A half-done command is its own answer and must not be flattened into
    // either neighbour. Told "it did not work" the model apologises for
    // nothing; told "done" it hides a lamp that stayed dark. Naming the
    // devices that were missed is what lets the reply be true.
    match vendor.admission(&res.text) {
        Some(vendor::Admission::NothingWorked) => {
            return bad(format!("{} did not work: {}", call.name, brief(&res.text)));
        }
        Some(vendor::Admission::PartlyWorked { failed }) => {
            return bad(format!(
                "{} worked for some devices but not for these: {}. Tell the user which ones \
                 did not respond, and do not claim the whole request succeeded.",
                call.name,
                failed.join(", ")
            ));
        }
        Some(vendor::Admission::Worked) | None => {}
    }

    // Top rung. Ask the world itself, rather than taking the server's word for
    // its own success. Costs one extra read (~100 ms), so it is only paid when
    // there is a readback tool and the vendor can actually judge the answer.
    match (&r.readback, r.verify) {
        (Some(rb), VerifyClass::PostCondition) if vendor.checks(&call.name) => {
            match confirm(r, vendor, rb, &call, &res.text).await {
                Some(Verdict::Contradicted) => {
                    // Deliberately not "nothing changed": the check may have
                    // found some devices obeying and others not, and claiming
                    // more than was observed is the mistake this rung exists
                    // to stop.
                    return bad(format!(
                        "{} was accepted, but the devices are not in the state you asked for.",
                        call.name
                    ));
                }
                Some(Verdict::Confirmed) => info!(tool = %call.name, "action confirmed"),
                // Unproven is not disproven: the weaker rungs stand.
                None => {}
            }
        }
        // Nothing observes this tool's effect, so "it was accepted" is the
        // strongest true statement available. Saying "done" here would be
        // inventing evidence.
        (_, VerifyClass::None) => {
            return ok(format!("{} was sent. {}", call.name, brief(&res.text)));
        }
        _ => {}
    }
    ok(brief(&res.text))
}

/// Re-read the world and ask the vendor whether it matches what was asked.
///
/// A readback that fails to arrive yields `None`: not being able to check is
/// not the same as having checked and found a problem, and reporting a working
/// command as broken because a second request timed out would be its own bug.
async fn confirm(
    r: &Runnable,
    vendor: &'static dyn Vendor,
    readback: &str,
    call: &ToolCall,
    result: &str,
) -> Option<Verdict> {
    let empty = serde_json::json!({});
    match mcp_client::call_tool(&r.endpoint, readback, &empty).await {
        Ok(back) => vendor.confirms(call, result, &back.text),
        Err(e) => {
            warn!("actions: could not check whether {} worked: {e}", call.name);
            None
        }
    }
}

/// Servers can be chatty, and every extra token here is paid for twice —
/// once reading it, once replying to it. The cap is nonetheless generous,
/// because the part a server puts last is often the part that says what
/// went wrong: Home Assistant's result ends with its list of failures, and
/// a tighter limit cut exactly that off, leaving the model reading an
/// apparently clean success. Trimming is visible (`…`) so a truncated
/// result is never mistaken for a complete one.
fn brief(text: &str) -> String {
    const MAX: usize = 2000;
    let t = text.trim();
    if t.is_empty() {
        return "Done.".into();
    }
    match t.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(verify: VerifyClass) -> std::collections::HashMap<String, Runnable> {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "HassTurnOn".to_string(),
            Runnable {
                endpoint: McpEndpoint {
                    url: "http://127.0.0.1:1/sse".into(),
                    token: None,
                    timeout: std::time::Duration::from_millis(50),
                },
                verify,
                readback: Some("GetLiveContext".into()),
            },
        );
        m
    }

    fn call(name: &str) -> ToolCall {
        ToolCall { id: "1".into(), name: name.into(), arguments: "{}".into() }
    }

    /// A name the catalogue does not know must be reported, not silently
    /// dropped — otherwise the model waits for a result that never comes.
    #[tokio::test]
    async fn an_unknown_tool_is_reported() {
        let out = run_one(&tools(VerifyClass::PostCondition), call("Nope")).await;
        assert!(out.failed, "an unknown tool is not a success");
        assert!(out.summary.contains("no tool called Nope"), "{}", out.summary);
    }

    /// A server we cannot reach must say so in words the user can act on,
    /// rather than the turn failing or, worse, claiming success.
    #[tokio::test]
    async fn an_unreachable_server_is_reported_not_claimed_done() {
        let out = run_one(&tools(VerifyClass::PostCondition), call("HassTurnOn")).await;
        assert!(out.failed, "unreachable must not be logged as a success");
        assert!(out.summary.starts_with("HassTurnOn could not be run"), "{}", out.summary);
        assert!(!out.summary.to_lowercase().contains("done"), "{}", out.summary);
    }

    /// A device named after somewhere it is not — a lamp called after the
    /// place it lights rather than the place it sits — was reported missing
    /// because the model narrowed its search to that room. The hint has to
    /// say so, or the same lookup fails the same way.
    #[test]
    fn the_room_hint_warns_against_searching_by_room_for_a_named_device() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string(), "Yard".to_string()]).expect("names");
        let hint = room_hint(&store).expect("names present means a hint");
        assert!(hint.contains("Office"), "{hint}");
        let lower = hint.to_lowercase();
        assert!(lower.contains("never translate"), "{hint}");
        assert!(lower.contains("do not narrow the search to a room"), "{hint}");
    }

    /// A room-wide switch-on reaches everything switchable in the room. Asked
    /// for *the light* in the office, the model asked for the office, and the
    /// air conditioning came on. The hint has to name the domain escape hatch,
    /// or a request for one kind of device keeps acting on all of them.
    #[test]
    fn the_room_hint_asks_for_a_domain_when_the_user_named_a_kind_of_device() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string()]).expect("names");
        let hint = room_hint(&store).expect("names present means a hint");
        let lower = hint.to_lowercase();
        assert!(lower.contains("domain"), "{hint}");
        assert!(hint.contains("[\"light\"]"), "the worked example must survive: {hint}");
    }

    /// The home matches a device name only exactly: "outdoor office light"
    /// and "outdoor light" both find nothing when the device is "Office
    /// outdoor light". Naming the devices removes the guess, so the list has
    /// to reach the prompt verbatim, with the exactness spelled out.
    #[test]
    fn device_names_are_stated_exactly_when_there_are_few_enough() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Yard".to_string()]).expect("rooms");
        store
            .set_device_names(
                "home",
                &["Office outdoor light".to_string(), "Hall lamp".to_string()],
            )
            .expect("devices");
        let hint = room_hint(&store).expect("hint");
        assert!(hint.contains("Office outdoor light"), "{hint}");
        assert!(hint.to_lowercase().contains("only exactly"), "{hint}");
    }

    /// A truncated list is worse than none: the model would read it as the
    /// whole house and tell the user a real device does not exist.
    #[test]
    fn too_many_devices_are_left_out_rather_than_cut_short() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Yard".to_string()]).expect("rooms");
        let many: Vec<String> =
            (0..=MAX_LISTED_DEVICES).map(|i| format!("Device number {i}")).collect();
        store.set_device_names("home", &many).expect("devices");
        let hint = room_hint(&store).expect("rooms still give a hint");
        assert!(hint.contains("Yard"), "the room half must survive: {hint}");
        assert!(!hint.contains("Device number"), "a partial list must not be stated: {hint}");
    }

    /// No rooms means no sentence: an empty list would spend tokens telling
    /// the model to choose from nothing.
    #[test]
    fn no_rooms_means_no_hint() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        assert!(room_hint(&store).is_none());
    }

    /// Long server output is trimmed, but visibly, and with enough room
    /// that a real result's trailing failure list survives.
    #[test]
    fn long_output_is_trimmed_visibly() {
        assert_eq!(brief("  "), "Done.");
        let out = brief(&"x".repeat(50_000));
        assert!(out.ends_with('…'), "truncation must be visible: {}", &out[out.len() - 20..]);
        assert!(out.chars().count() < 2100);
    }

    /// The whole chain except the model: real config, real catalogue, real
    /// server, real light. Everything up to here can be mocked into
    /// agreeing with itself; only this says the lamp changed.
    ///
    /// Run with a configured server present:
    /// `cargo test -p fono --lib turns_on_a_real_light -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a configured MCP server and switches a real light"]
    async fn turns_on_a_real_light() {
        let paths = Paths::resolve().expect("paths");
        let cfg = Config::load(&paths.config_file()).expect("config");
        let tools = build(&cfg, &paths).expect("no tools configured — nothing to test");
        assert!(
            tools.descriptors.iter().any(|d| d["function"]["name"] == "HassTurnOn"),
            "HassTurnOn is not switched on in the catalogue"
        );

        let area = std::env::var("FONO_TEST_ACTION_AREA").unwrap_or_else(|_| "Kitchen".into());
        let switch = |on: bool| ToolCall {
            id: "live".into(),
            name: if on { "HassTurnOn" } else { "HassTurnOff" }.into(),
            arguments: serde_json::json!({"area": area, "domain": ["light"]}).to_string(),
        };
        let started = std::time::Instant::now();
        let out = (tools.execute)(switch(true)).await;
        println!("[{} ms] failed={} {}", started.elapsed().as_millis(), out.failed, out.summary);
        assert!(!out.failed, "{}", out.summary);
        assert!(out.summary.contains(&area), "nothing in {area} was touched: {}", out.summary);

        // Again, with the lights already on. Nothing changes, and that must
        // still be reported as having worked: what is checked is whether the
        // world is as the user asked, not whether anything moved. A change
        // detector would call this a failure — and it would be wrong, because
        // "turn on the light" is satisfied by a light that is already on.
        let started = std::time::Instant::now();
        let out = (tools.execute)(switch(true)).await;
        println!(
            "[{} ms] again: failed={} {}",
            started.elapsed().as_millis(),
            out.failed,
            out.summary
        );
        assert!(!out.failed, "asking for a state already true is not a failure: {}", out.summary);

        let _ = (tools.execute)(switch(false)).await;
    }

    /// A backend that cannot invoke tools must not be handed them, because
    /// the failure is silent: it answers fluently, having ignored them, and
    /// the model promises an action nothing performs. This is exactly what
    /// the embedded local backend did — 15 tools offered, the reply said it
    /// would turn the bedroom light on, and no call was ever made.
    #[test]
    fn a_backend_that_cannot_act_is_told_so_instead_of_being_handed_tools() {
        let tools = Arc::new(ActionTools {
            descriptors: vec![serde_json::json!({"type": "function"})],
            execute: Arc::new(|_| {
                Box::pin(async { ToolOutcome { summary: String::new(), failed: false } })
            }),
            hint: Some("Rooms: Kitchen".into()),
        });

        let (kept, note) = for_backend(Some(tools.clone()), true, "openai");
        assert!(kept.is_some(), "a backend that can act keeps its tools");
        assert_eq!(note.as_deref(), Some("Rooms: Kitchen"), "and still gets the room names");

        let (kept, note) = for_backend(Some(tools), false, "llama-local");
        assert!(kept.is_none(), "a backend that cannot act is not handed tools");
        let note = note.expect("and is told why");
        assert!(note.contains("cannot control"), "{note}");
        // The room list is pointless here and would only cost tokens.
        assert!(!note.contains("Kitchen"), "{note}");

        // No tools configured at all stays silent: nothing to explain.
        assert_eq!(for_backend(None, false, "llama-local").1, None);
    }

    /// The whole chain, model included: spoken words in, real light out.
    /// This is the only test that says the feature works — everything
    /// narrower proves a part in isolation.
    ///
    /// `cargo test -p fono --lib says_a_command_and_the_light_changes -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a configured assistant and MCP server, and switches a real light"]
    #[allow(clippy::too_many_lines, reason = "one end-to-end story, split would obscure it")]
    async fn says_a_command_and_the_light_changes() {
        use futures::StreamExt;

        let paths = Paths::resolve().expect("paths");
        let cfg = Config::load(&paths.config_file()).expect("config");
        let secrets = Secrets::load(&paths.secrets_file()).unwrap_or_default();
        // Connect first, as pressing "Save & connect" does, so the room names
        // are learned before any command is spoken. This is also the whole
        // claim: the names cost nothing per command because they are already
        // known by the time one arrives.
        let mut endpoint = None;
        for s in &cfg.assistant.tools.mcp {
            let ep = fono_assistant::mcp_client::McpEndpoint {
                url: s.sse_url(),
                token: secrets.keys.get(&s.token_ref()).cloned(),
                timeout: CALL_TIMEOUT,
            };
            let found = fono_assistant::mcp_client::discover(&ep).await.expect("discover");
            let store = ToolCatalogStore::open(&paths.tool_catalog_db()).expect("store");
            store.set_place_names(&s.name, &found.places).expect("store rooms");
            store.set_device_names(&s.name, &found.devices).expect("store devices");
            println!(
                "{} is {}, {} rooms, {} devices",
                s.name,
                found.server.name,
                found.places.len(),
                found.devices.len()
            );
            endpoint = Some(ep);
        }
        let ep = endpoint.expect("no MCP server configured");
        let actions = build(&cfg, &paths).expect("no tools configured");
        assert!(actions.hint.is_some(), "the model was told no room names");
        let assistant =
            fono_assistant::build_assistant(&cfg.assistant, &secrets, &paths.polish_models_dir())
                .expect("build assistant")
                .expect("no assistant backend configured");

        // Nothing about one particular home is baked in: the room defaults to
        // a name almost every house has, and both it and the foreign name can
        // be pointed at whatever the machine running this actually owns.
        let area = std::env::var("FONO_TEST_ACTION_AREA").unwrap_or_else(|_| "Kitchen".into());
        let alt = std::env::var("FONO_TEST_ACTION_ALT_AREA").unwrap_or_else(|_| "bucătărie".into());

        // Each phrase asks for a state the lights are not already in, so a
        // command that does nothing cannot pass by accident. The second is the
        // whole point: a room named in another language, which the house has
        // never heard of and only the room list can rescue. What is asserted
        // is the light, never the reply: the assistant claiming success is
        // exactly the thing under suspicion.
        let phrases = [
            (format!("turn off the {} lights", area.to_lowercase()), false),
            (format!("aprinde luminile din {alt}"), true),
        ];
        for (phrase, want_on) in phrases {
            set_lights(&ep, &area, !want_on).await;
            let ctx = fono_assistant::AssistantContext {
                // Compose the prompt through the shipping path, so the room
                // names reach the model exactly as they do in a real turn.
                system_prompt: crate::session::assistant_system_prompt(
                    &cfg.assistant.prompt_main,
                    actions.hint.as_deref(),
                    None,
                ),
                actions: Some(actions.clone()),
                ..Default::default()
            };
            let started = std::time::Instant::now();
            let mut stream = assistant.reply_stream(&phrase, &ctx).await.expect("reply_stream");
            let mut said = String::new();
            let mut ran = Vec::new();
            while let Some(d) = stream.next().await {
                let d = d.expect("delta");
                said.push_str(&d.text);
                if let Some(fono_assistant::ToolEvent::Called(c)) = &d.tool_event {
                    ran.push(c.name.clone());
                }
            }
            let lit = lights_are_on(&ep, &area).await;
            println!(
                "\n[{} ms] {phrase:?}\n  ran: {ran:?}\n  said: {}\n  lights on: {lit} (wanted {want_on})",
                started.elapsed().as_millis(),
                said.trim()
            );
            assert_eq!(lit, want_on, "the lights did not end up as asked; it said: {said}");
        }

        // A device asked for by its own name, which is the case the room list
        // cannot help with: the name often mentions a room the device is not
        // in, and the house matches names exactly, so a paraphrase finds
        // nothing. Skipped unless the machine running this names a device it
        // actually owns.
        let Ok(device) = std::env::var("FONO_TEST_ACTION_DEVICE") else { return };
        for want_on in [true, false] {
            set_device(&ep, &device, !want_on).await;
            let ctx = fono_assistant::AssistantContext {
                system_prompt: crate::session::assistant_system_prompt(
                    &cfg.assistant.prompt_main,
                    actions.hint.as_deref(),
                    None,
                ),
                actions: Some(actions.clone()),
                ..Default::default()
            };
            // Deliberately not the device's exact name: the point is that the
            // model has to recover the real one from the list it was given.
            let phrase = format!(
                "turn {} the {}",
                if want_on { "on" } else { "off" },
                device.to_lowercase()
            );
            let started = std::time::Instant::now();
            let mut stream = assistant.reply_stream(&phrase, &ctx).await.expect("reply_stream");
            let mut said = String::new();
            let mut ran = Vec::new();
            while let Some(d) = stream.next().await {
                let d = d.expect("delta");
                said.push_str(&d.text);
                if let Some(fono_assistant::ToolEvent::Called(c)) = &d.tool_event {
                    ran.push(c.name.clone());
                }
            }
            let lit = device_is_on(&ep, &device).await;
            println!(
                "\n[{} ms] {phrase:?}\n  ran: {ran:?}\n  said: {}\n  on: {lit} (wanted {want_on})",
                started.elapsed().as_millis(),
                said.trim()
            );
            assert_eq!(lit, want_on, "{device} did not end up as asked; it said: {said}");
        }
    }

    /// Put one named device in a known state, bypassing the model.
    #[cfg(test)]
    async fn set_device(ep: &fono_assistant::mcp_client::McpEndpoint, name: &str, on: bool) {
        let tool = if on { "HassTurnOn" } else { "HassTurnOff" };
        let args = serde_json::json!({"name": name});
        fono_assistant::mcp_client::call_tool(ep, tool, &args).await.expect("set the device");
    }

    /// Ask the house about one named device, rather than believing the reply.
    #[cfg(test)]
    /// Home Assistant hands the dump back as an escaped JSON string, so the
    /// newlines are two characters until it is unwrapped. Reading it raw makes
    /// every exact-name match fail, which looks exactly like a dark lamp.
    async fn live_dump(ep: &fono_assistant::mcp_client::McpEndpoint) -> String {
        let out =
            fono_assistant::mcp_client::call_tool(ep, "GetLiveContext", &serde_json::json!({}))
                .await
                .expect("read the house");
        serde_json::from_str::<serde_json::Value>(&out.text)
            .ok()
            .and_then(|v| v.get("result")?.as_str().map(str::to_owned))
            .unwrap_or(out.text)
    }

    async fn device_is_on(ep: &fono_assistant::mcp_client::McpEndpoint, name: &str) -> bool {
        live_dump(ep)
            .await
            .split("- names: ")
            .filter(|b| b.split('\n').next().unwrap_or("").eq_ignore_ascii_case(name))
            .any(|b| b.contains("state: 'on'"))
    }

    /// Put a room's lights in a known state without involving the model, so
    /// the command under test has something real to change.
    #[cfg(test)]
    async fn set_lights(ep: &fono_assistant::mcp_client::McpEndpoint, area: &str, on: bool) {
        let name = if on { "HassTurnOn" } else { "HassTurnOff" };
        let args = serde_json::json!({"area": area, "domain": ["light"]});
        fono_assistant::mcp_client::call_tool(ep, name, &args).await.expect("set the lights");
    }

    /// Ask the house, rather than the assistant, whether the lights are on.
    ///
    /// Goes straight to the server rather than through the executor: what the
    /// executor returns is trimmed to keep a huge dump out of the model's
    /// prompt, and the answer we need can be past the cut. Checking a state
    /// has to see the whole state.
    #[cfg(test)]
    async fn lights_are_on(ep: &fono_assistant::mcp_client::McpEndpoint, area: &str) -> bool {
        live_dump(ep)
            .await
            .split("- names: ")
            .filter(|b| {
                b.contains("domain: light") && b.split('\n').next().unwrap_or("").contains(area)
            })
            .any(|b| b.contains("state: 'on'"))
    }
}
