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
use fono_core::conversations::ToolUse;
use fono_core::paths::Paths;
use fono_core::secrets::Secrets;
use fono_core::tool_catalog::{RunOutcome, ToolCatalogStore, VerifyClass};
use fono_core::turn_trace::{current_instant, current_span, ACTIONS_LANE};
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
    /// What the server said it accepts, kept so an obviously wrong argument
    /// can be caught here rather than costing a round trip.
    schema: serde_json::Value,
    /// Which server offers it. Two servers may publish the same tool name, so
    /// a run has to be filed against the right one.
    source: String,
}

/// Where a finished call gets written down, and on whose behalf.
///
/// The store is reopened per call rather than held open: a SQLite handle is
/// not shareable across the async boundary this closure lives on, and opening
/// one costs microseconds beside a round trip to the house. Recording is
/// strictly best-effort — a command that worked must never be reported as
/// failed because a bookkeeping write did not land.
#[derive(Clone)]
struct Journal {
    db: std::path::PathBuf,
    /// The enrolled speaker for this turn, when one was recognised. Fixed for
    /// the life of the turn, because that is what it describes.
    speaker: Option<String>,
    /// When the assistant was last free to think: the moment this turn's tools
    /// were built, and thereafter the moment each call returned. The gap up to
    /// the next call is the model deciding, which is usually the larger half of
    /// what the user experiences as "how long that took" — a page reporting
    /// only the round trip to the server flatters Fono and misleads whoever is
    /// trying to work out why a command feels slow.
    idle_since: Arc<std::sync::Mutex<std::time::Instant>>,
}

impl Journal {
    fn note(
        &self,
        source: &str,
        tool: &str,
        how: RunOutcome,
        think: std::time::Duration,
        elapsed: std::time::Duration,
        targets: &[vendor::Target],
    ) {
        let ms = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);
        let think_ms = i64::try_from(think.as_millis()).unwrap_or(i64::MAX);
        let res = ToolCatalogStore::open(&self.db).and_then(|s| {
            s.record_run(source, tool, how, ms, Some(think_ms), self.speaker.as_deref())?;
            // Per device as well as per tool, because "the office lamp never
            // works" is what people actually notice — and because one command
            // naming a room reaches several things with different fates, which
            // a single row for the tool cannot represent. Only servers that
            // name what they touched produce anything here.
            for t in targets {
                s.record_device_run(source, &t.name, t.landed)?;
            }
            Ok(())
        });
        if let Err(e) = res {
            debug!("actions: could not note that {tool} ran: {e}");
        }
    }

    /// How long the assistant has been thinking since it was last busy, and
    /// restart that clock. Called immediately before a call is sent.
    fn take_think_time(&self) -> std::time::Duration {
        let now = std::time::Instant::now();
        // A poisoned lock here would cost a timing figure, never a command,
        // so the elapsed time is simply unknown and reported as zero.
        let Ok(mut since) = self.idle_since.lock() else { return std::time::Duration::ZERO };
        let waited = now.saturating_duration_since(*since);
        *since = now;
        waited
    }

    /// The assistant is thinking again, as of now.
    fn resumed(&self) {
        if let Ok(mut since) = self.idle_since.lock() {
            *since = std::time::Instant::now();
        }
    }
}

/// Build the tool set for this turn, or `None` when the user has no tools
/// switched on — in which case the turn stays conversation-only and costs
/// nothing extra.
///
/// `speaker` is the enrolled name Fono recognised for this turn, when it
/// recognised one. It is only ever written next to a completed call, so the
/// page can say who a thing was done for; it is not sent anywhere.
pub fn build(cfg: &Config, paths: &Paths, speaker: Option<&str>) -> Option<Arc<ActionTools>> {
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
    // The rows that survived the endpoint check, kept so the rails describe
    // exactly the tools the model is being offered — no more, no fewer.
    let mut offered = Vec::with_capacity(rows.len());
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
            Runnable {
                endpoint,
                verify: r.verify_class,
                readback: r.readback_tool.clone(),
                schema: r.schema.clone(),
                source: r.source.clone(),
            },
        );
        offered.push(r);
    }
    if descriptors.is_empty() {
        return None;
    }
    info!("actions: {} tools offered to the assistant", descriptors.len());

    let runnable = Arc::new(runnable);
    let house = Arc::new(HouseFacts::learn(
        &store,
        &offered.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
    ));
    let journal = Journal {
        db: paths.tool_catalog_db(),
        speaker: speaker.map(ToString::to_string),
        idle_since: Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
    };
    let execute: fono_assistant::ToolExecFn = Arc::new(move |call: ToolCall| {
        let runnable = runnable.clone();
        let house = house.clone();
        let journal = journal.clone();
        Box::pin(async move {
            let name = call.name.clone();
            let think = journal.take_think_time();
            let started = std::time::Instant::now();
            let ran = run_one(&runnable, &house, call).await;
            // Filed against the server that offers it, so a name published by
            // two servers cannot credit the wrong one. A call to a tool nobody
            // offers has no row to write to and is simply not recorded.
            if let Some(r) = runnable.get(&name) {
                journal.note(&r.source, &name, ran.how, think, started.elapsed(), &ran.targets);
            }
            journal.resumed();
            ran.out
        })
    });
    let hint = cfg.assistant.tools.place_names.then(|| room_hint(&store)).flatten();
    let grammar = cfg.assistant.tools.grammar.then(|| rails(&store, &offered)).flatten();
    // These names go to the assistant model and nowhere else — never to the
    // speech recogniser, which is frequently a cloud service chosen for audio
    // alone. See `docs/privacy.md`.
    Some(Arc::new(ActionTools { descriptors, execute, hint, grammar }))
}

/// The rails a local model is held to while it writes a command.
///
/// Everything here comes from two places, and neither is a list somebody has to
/// keep up to date: each tool's own published schema, and what the house said
/// about itself when it was connected. The only vendor knowledge involved is
/// three field names, and it is asked for rather than assumed — a server whose
/// catalogue is not recognised supplies none, and gets constraints from its
/// schemas alone.
///
/// `None` whenever nothing usable could be derived, which leaves the model
/// exactly as free as it is today.
fn rails(store: &ToolCatalogStore, rows: &[fono_core::tool_catalog::ToolRow]) -> Option<String> {
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let fields = vendor::for_catalogue(&names).slot_fields();

    let mut slots = fono_core::tool_grammar::SlotValues::new();
    let mut described = Vec::new();
    if let Some(field) = fields.place {
        if let Ok(places) = store.place_names() {
            described.push(format!("{} rooms", places.len()));
            slots.set(field, places);
        }
    }
    if let Some(field) = fields.device {
        if let Ok(devices) = store.device_names() {
            described.push(format!("{} devices", devices.len()));
            slots.set(field, devices);
        }
    }
    if let Some(field) = fields.kind {
        if let Ok(mut kinds) = store.device_domains() {
            // Only the kinds this house actually contains, so a command cannot
            // ask for a kind of thing that is not here. `__all__` is the way to
            // still say "everything in this room" — without it a required kind
            // would cost the user that sentence entirely.
            described.push(format!("{} kinds of device", kinds.len()));
            kinds.push(fono_core::tool_grammar::ANY_KIND.to_string());
            slots.set(field, kinds);
        }
    }

    let g = fono_core::tool_grammar::build(rows, &slots);
    if let Some(text) = &g {
        info!(
            "actions: while writing a command the model is held to what this home reported{}{} \
             ({} bytes of rules)",
            if described.is_empty() { "" } else { " — " },
            described.join(", "),
            text.len()
        );
    } else {
        debug!("actions: nothing to hold the model to; commands stay unconstrained");
    }
    g
}

/// How many past invocations to show per tool. Enough to see whether a
/// failure is the standing state or a one-off, few enough that the panel
/// stays a summary rather than becoming a second history page.
const USES_PER_TOOL: usize = 4;

/// Group past invocations by tool, newest first, keeping a handful each.
///
/// Long payloads are cut here rather than in the browser so the response
/// stays small: a Home Assistant result is routinely a few kilobytes of
/// JSON, and two dozen tools' worth of them would dwarf everything else on
/// the page.
fn uses_by_tool(uses: &[ToolUse]) -> serde_json::Value {
    let mut out: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for u in uses {
        let slot = out
            .entry(u.tool.clone())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .expect("just inserted an array");
        if slot.len() >= USES_PER_TOOL {
            continue;
        }
        slot.push(serde_json::json!({
            "at": u.at,
            "said": u.said.as_deref().map(|s| clip(s, 240)),
            "speaker": u.speaker,
            "args": clip(&u.args, 400),
            "result": u.result.as_deref().map(|s| clip(s, 600)),
            "ok": u.ok,
        }));
    }
    serde_json::Value::Object(out)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).chain(std::iter::once('…')).collect()
}

/// Everything the Tools &amp; actions page needs beyond the tool list itself.
///
/// One payload, built by the same code that builds the prompt, from the same
/// store, at the same moment — so the page cannot show something the model was
/// not told. That is the whole point of it: the two worst bugs in this area
/// were both a mechanism working correctly while the only place anyone could
/// look sat in another crate, reporting something else.
///
/// Everything here is read-only and comes from the local store, so the page
/// renders instantly and no server is contacted.
///
/// `uses` is the recent tail of the conversation log — empty when the user
/// keeps no history, which the page states rather than hiding.
pub(crate) fn page_extras(
    cfg: &Config,
    store: &ToolCatalogStore,
    uses: &[ToolUse],
) -> serde_json::Value {
    let active = store.active_tools().unwrap_or_default();
    let names: Vec<&str> = active.iter().map(|r| r.name.as_str()).collect();
    // Which published field carries a room, a device and a kind — asked of the
    // vendor rather than assumed, exactly as the grammar asks. A server we do
    // not recognise reports none, and the page then says plainly that nothing
    // is held to the house.
    let slots = vendor::for_catalogue(&names).slot_fields();

    let devices = store.devices().unwrap_or_default();
    serde_json::json!({
        "grammar": cfg.assistant.tools.grammar,
        "place_names": cfg.assistant.tools.place_names,
        "slots": {
            "place": slots.place,
            "device": slots.device,
            "kind": slots.kind,
        },
        "any_kind": fono_core::tool_grammar::ANY_KIND,
        "house": {
            "places": store.place_names().unwrap_or_default(),
            "devices": devices,
            "kinds": store.device_domains().unwrap_or_default(),
        },
        // The literal sentences the model is given about this home, or nothing
        // when it is given none. Shown verbatim: paraphrasing it here would
        // recreate the very gap this page exists to close.
        "hint": cfg.assistant.tools.place_names.then(|| room_hint(store)).flatten(),
        "catalogue_hash": store.catalogue_hash().unwrap_or_default(),
        "offered": active.len(),
        // What each tool has actually been asked to do, in the user's own
        // words. Read back out of the ordinary transcript, so it is present
        // only while conversation history is kept.
        "uses": uses_by_tool(uses),
        "history_kept": cfg.conversations.enabled,
    })
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
///
/// The domain rule leads, and says *required*, because stating it second — after
/// "act on the room in one call" — did not work. A later trace, with that
/// wording in the prompt, still produced a bare `{"area": "Master bedroom"}`
/// and moved the curtains and the roller. Two things were wrong with putting it
/// second: the sentence opened with the permission ("act on the room in one
/// call") and only then qualified it, and the qualification was phrased as
/// advice ("pass that kind as the domain") rather than an obligation. The
/// one-call economy is a separate rule now, so it cannot be read as licence to
/// omit the domain.
/// The fourth rule exists because the model picked the wrong tool, not the
/// wrong target. Asked in Romanian and again in English to turn the bedroom
/// lights on, it reached for the brightness-and-colour tool and invented both
/// values; the house rejected the payload and the lights stayed off. The hint
/// had been entirely about *targeting* and silent on *choosing*, while a
/// couple of dozen near-identical signatures competed for the same request.
/// The fifth rule is the other half of that failure: nobody had mentioned
/// brightness, and a field is not a request.
///
/// Written as a numbered list rather than a paragraph, and shorter than the
/// prose it replaces. Verbosity is not instruction strength — the paragraph
/// version stated the domain rule at length and still failed to prevent a
/// domain-less call.
fn room_hint(store: &ToolCatalogStore) -> Option<String> {
    let names = store.place_names().ok()?;
    if names.is_empty() {
        return None;
    }
    let mut hint = format!(
        "Rooms in this home, named exactly as they must be used: {}.\n\
         Rules for acting on this home:\n\
         1. Never translate or invent a room or device name — pick the closest one listed.\n\
         2. Whenever the user says which kind of device they mean — the lights, the heating, \
         the blinds — the domain is required, for example {{\"area\": \"Master bedroom\", \
         \"domain\": [\"light\"]}}. Leave the domain out only when the user really meant \
         everything in the room, because without it the command reaches every switchable \
         device there and will open the blinds and start the air conditioning.\n\
         3. One call for a room, not one per device: a room plus a domain is a single call.\n\
         4. When the user names a device rather than a room, act on it by that name and do \
         not narrow the search to a room: a device's name often mentions somewhere it is not.\n\
         5. Use the simplest tool that does what was asked. A tool that sets brightness, \
         colour or temperature is only for when the user asked for that value; to switch \
         something on or off, use the plain on/off tool.\n\
         6. Fill in only the arguments the user actually asked for. Never invent a value \
         because the tool offers the field.",
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
                "\nDevices in this home, named exactly as they must be used: {}. \
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

/// Drop arguments the model filled in with nothing.
///
/// A small local model, asked to turn off the kitchen lights, sent
/// `{"area": "Kitchen", "domain": ["light"], "floor": null, "name":
/// "Kitchen lights"}`. Every field the tool advertises got a value, and two of
/// them were placeholders: `floor` was `null` and, in a sibling trace, `name`
/// was an empty string. Home Assistant answered *"Input validation error: None
/// is not of type 'string'"* and did nothing — twice in a row, with the user
/// repeating themselves and the model apologising each time. Nothing was
/// broken, and the model was one `null` away from a working command.
///
/// A key the caller did not mean to set and a key it left blank are the same
/// request, so the blank ones are removed before the server sees them: `null`,
/// the empty string, and the empty list, at the top level and inside nested
/// objects. Anything with a value is passed through untouched — this never
/// changes what was asked for, only stops us asking for it badly.
fn drop_empty_arguments(args: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    fn is_blank(v: &Value) -> bool {
        match v {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        }
    }
    match args {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, drop_empty_arguments(v)))
                .filter(|(_, v)| !is_blank(v))
                .collect(),
        ),
        other => other,
    }
}

/// Take back the one value Fono offered that no server accepts.
///
/// The rails make a field compulsory to stop it being forgotten, and offer
/// [`fono_core::tool_grammar::ANY_KIND`] as the way to still say "everything in
/// this room". Nothing outside Fono has ever heard of it, so it is removed here
/// and the field goes back to being absent — which is exactly what "everything"
/// has always meant to a server.
///
/// The gain is in the record rather than the behaviour: a command that meant the
/// whole room and one that forgot to say what it meant used to be the same
/// payload, and both open the blinds. Now they are told apart before this point,
/// and only the deliberate one gets here.
///
/// Left blank by [`drop_empty_arguments`] rather than deleted outright, so the
/// two rules compose and neither has to know about the other. Runs before the
/// schema check, because the value would fail it.
fn drop_any_kind(args: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    fn strip(v: Value) -> Value {
        match v {
            Value::String(s) if s == fono_core::tool_grammar::ANY_KIND => Value::Null,
            // The kind is usually a list, so "everything" arrives as a
            // one-element array and the whole array has to go: a list with the
            // placeholder taken out would ask for nothing at all.
            Value::Array(a) => {
                let cleaned: Vec<Value> = a
                    .into_iter()
                    .filter(|e| e.as_str() != Some(fono_core::tool_grammar::ANY_KIND))
                    .collect();
                Value::Array(cleaned)
            }
            other => other,
        }
    }
    match args {
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, strip(v))).collect()),
        other => other,
    }
}

/// What this home already told us about the things in it.
///
/// Built once per turn from the same store the rails come from. Only the one
/// fact worth acting on is kept: which kind of thing each named device is.
///
/// The default knows nothing, which leaves every call exactly as written.
#[derive(Default)]
struct HouseFacts {
    /// Which published argument carries a device name and which carries a kind,
    /// asked of the vendor rather than assumed. A server naming neither leaves
    /// everything below inert.
    slots: vendor::SlotFields,
    /// Device name, folded for comparison, to the single kind it is. A name
    /// this home uses for two kinds of thing is left out: there is no one
    /// answer, so there is nothing to correct to.
    kind_of: std::collections::HashMap<String, String>,
}

impl HouseFacts {
    fn learn(store: &ToolCatalogStore, tools: &[&str]) -> Self {
        let slots = vendor::for_catalogue(tools).slot_fields();
        let mut kind_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut ambiguous = Vec::new();
        for d in store.devices().unwrap_or_default() {
            let key = d.name.trim().to_lowercase();
            match kind_of.get(&key) {
                Some(kind) if *kind == d.domain => {}
                Some(_) => ambiguous.push(key),
                None => {
                    kind_of.insert(key, d.domain);
                }
            }
        }
        for key in ambiguous {
            kind_of.remove(&key);
        }
        Self { slots, kind_of }
    }

    /// Make the kind agree with the device that was named.
    ///
    /// A command names a thing in the house and then says what kind of thing it
    /// is. Only one of those is the model's to decide — the house published the
    /// other when it was connected, so a disagreement has exactly one right
    /// answer and no reason to cost a round trip.
    ///
    /// It cost several. Asked in plain English to turn the air conditioner off,
    /// a local model wrote `{"name": "Air conditioner", "domain": ["light"]}`;
    /// Home Assistant looked for a light by that name, found none, and reported
    /// a failure the model then read aloud. The same mistake broke four of the
    /// benchmark's cells and survived every rewording of the prompt, because
    /// the field is free and a plausible wrong value is as easy to write as the
    /// right one.
    ///
    /// Corrects rather than refuses: the device named is the request, and the
    /// kind is bookkeeping the caller should not have been asked for. Silent
    /// when the named device is unknown to us, when the kind already agrees,
    /// and for any server whose field names we do not know — in each of those
    /// the call goes out exactly as written.
    ///
    /// The corrected value keeps the shape it was written in, list or single
    /// value, because that is what the tool's own schema asks for.
    fn agree(&self, args: serde_json::Value) -> (serde_json::Value, Option<String>) {
        use serde_json::Value;
        let (Some(device_field), Some(kind_field)) = (self.slots.device, self.slots.kind) else {
            return (args, None);
        };
        let Some(map) = args.as_object() else { return (args, None) };
        let Some(named) = map.get(device_field).and_then(|v| v.as_str()) else {
            return (args, None);
        };
        let Some(kind) = self.kind_of.get(&named.trim().to_lowercase()) else {
            return (args, None);
        };
        let Some(written) = map.get(kind_field) else { return (args, None) };
        let agrees = match written {
            Value::String(s) => s == kind,
            Value::Array(a) => a.len() == 1 && a[0].as_str() == Some(kind.as_str()),
            // Anything else is not a kind we can read, so nothing is claimed.
            _ => return (args, None),
        };
        if agrees {
            return (args, None);
        }
        let note = format!(
            "{kind_field} was {}, but this home says {named} is a {kind}",
            written.to_string().trim_matches('"')
        );
        let fixed = match written {
            Value::Array(_) => Value::Array(vec![Value::String(kind.clone())]),
            _ => Value::String(kind.clone()),
        };
        let mut map = map.clone();
        map.insert(kind_field.to_string(), fixed);
        (Value::Object(map), Some(note))
    }
}

/// Check the arguments against what the server said it accepts.
///
/// Returns the server's own vocabulary for what is wrong, or `None` when
/// nothing obviously is.
///
/// A small local model asked to turn on the bedroom lights sent
/// `{"area": "Master bedroom", "brightness": 10, "color": "#FFFFFF"}` to a
/// tool whose `color` is an enumeration of colour names. Nobody had mentioned
/// brightness or colour; the model filled the fields in because they were
/// there. Home Assistant answered *"Received invalid slot info"* and did
/// nothing, twice, in two languages.
///
/// Catching that here rather than at the house is worth a round trip, but the
/// real value is the sentence: an argument named against the schema the model
/// was shown is a correction it can act on, where a server's rejection of the
/// whole payload often is not.
///
/// Deliberately shallow. Only two things are checked — the type of a value,
/// and membership of an enumeration — because those are the server's own
/// unambiguous statements about itself. Required fields are not enforced: an
/// advertised schema is routinely stricter than the behaviour behind it, and
/// refusing a call the house would have accepted is a worse failure than
/// letting it through.
fn schema_complaint(schema: &serde_json::Value, args: &serde_json::Value) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    let given = args.as_object()?;
    let mut bad = Vec::new();
    for (key, value) in given {
        let Some(spec) = props.get(key) else {
            // An argument the tool never advertised. Not necessarily wrong —
            // servers extend themselves — so it is passed through.
            continue;
        };
        if let Some(allowed) = spec.get("enum").and_then(|e| e.as_array()) {
            if !allowed.contains(value) {
                let names: Vec<String> = allowed.iter().map(ToString::to_string).collect();
                bad.push(format!("{key} must be one of {}", names.join(", ")));
                continue;
            }
        }
        if let Some(want) = spec.get("type").and_then(|t| t.as_str()) {
            if !matches_json_type(want, value) {
                bad.push(format!("{key} must be a {want}"));
            }
        }
    }
    (!bad.is_empty()).then(|| bad.join("; "))
}

/// Does a value match a JSON Schema `type` keyword?
///
/// `integer` accepts any number without a fractional part, matching the
/// specification: a model that writes `21.0` for a whole number of degrees is
/// not making the mistake this check is looking for.
fn matches_json_type(want: &str, value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match want {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => matches!(value, Value::Null),
        // A type keyword we do not understand, or a list of them. Saying
        // nothing is the safe answer.
        _ => true,
    }
}

/// Appended to a failure where nothing in the world moved.
///
/// The traces that motivated this all ended the same way: the house said in
/// plain words what was wrong with the request, Fono read the objection aloud,
/// and the user had to say the whole thing again. The correction was already
/// in the model's hands and nothing invited it to use it. One sentence does,
/// and it costs nothing on a call that worked.
///
/// Deliberately not a promise: a second failure is a real answer, and saying
/// so beats a third attempt.
const RETRY_INVITATION: &str = "Nothing was changed. If you can tell from this what was wrong \
     with the request, correct it and call the tool once more; otherwise tell the user plainly \
     what went wrong.";

/// Appended to a failure where part of the request may already have landed.
///
/// Says nothing about what did or did not happen, because at this rung we
/// genuinely do not know — a room-wide command that moved four things and
/// missed two is the common case. Only offered where running the tool again
/// is the same request as running it once, so a repeat cannot double an
/// effect on the parts that did work.
const RETRY_THE_REST: &str = "If you can tell from this what was wrong with the request, \
     correct it and call the tool once more for what was missed; otherwise tell the user \
     plainly which parts did not happen.";

/// What one call did, and how strongly Fono can say so.
///
/// The second field is not a restatement of `out.failed`. It carries the one
/// distinction only this function can make — whether success was *checked*
/// against the world, merely accepted by the server, or simply sent with
/// nothing knowable afterwards. Recovering that from the outside is
/// impossible, and guessing it would put a claim on the page that nothing
/// supports.
struct Ran {
    out: ToolOutcome,
    how: RunOutcome,
    /// The individual things in the home this call reached, when the server
    /// names them. Empty for every server Fono has no specific knowledge of,
    /// and for anything that never left — which is why nothing is recorded in
    /// those cases rather than recorded as a failure.
    targets: Vec<vendor::Target>,
}

/// Send one call to its server and record the timing, or describe why it
/// never landed.
///
/// Split out from [`run_one`] so the trace span and the two ways a send can
/// come back empty sit together, away from the judging of a successful answer.
async fn execute(
    r: &Runnable,
    call: &ToolCall,
    args: &serde_json::Value,
) -> Result<mcp_client::ToolResult, String> {
    // Running the command is the part the user is waiting on, and until this
    // span existed it was an unexplained gap between two model requests: a
    // real trace showed 587 ms of silence there with nothing to attribute it
    // to. Finished before the outcome is judged, so the timing measures the
    // server and not our reading of it.
    let span = current_span("tool.execute", "actions", ACTIONS_LANE);
    let called = mcp_client::call_tool(&r.endpoint, &call.name, args).await;
    // What was asked for and what the server said about it both belong here.
    // A trace of a command that never happened showed only the tool's name and
    // that something went wrong, which is not enough to tell a bad room name
    // from an unreachable server from a device that cannot do what was asked.
    let detail = match &called {
        Err(e) => Some(e.to_string()),
        Ok(res) if res.is_error => Some(res.text.clone()),
        Ok(_) => None,
    };
    if let Some(detail) = &detail {
        warn!(tool = %call.name, args = %call.arguments, "actions: {} refused: {detail}", call.name);
    }
    span.finish(serde_json::json!({
        "tool": call.name,
        "args": args,
        "answered": called.is_ok(),
        "server_error": called.as_ref().is_ok_and(|res| res.is_error),
        "error": detail.as_deref().map(|d| d.chars().take(300).collect::<String>()),
    }));
    match called {
        // The server was never reached, so nothing moved. Worth one more go:
        // the model may pick a different tool, and if it picks the same one
        // the second failure is the honest answer.
        Err(e) => Err(format!("{} could not be run: {e}", call.name)),
        // The server objected. Its own words are the most useful thing we
        // have: they tell the user why, and they are also precisely what the
        // model needs to correct itself. A refused call did nothing, so
        // trying again cannot double an effect.
        Ok(res) if res.is_error => Err(format!("{} failed: {}", call.name, brief(&res.text))),
        Ok(res) => Ok(res),
    }
}

/// Run one call the model asked for and describe what happened.
///
/// Never returns an error: a tool that failed is the news, not a fault in
/// the turn, and the user has to hear it.
async fn run_one(
    runnable: &std::collections::HashMap<String, Runnable>,
    house: &HouseFacts,
    call: ToolCall,
) -> Ran {
    // Two ways for a call to end badly, and they are not equally safe to
    // repeat. `nothing_happened` is for the cases where the request never
    // reached the world — an unknown tool, an unreachable server, a payload
    // the server refused — so a second go cannot double anything and is always
    // offered. `not_as_asked` is for the cases where something may already have
    // moved, and only the vendor can say whether asking again is the same
    // request as asking once.
    let nothing_happened = |s: String| Ran {
        out: ToolOutcome {
            summary: format!("{s} {RETRY_INVITATION}"),
            failed: true,
            retryable: true,
        },
        how: RunOutcome::Failed,
        targets: Vec::new(),
    };

    let Some(r) = runnable.get(&call.name) else {
        return nothing_happened(format!("There is no tool called {}.", call.name));
    };
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    let args = drop_empty_arguments(drop_any_kind(args));
    // Anything the house has already stated is not the model's to get wrong.
    let (args, corrected) = house.agree(args);
    if let Some(note) = corrected {
        debug!(tool = %call.name, "actions: corrected {}: {note}", call.name);
        current_instant(
            "tool.corrected",
            "actions",
            ACTIONS_LANE,
            serde_json::json!({ "tool": call.name, "args": args, "note": note }),
        );
    }

    // Never sent, so nothing moved and a correction is free. The complaint is
    // phrased against the schema the model was shown, which is a more useful
    // thing to hand back than the server's rejection of the whole payload.
    if let Some(complaint) = schema_complaint(&r.schema, &args) {
        warn!(tool = %call.name, args = %call.arguments, "actions: not sending {}: {complaint}", call.name);
        current_instant(
            "tool.rejected",
            "actions",
            ACTIONS_LANE,
            serde_json::json!({ "tool": call.name, "args": args, "complaint": complaint }),
        );
        return nothing_happened(format!("{} was not sent: {complaint}.", call.name));
    }

    let res = match execute(r, &call, &args).await {
        Ok(res) => res,
        // Either the server was never reached or it objected outright. Both
        // mean nothing moved, so both are safe to offer again.
        Err(complaint) => return nothing_happened(complaint),
    };

    // Which software answered decides what its answer means, and the answer
    // itself is the only thing that can say. Anything unrecognised gets no
    // opinion, so the rungs below simply do not fire.
    let vendor = vendor::for_result(&res.text);
    // Read once, off the reply, and carried on every outcome below — including
    // the half-done one, which is precisely where knowing *which* device was
    // left behind is the whole answer.
    let touched = vendor.targets(&res.text);
    let ok = |how: RunOutcome, s: String| Ran {
        out: ToolOutcome::worked(s),
        how,
        targets: touched.clone(),
    };

    // A command that may be safely asked for twice can be handed back for one
    // more attempt; one that names a change rather than an end state cannot,
    // because asking twice for two degrees warmer is four degrees.
    let not_as_asked = |s: String| Ran {
        out: ToolOutcome {
            summary: if vendor.repeatable(&call.name) {
                format!("{s} {RETRY_THE_REST}")
            } else {
                s
            },
            failed: true,
            retryable: vendor.repeatable(&call.name),
        },
        how: RunOutcome::Failed,
        targets: touched.clone(),
    };

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
            return not_as_asked(format!("{} did not work: {}", call.name, brief(&res.text)));
        }
        Some(vendor::Admission::PartlyWorked { failed }) => {
            return not_as_asked(format!(
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
    //
    // `checked` records whether the world was consulted and agreed. An unproven
    // check must not set it: "checked" and "the server did not complain" are
    // different claims, and the record is only worth keeping while it holds
    // them apart.
    let mut checked = false;
    match (&r.readback, r.verify) {
        (Some(rb), VerifyClass::PostCondition) if vendor.checks(&call.name) => {
            match confirm(r, vendor, rb, &call, &res.text).await {
                Some(Verdict::Contradicted) => {
                    // Deliberately not "nothing changed": the check may have
                    // found some devices obeying and others not, and claiming
                    // more than was observed is the mistake this rung exists
                    // to stop.
                    return not_as_asked(format!(
                        "{} was accepted, but the devices are not in the state you asked for.",
                        call.name
                    ));
                }
                Some(Verdict::Confirmed) => {
                    checked = true;
                    info!(tool = %call.name, "action confirmed");
                }
                // Unproven is not disproven: the weaker rungs stand.
                None => {}
            }
        }
        // Nothing observes this tool's effect, so "it was accepted" is the
        // strongest true statement available. Saying "done" here would be
        // inventing evidence.
        (_, VerifyClass::None) => {
            return ok(RunOutcome::Sent, format!("{} was sent. {}", call.name, brief(&res.text)));
        }
        _ => {}
    }
    ok(if checked { RunOutcome::Confirmed } else { RunOutcome::Accepted }, brief(&res.text))
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
    // Sequential with `tool.execute` and never nested inside it, so the two
    // costs read off the lane separately: proving a command landed is a whole
    // extra round trip to the same server, and it is charged to the same turn.
    let span = current_span("tool.verify", "actions", ACTIONS_LANE);
    let back = mcp_client::call_tool(&r.endpoint, readback, &empty).await;
    let verdict = match &back {
        Ok(back) => vendor.confirms(call, result, &back.text),
        Err(e) => {
            warn!("actions: could not check whether {} worked: {e}", call.name);
            None
        }
    };
    span.finish(serde_json::json!({
        "tool": call.name,
        "readback": readback,
        "verdict": match verdict {
            Some(Verdict::Confirmed) => "confirmed",
            Some(Verdict::Contradicted) => "contradicted",
            None => "unproven",
        },
    }));
    verdict
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
                schema: serde_json::json!({}),
                source: "home".into(),
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
        let ran =
            run_one(&tools(VerifyClass::PostCondition), &HouseFacts::default(), call("Nope")).await;
        assert!(ran.out.failed, "an unknown tool is not a success");
        assert!(ran.out.summary.contains("no tool called Nope"), "{}", ran.out.summary);
        assert_eq!(ran.how, RunOutcome::Failed);
    }

    /// A server we cannot reach must say so in words the user can act on,
    /// rather than the turn failing or, worse, claiming success.
    #[tokio::test]
    async fn an_unreachable_server_is_reported_not_claimed_done() {
        let ran =
            run_one(&tools(VerifyClass::PostCondition), &HouseFacts::default(), call("HassTurnOn"))
                .await;
        assert!(ran.out.failed, "unreachable must not be logged as a success");
        assert!(ran.out.summary.starts_with("HassTurnOn could not be run"), "{}", ran.out.summary);
        assert!(!ran.out.summary.to_lowercase().contains("done"), "{}", ran.out.summary);
        // And it must be written down as a failure, not as "sent" — nothing
        // was sent, and a page saying otherwise would be worse than silent.
        assert_eq!(ran.how, RunOutcome::Failed);
    }

    /// Verbatim from a trace: asked to turn the kitchen lights off, a small
    /// local model filled in every field the tool advertises, two of them with
    /// nothing. Home Assistant rejected the whole command over the `null` and
    /// the light stayed on, twice in a row. A key left blank is a key the
    /// caller did not mean to set.
    #[test]
    fn a_blank_argument_is_not_sent_to_the_server() {
        let args = serde_json::json!({
            "area": "Kitchen",
            "domain": ["light"],
            "floor": null,
            "name": "",
            "device_class": [],
            "extra": {"nested": null, "kept": "yes"},
        });
        assert_eq!(
            drop_empty_arguments(args),
            serde_json::json!({
                "area": "Kitchen",
                "domain": ["light"],
                "extra": {"kept": "yes"},
            })
        );
    }

    /// Trimming must never change what was asked for: a real value of every
    /// shape survives, including the ones that look empty but are not.
    #[test]
    fn a_real_argument_is_passed_through_untouched() {
        let args = serde_json::json!({
            "brightness": 0,
            "on": false,
            "name": "Kitchen lights",
            "domain": ["light", "switch"],
        });
        assert_eq!(drop_empty_arguments(args.clone()), args);
    }

    /// The rails make the kind of device compulsory so it cannot be forgotten,
    /// and hand the model one word for "everything in this room". No server has
    /// heard of that word, so it must be taken back out before the call leaves —
    /// and taking it out has to mean the field is absent, which is what a server
    /// has always read as "everything".
    #[test]
    fn the_word_for_everything_never_reaches_the_server() {
        let args = serde_json::json!({
            "area": "Kitchen",
            "domain": [fono_core::tool_grammar::ANY_KIND],
        });
        // Both rules run together on the real path, in this order.
        assert_eq!(
            drop_empty_arguments(drop_any_kind(args)),
            serde_json::json!({ "area": "Kitchen" }),
            "the whole field goes, not just the word — a list with it removed asks for nothing"
        );

        // A bare string form is handled too, since a schema may not use a list.
        let args = serde_json::json!({ "domain": fono_core::tool_grammar::ANY_KIND });
        assert_eq!(drop_empty_arguments(drop_any_kind(args)), serde_json::json!({}));
    }

    /// A real kind of device must survive untouched, or asking for the lights
    /// only would silently become asking for everything in the room.
    #[test]
    fn a_real_kind_of_device_is_left_alone() {
        let args = serde_json::json!({ "area": "Kitchen", "domain": ["light"] });
        assert_eq!(drop_any_kind(args.clone()), args);
    }

    /// A house with one of each, and one name it uses twice.
    fn house() -> HouseFacts {
        let mut kind_of = std::collections::HashMap::new();
        kind_of.insert("air conditioner".to_string(), "climate".to_string());
        kind_of.insert("balcony lights".to_string(), "light".to_string());
        HouseFacts {
            slots: vendor::SlotFields {
                place: Some("area"),
                device: Some("name"),
                kind: Some("domain"),
            },
            kind_of,
        }
    }

    /// Verbatim from the benchmark, four cells over two languages: asked in
    /// plain words to turn the air conditioner off, the model named the right
    /// device and then called it a light. The house said otherwise when it was
    /// connected, so the disagreement has one answer and costs no round trip.
    #[test]
    fn a_kind_that_contradicts_the_named_device_is_corrected() {
        let (fixed, note) =
            house().agree(serde_json::json!({"name": "Air conditioner", "domain": ["light"]}));
        assert_eq!(fixed, serde_json::json!({"name": "Air conditioner", "domain": ["climate"]}));
        assert!(note.is_some_and(|n| n.contains("climate")), "the correction is written down");

        // Whatever shape it was written in is the shape the schema asked for.
        let (fixed, _) =
            house().agree(serde_json::json!({"name": "Air conditioner", "domain": "light"}));
        assert_eq!(fixed, serde_json::json!({"name": "Air conditioner", "domain": "climate"}));
    }

    /// Correcting must be silent whenever there is nothing it can prove: an
    /// agreeing kind, a device this home never mentioned, a room-wide command
    /// naming no device, and any server whose field names we do not know. In
    /// every one of those the call has to go out exactly as written.
    #[test]
    fn nothing_is_corrected_without_grounds() {
        let h = house();
        for args in [
            serde_json::json!({"name": "Air conditioner", "domain": ["climate"]}),
            serde_json::json!({"name": "Something else entirely", "domain": ["light"]}),
            serde_json::json!({"area": "Kitchen", "domain": ["light"]}),
            serde_json::json!({"name": "Air conditioner"}),
        ] {
            let (out, note) = h.agree(args.clone());
            assert_eq!(out, args, "left alone");
            assert_eq!(note, None);
        }

        let unknown = HouseFacts { slots: vendor::SlotFields::default(), ..house() };
        let args = serde_json::json!({"name": "Air conditioner", "domain": ["light"]});
        assert_eq!(unknown.agree(args.clone()), (args, None), "no field names, no opinion");
    }

    /// Verbatim from two traces, one in Romanian and one in English: asked to
    /// turn the bedroom lights on, the model reached for the brightness-and-
    /// colour tool and invented both values. `#FFFFFF` is not a colour that
    /// tool accepts, and the house threw the whole command away over it.
    /// Naming the offending argument against the schema the model was shown
    /// is a correction it can act on; "received invalid slot info" is not.
    #[test]
    fn a_value_outside_the_advertised_enumeration_is_named() {
        let schema = serde_json::json!({
            "properties": {
                "area": {"type": "string"},
                "color": {"enum": ["red", "white", "warm white"]},
            }
        });
        let args = serde_json::json!({"area": "Master bedroom", "color": "#FFFFFF"});
        let complaint =
            schema_complaint(&schema, &args).expect("an invented colour is a complaint");
        assert!(complaint.contains("color"), "{complaint}");
        assert!(
            complaint.contains("red"),
            "the allowed values belong in the sentence: {complaint}"
        );
    }

    /// A number where a word belongs is the other half of the same mistake,
    /// and just as cheap to catch before it costs a round trip.
    #[test]
    fn a_value_of_the_wrong_type_is_named() {
        let schema = serde_json::json!({"properties": {"name": {"type": "string"}}});
        let complaint = schema_complaint(&schema, &serde_json::json!({"name": 7}))
            .expect("a number is not a name");
        assert!(complaint.contains("name must be a string"), "{complaint}");
    }

    /// The check must stay quiet about everything it is not sure of. An
    /// advertised schema is routinely stricter than the behaviour behind it,
    /// so refusing a call the house would have accepted is the worse failure:
    /// a missing required field, an argument the tool never advertised, and a
    /// whole number written with a decimal point all pass.
    #[test]
    fn the_schema_check_refuses_only_what_the_server_plainly_forbids() {
        let schema = serde_json::json!({
            "properties": {
                "area": {"type": "string"},
                "temperature": {"type": "integer"},
            },
            "required": ["area", "missing_one"],
        });
        let args = serde_json::json!({
            "area": "Office",
            "temperature": 21.0,
            "undocumented": "servers extend themselves",
        });
        assert_eq!(schema_complaint(&schema, &args), None);
        // A tool that advertises nothing can never be complained about.
        assert_eq!(schema_complaint(&serde_json::json!({}), &args), None);
    }

    /// A call that never left Fono changed nothing, so the model is invited
    /// to correct it inside the same turn rather than the user being asked to
    /// say the whole thing again.
    #[tokio::test]
    async fn a_failure_that_changed_nothing_invites_one_correction() {
        let ran =
            run_one(&tools(VerifyClass::PostCondition), &HouseFacts::default(), call("Nope")).await;
        assert!(ran.out.failed);
        assert!(ran.out.retryable, "an unknown tool moved nothing, so a second go is free");
        assert!(ran.out.summary.contains("call the tool once more"), "{}", ran.out.summary);
    }

    /// The failure in two traces was choosing the wrong tool, not naming the
    /// wrong room: asked to switch lights on, the model reached for the
    /// brightness-and-colour tool and invented both values. The hint said
    /// nothing about choosing between a couple of dozen near-identical
    /// signatures, and nothing about leaving a field alone.
    #[test]
    fn the_room_hint_says_which_tool_to_use_and_not_to_invent_values() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string()]).expect("names");
        let hint = room_hint(&store).expect("names present means a hint");
        let lower = hint.to_lowercase();
        assert!(lower.contains("simplest tool"), "{hint}");
        assert!(lower.contains("on/off tool"), "{hint}");
        assert!(lower.contains("never invent a value"), "{hint}");
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
    ///
    /// Stating it was not enough. With the rule in the prompt but placed
    /// *after* "act on the room in one call", a later trace still sent a bare
    /// `{"area": "Master bedroom"}` and moved the curtains and the roller. So
    /// the ordering is part of the fix, and is asserted: the obligation comes
    /// before the economy, and it is phrased as an obligation.
    #[test]
    fn the_room_hint_asks_for_a_domain_when_the_user_named_a_kind_of_device() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string()]).expect("names");
        let hint = room_hint(&store).expect("names present means a hint");
        let lower = hint.to_lowercase();
        assert!(lower.contains("domain"), "{hint}");
        assert!(hint.contains("[\"light\"]"), "the worked example must survive: {hint}");
        assert!(lower.contains("domain is required"), "advice was not enough: {hint}");

        let domain_at = lower.find("the domain is required").expect("the obligation");
        let one_call_at = lower.find("one call for a room").expect("the economy");
        assert!(
            domain_at < one_call_at,
            "the obligation must come before the one-call economy, or the economy \
             reads as licence to omit the domain: {hint}"
        );
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
            .set_devices(
                "home",
                &[
                    fono_core::tool_catalog::Device::new("Office outdoor light", "light"),
                    fono_core::tool_catalog::Device::new("Hall lamp", "light"),
                ],
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
        let many: Vec<fono_core::tool_catalog::Device> = (0..=MAX_LISTED_DEVICES)
            .map(|i| fono_core::tool_catalog::Device::new(format!("Device number {i}"), "light"))
            .collect();
        store.set_devices("home", &many).expect("devices");
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

    /// A store standing in for a small Home Assistant: two rooms, two kinds of
    /// device, and the one tool the traces keep failing on.
    fn a_small_home() -> (ToolCatalogStore, Vec<fono_core::tool_catalog::ToolRow>) {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store
            .set_place_names("home", &["Office".to_string(), "Master bedroom".to_string()])
            .expect("rooms");
        store
            .set_devices(
                "home",
                &[
                    fono_core::tool_catalog::Device::new("Office outdoor light", "light"),
                    fono_core::tool_catalog::Device::new("Bedroom blind", "cover"),
                ],
            )
            .expect("devices");
        let rows = vec![fono_core::tool_catalog::ToolRow {
            source: "home".into(),
            name: "HassTurnOn".into(),
            description: String::new(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "area": {"type": "string"},
                    "name": {"type": "string"},
                    "domain": {"type": "array", "items": {"type": "string"}},
                },
            }),
            schema_hash: String::new(),
            capability: fono_core::tool_catalog::Capability::Safe,
            verify_class: fono_core::tool_catalog::VerifyClass::None,
            readback_tool: None,
            available: true,
            enabled: true,
            user_touched: false,
            runs: 0,
            last_run: None,
        }];
        (store, rows)
    }

    /// The rails are built from what the house reported and nothing else, so
    /// every room and device name it gave must appear, and the word for
    /// "everything in this room" must be offered alongside the real kinds.
    #[test]
    fn the_rails_are_built_from_what_the_home_reported() {
        let (store, rows) = a_small_home();
        let g = rails(&store, &rows).expect("a home with rooms and devices gives rails");
        assert!(g.contains("Office"), "{g}");
        assert!(g.contains("Master bedroom"), "{g}");
        assert!(g.contains("Office outdoor light"), "{g}");
        assert!(g.contains("light"), "{g}");
        assert!(g.contains("cover"), "{g}");
        assert!(
            g.contains(fono_core::tool_grammar::ANY_KIND),
            "the escape hatch must be there: {g}"
        );
        assert!(g.contains("HassTurnOn"), "the tool name must be pinned too: {g}");
    }

    /// The switch is the whole point of shipping this off by default: it has to
    /// be possible to run the same home with and without the rails and compare.
    /// A setting that is read but ignored would make that comparison a lie.
    #[test]
    fn the_switch_decides_whether_the_rails_exist_at_all() {
        let (store, rows) = a_small_home();
        // This mirrors the one line in `build` that consults the setting.
        let with = true.then(|| rails(&store, &rows)).flatten();
        let without = false.then(|| rails(&store, &rows)).flatten();
        assert!(with.is_some(), "on means rails");
        assert!(without.is_none(), "off means the model is exactly as free as before");
    }

    /// A house that said nothing about itself gives nothing to hold the model
    /// to, and must leave it unconstrained rather than fail or invent a menu.
    #[test]
    fn a_silent_home_leaves_the_model_free() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        let rows: Vec<fono_core::tool_catalog::ToolRow> = Vec::new();
        assert!(rails(&store, &rows).is_none());
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

    /// The page exists to close one gap: the model was told things nobody
    /// could see. So everything the prompt is built from has to reach it —
    /// the exact sentences about this home, the rooms and devices those
    /// sentences came from, which published field each of those lands in,
    /// and the fingerprint that says whether a warmed model is stale. A
    /// payload missing any of these leaves a question that can only be
    /// answered by reading a trace, which is the situation being fixed.
    #[test]
    fn the_page_is_told_everything_the_prompt_was_told() {
        let (store, _) = a_small_home();
        store
            .reconcile(
                "home",
                "sse",
                &[fono_core::tool_catalog::DiscoveredTool {
                    name: "HassTurnOn".into(),
                    description: "Turns on a device".into(),
                    schema: serde_json::json!({"type": "object"}),
                    capability: fono_core::tool_catalog::Capability::Safe,
                    verify_class: fono_core::tool_catalog::VerifyClass::PostCondition,
                    readback_tool: Some("GetLiveContext".into()),
                }],
            )
            .expect("reconcile");

        let mut cfg = Config::default();
        cfg.assistant.tools.place_names = true;
        let v = page_extras(&cfg, &store, &[]);

        assert_eq!(v["offered"], 1, "the count on the page must be the count in the prompt");
        let hint = v["hint"].as_str().expect("the sentences about this home must be shown");
        assert!(hint.contains("Office"), "{hint}");
        assert!(!v["catalogue_hash"].as_str().unwrap_or_default().is_empty());
        let places = v["house"]["places"].as_array().expect("rooms");
        assert!(places.iter().any(|p| p == "Office"), "{places:?}");
        let devices = v["house"]["devices"].as_array().expect("devices");
        assert!(devices.iter().any(|d| d["name"] == "Bedroom blind"), "{devices:?}");
        assert_eq!(v["any_kind"], fono_core::tool_grammar::ANY_KIND);
        // Which field carries a room is asked of the vendor, never assumed;
        // showing it is how a wrong guess becomes visible instead of costing
        // an afternoon in the traces.
        assert_eq!(v["slots"]["place"], "area", "{:?}", v["slots"]);

        // Switched off, the page must say so rather than show sentences the
        // model never received — a page that lies is worse than no page.
        cfg.assistant.tools.place_names = false;
        assert!(page_extras(&cfg, &store, &[])["hint"].is_null());
    }

    /// The page must show what a tool was actually asked to do, grouped by
    /// tool and capped, so one chatty tool cannot bury the rest.
    #[test]
    fn past_uses_are_grouped_per_tool_and_capped() {
        let use_of = |tool: &str, said: &str| ToolUse {
            tool: tool.into(),
            at: 0,
            args: r#"{"area":"Office"}"#.into(),
            said: Some(said.into()),
            speaker: Some("Bogdan".into()),
            result: Some("ok".into()),
            ok: Some(true),
        };
        let mut uses: Vec<ToolUse> =
            (0..9).map(|i| use_of("HassTurnOn", &format!("n{i}"))).collect();
        uses.push(use_of("HassTurnOff", "off please"));

        let v = uses_by_tool(&uses);
        let on = v["HassTurnOn"].as_array().expect("grouped under its tool");
        assert_eq!(on.len(), USES_PER_TOOL, "one busy tool must not fill the payload");
        assert_eq!(on[0]["said"], "n0", "newest first, as the query returned them");
        assert_eq!(v["HassTurnOff"].as_array().map(Vec::len), Some(1));
        assert!(v.get("GetLiveContext").is_none(), "a tool with no uses gets no entry");
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
        let tools =
            build(&cfg, &paths, Some("live test")).expect("no tools configured — nothing to test");
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
            execute: Arc::new(|_| Box::pin(async { ToolOutcome::worked(String::new()) })),
            hint: Some("Rooms: Kitchen".into()),
            grammar: None,
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
            store.set_devices(&s.name, &found.devices).expect("store devices");
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
        let actions = build(&cfg, &paths, Some("live test")).expect("no tools configured");
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
                system_prompt: crate::session::assistant_prompt_context(actions.hint.as_deref()),
                instructions: Some(cfg.assistant.prompt_main.clone()),
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
                system_prompt: crate::session::assistant_prompt_context(actions.hint.as_deref()),
                instructions: Some(cfg.assistant.prompt_main.clone()),
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
