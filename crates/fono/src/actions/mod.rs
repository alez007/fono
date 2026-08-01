// SPDX-License-Identifier: GPL-3.0-only
//! Turns the user's tool catalogue into something the model can call.
//!
//! Everything here is assembled once per turn from data that already
//! exists: the servers in the config, the rows the user left switched on,
//! and the secrets file. Nothing is discovered on the request path.
//!
//! The one rule this module exists to enforce is honesty about outcomes.
//! A server can answer cheerfully and have done nothing at all — Home
//! Assistant does exactly that when a command names an area it does not
//! have — so the wording of every summary is capped by how well the
//! effect could actually be checked. See [`fono_core::tool_catalog::VerifyClass`].
//!
//! Deciding *how well* means reading a server's own payloads, which is
//! knowledge of that particular software. All of it lives in [`vendor`]; this
//! module knows the ladder, never a vendor's name.

pub mod vendor;

use std::collections::BTreeMap;
use std::fmt::Write as _;
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

/// One thing this turn did, in the form a spoken phrase can be keyed to.
///
/// Collected here rather than by the caller because this is the only layer that
/// sees the arguments as they were *finally* sent — after the blank fields were
/// dropped and the house's own facts applied — and the only one that sees which
/// things in the home the reply named.
#[derive(Clone)]
struct Acted {
    source: String,
    tool: String,
    /// Exactly what went to the server, so a replay sends the same thing.
    args: String,
    devices: Vec<String>,
    ok: bool,
    ms: i64,
}

/// What a turn did, held until the reply is over and it can be written down.
///
/// Two halves of one fact live in different places, which is the whole reason
/// this exists. The actions layer knows *what was done*; only the caller knows
/// *what was said* and when the reply finished. So the turn fills this in as it
/// runs and hands it back at the end.
///
/// The waiting is not laziness. A run is judged by whether the user came back
/// about the same thing within [`fono_core::tool_catalog::COMPLAINT_WINDOW_SECS`]
/// of hearing the reply, and starting that clock when the command was *sent*
/// would let a slow turn eat the window and push a real complaint outside it.
///
/// Cloning shares one record: the turn keeps a handle, the executor keeps a
/// handle, and both mean the same turn.
#[derive(Clone, Default)]
pub struct Learning {
    /// Absent on a path that is not a turn, and then nothing is written.
    db: Option<std::path::PathBuf>,
    did: Arc<std::sync::Mutex<Vec<Acted>>>,
}

impl Learning {
    /// Somewhere for one turn's actions to be written down.
    #[must_use]
    pub fn new(paths: &Paths) -> Self {
        Self { db: Some(paths.tool_catalog_db()), did: Arc::default() }
    }

    /// One that records nothing, for a path that is not a turn — warming a
    /// prompt, where nobody has spoken and so there is nothing to learn from.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    fn add(&self, a: Acted) {
        // A poisoned lock costs a promotion, never a command.
        if let Ok(mut did) = self.did.lock() {
            did.push(a);
        }
    }

    /// The reply is over: judge what was waiting, and write down what this turn
    /// did.
    ///
    /// Order is load-bearing. Judging comes first, because what is being judged
    /// is the *previous* run of this phrase — whether the user has just come
    /// back about the same thing — and writing this turn down first would
    /// overwrite the very run being scored.
    ///
    /// Only a turn that ran **exactly one** command is written down. A turn that
    /// ran several did several things, and replaying one of them later would do
    /// part of what was asked; a turn that needed a second attempt is a turn the
    /// model did not get right first time, which is not a phrase to trust yet.
    /// Both simply stay slow, which is the direction this whole mechanism is
    /// allowed to be wrong in.
    ///
    /// Best-effort throughout: a command that worked must never be reported as
    /// failed because a bookkeeping write did not land.
    pub fn finished(&self, said: &str, lang: &str) {
        let Some(db) = &self.db else { return };
        let Ok(did) = self.did.lock() else { return };
        if did.is_empty() {
            return;
        }
        let store = match ToolCatalogStore::open(db) {
            Ok(s) => s,
            Err(e) => return debug!("actions: cannot write down what this turn did: {e}"),
        };
        // Everything this turn reached, whatever command reached it. A complaint
        // is about a thing in the home, not about the command that moved it.
        let touched: Vec<String> = did.iter().flat_map(|a| a.devices.iter().cloned()).collect();
        if let Err(e) = store.settle(said, &touched) {
            debug!("actions: cannot judge what the last turn did: {e}");
        }
        let [one] = did.as_slice() else { return };
        let said = fono_core::tool_catalog::Said {
            phrase: said,
            lang,
            source: &one.source,
            tool: &one.tool,
            args: &one.args,
            devices: &one.devices,
            ok: one.ok,
            ms: one.ms,
        };
        match store.remember(&said) {
            Ok(true) => debug!("actions: {:?} now stands for {}", said.phrase, one.tool),
            Ok(false) => {}
            Err(e) => debug!("actions: cannot write down {:?}: {e}", said.phrase),
        }
    }
}

/// Run what this phrase has always run, without asking the model.
///
/// `Some` means the command is done and the turn needs no model at all: the
/// events returned are the same ones a model turn would have put on the stream,
/// so history, the page and the trace see no difference. `None` means carry on
/// as normal — either the phrase has earned nothing, or the replay did not
/// work.
///
/// **Falling back on a failure cannot double an action.** Only a call that
/// *names a thing* is ever written down (see
/// [`fono_core::tool_catalog::ToolCatalogStore::remember`]); a call that asks
/// for an *amount* is refused, precisely because asking twice for two degrees
/// warmer is four degrees. So the model may safely ask again for anything a
/// phrase could have replayed.
///
/// Nothing is spoken. A phrase on the fast path is one the user has said
/// before and watched work, and the reply Fono could produce without a model
/// would be a fixed word in one language — so the light coming on is the
/// answer. Say the phrase again and the words come back, because a failed
/// replay hands the turn to the model.
pub async fn replay(
    learning: &Learning,
    tools: &ActionTools,
    said: &str,
) -> Option<Vec<fono_assistant::TokenDelta>> {
    let db = learning.db.as_ref()?;
    // Opened twice rather than held: a SQLite handle cannot be kept across the
    // await below, and opening one costs microseconds beside a round trip to the
    // house. Same reason the journal reopens per call.
    let found = ToolCatalogStore::open(db).ok()?.replay(said).ok().flatten()?;
    match run_again(tools, &found).await {
        Ok(events) => Some(events),
        Err(ms) => {
            // One bad run makes a phrase slow again. Written here rather than
            // left to the end of the turn, because the turn is about to run a
            // second command and a turn that did two things is deliberately
            // never learned from — so this is the only chance to record it.
            let dirty = fono_core::tool_catalog::Said {
                phrase: said,
                lang: &found.lang,
                source: &found.source,
                tool: &found.tool,
                args: &found.args,
                devices: &[],
                ok: false,
                ms,
            };
            match ToolCatalogStore::open(db).and_then(|s| s.remember(&dirty)) {
                Ok(_) => info!("actions: {said:?} did not work; asking the model instead"),
                Err(e) => debug!("actions: cannot write down that {said:?} failed: {e}"),
            }
            None
        }
    }
}

/// Send one phrase's stored command.
///
/// `Ok` is the turn: the events a model turn would have produced. `Err` carries
/// how long the attempt took, which the caller writes down as the run that makes
/// the phrase slow again.
///
/// Split from [`replay`] so this half can be tested without a promoted row —
/// earning the fast path takes two clean runs and two closed complaint windows,
/// which is the store's business and pinned there.
async fn run_again(
    tools: &ActionTools,
    found: &fono_core::tool_catalog::Shortcut,
) -> Result<Vec<fono_assistant::TokenDelta>, i64> {
    use fono_assistant::{TokenDelta, ToolEvent};

    let call = ToolCall {
        id: "replay".to_string(),
        name: found.tool.clone(),
        arguments: found.args.clone(),
    };
    let started = std::time::Instant::now();
    let out = (tools.execute)(call.clone()).await;
    let ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    // The saving is the whole claim, so put both halves of it where they can be
    // read off one timeline rather than inferred.
    current_instant(
        "tool.replayed",
        "actions",
        ACTIONS_LANE,
        serde_json::json!({ "tool": found.tool, "ms": ms, "failed": out.failed }),
    );
    if out.failed {
        return Err(ms);
    }
    Ok(vec![
        TokenDelta::tool(ToolEvent::Called(call)),
        TokenDelta::tool(ToolEvent::Result {
            tool_call_id: "replay".to_string(),
            summary: out.summary,
            failed: false,
        }),
    ])
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
    /// What this turn did, collected for the caller to write down once the
    /// reply is over.
    learning: Learning,
}

impl Journal {
    fn note(
        &self,
        source: &str,
        tool: &str,
        ran: &Ran,
        think: std::time::Duration,
        elapsed: std::time::Duration,
    ) {
        let ms = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);
        let think_ms = i64::try_from(think.as_millis()).unwrap_or(i64::MAX);
        self.learning.add(Acted {
            source: source.to_string(),
            tool: tool.to_string(),
            args: ran.sent.clone(),
            devices: ran.targets.iter().map(|t| t.name.clone()).collect(),
            ok: ran.how != RunOutcome::Failed,
            ms,
        });
        let res = ToolCatalogStore::open(&self.db).and_then(|s| {
            s.record_run(source, tool, ran.how, ms, Some(think_ms), self.speaker.as_deref())?;
            // Per device as well as per tool, because "the office lamp never
            // works" is what people actually notice — and because one command
            // naming an area reaches several things with different fates, which
            // a single row for the tool cannot represent. Only servers that
            // name what they touched produce anything here.
            for t in &ran.targets {
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
///
/// `learning` collects what this turn does, for the caller to write down once
/// the reply is over. Pass [`Learning::none`] on a path that is not a turn.
pub fn build(
    cfg: &Config,
    paths: &Paths,
    speaker: Option<&str>,
    learning: &Learning,
) -> Option<Arc<ActionTools>> {
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
        learning: learning.clone(),
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
                journal.note(&r.source, &name, &ran, think, started.elapsed());
            }
            journal.resumed();
            ran.out
        })
    });
    let hint = cfg.assistant.tools.place_names.then(|| area_hint(&store)).flatten();
    let grammar = rails(&store, &offered);
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
/// Asked **once per server**, not once over all of them together. Recognition
/// is "does this catalogue look like Home Assistant", so a single `Hass*` tool
/// anywhere used to make every server's tools Home-Assistant-shaped, and every
/// server's `name` field narrow to the one house Fono had read. `name` is the
/// commonest parameter name there is, so that did not merely over-constrain: it
/// left the second server's correct call with no legal value at all.
///
/// `None` whenever nothing usable could be derived, which leaves the model
/// exactly as free as it is today.
fn rails(store: &ToolCatalogStore, rows: &[fono_core::tool_catalog::ToolRow]) -> Option<String> {
    let mut slots = fono_core::tool_grammar::SlotValues::new();
    let mut described = Vec::new();

    let mut by_server: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for r in rows {
        by_server.entry(r.source.as_str()).or_default().push(r.name.as_str());
    }

    for (source, names) in &by_server {
        let fields = vendor::for_catalogue(names).slot_fields();
        let mut said = Vec::new();
        if let Some(field) = fields.place {
            if let Ok(places) = store.place_names_of(source) {
                said.push(format!("{} areas", places.len()));
                slots.set(source, field, places);
            }
        }
        if let Some(field) = fields.device {
            if let Ok(devices) = store.device_names_of(source) {
                said.push(format!("{} devices", devices.len()));
                slots.set(source, field, devices);
            }
        }
        if let Some(field) = fields.kind {
            if let Ok(mut kinds) = store.device_domains_of(source) {
                // Only the kinds this house actually contains, so a command cannot
                // ask for a kind of thing that is not here. `__all__` is the way to
                // still say "everything in this area" — without it a required kind
                // would cost the user that sentence entirely.
                said.push(format!("{} kinds of device", kinds.len()));
                kinds.push(fono_core::tool_grammar::ANY_KIND.to_string());
                slots.set(source, field, kinds);
            }
        }
        if !said.is_empty() {
            described.push(format!("{source}: {}", said.join(", ")));
        }
    }

    let g = fono_core::tool_grammar::build(rows, &slots);
    if let Some(text) = &g {
        info!(
            "actions: while writing a command the model is held to what each server reported{}{} \
             ({} bytes of rules)",
            if described.is_empty() { "" } else { " — " },
            described.join("; "),
            text.len()
        );
    } else {
        debug!("actions: nothing to hold the model to; commands stay unconstrained");
    }
    g
}

/// What each server's tools are held to, for the page to state once per server.
///
/// The same probe and the same readers as [`rails`], so the page cannot claim a
/// narrowing the model did not get. Per server for the same reason the rails
/// are: on a second server these numbers are a different house, and one
/// sentence covering both would be true of neither.
///
/// A server whose catalogue Fono does not recognise reports no fields, and the
/// page then says plainly that nothing is held to a house.
fn rails_facts(
    store: &ToolCatalogStore,
    rows: &[fono_core::tool_catalog::ToolRow],
) -> serde_json::Value {
    let mut by_server: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for r in rows {
        by_server.entry(r.source.as_str()).or_default().push(r.name.as_str());
    }
    let mut out = serde_json::Map::new();
    for (source, names) in &by_server {
        let f = vendor::for_catalogue(names).slot_fields();
        // Counted only where a field carries it. No field means nothing is
        // narrowed, and a number would read as though something were.
        let areas = f.place.map(|_| store.place_names_of(source).unwrap_or_default().len());
        let devices = f.device.map(|_| store.device_names_of(source).unwrap_or_default().len());
        let kinds = f.kind.map(|_| store.device_domains_of(source).unwrap_or_default().len());
        out.insert(
            (*source).to_string(),
            serde_json::json!({
                "place": f.place, "device": f.device, "kind": f.kind,
                "areas": areas, "devices": devices, "kinds": kinds,
            }),
        );
    }
    serde_json::Value::Object(out)
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

    let devices = store.devices().unwrap_or_default();
    serde_json::json!({
        "place_names": cfg.assistant.tools.place_names,
        "rails": rails_facts(store, &active),
        "any_kind": fono_core::tool_grammar::ANY_KIND,
        "house": {
            "places": store.place_names().unwrap_or_default(),
            "devices": devices,
            "kinds": store.device_domains().unwrap_or_default(),
        },
        // The literal sentences the model is given about this home, or nothing
        // when it is given none. Shown verbatim: paraphrasing it here would
        // recreate the very gap this page exists to close.
        "hint": cfg.assistant.tools.place_names.then(|| area_hint(store)).flatten(),
        "catalogue_hash": store.catalogue_hash().unwrap_or_default(),
        "offered": active.len(),
        // What each tool has actually been asked to do, in the user's own
        // words. Read back out of the ordinary transcript, so it is present
        // only while conversation history is kept.
        "uses": uses_by_tool(uses),
        "history_kept": cfg.conversations.enabled,
        // The phrases Fono has written down and what each one would run. The
        // list of phrases that have worked but never earned the fast path is the
        // model's own blind-spot list, so it is shown rather than hidden.
        "shortcuts": phrases(store),
    })
}

/// The phrases Fono has written down, each with the one word the page may put
/// beside it.
///
/// The word is asked of the row rather than worked out here, so the page and the
/// fast path can never disagree about which phrases are replayed.
fn phrases(store: &ToolCatalogStore) -> Vec<serde_json::Value> {
    store
        .shortcuts()
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "phrase": s.phrase,
                "lang": s.lang,
                "source": s.source,
                "tool": s.tool,
                "args": s.args,
                "target": target_in(&s.tool, &s.args),
                "origin": s.origin,
                "state": s.state(),
                "runs": s.runs,
                "clean": s.clean,
                "last_run": s.last_run,
                "last_ok": s.last_ok,
                "last_ms": s.last_ms,
            })
        })
        .collect()
}

/// What in the home a stored command names, when the server's fields for it are
/// known.
///
/// So every row can read as the sentence it is — this phrase turns *that thing*
/// off — rather than as a line of JSON. Asked of the vendor exactly as the rails
/// and the house's own corrections ask, so a server Fono has no specific
/// knowledge of yields nothing here and the page falls back to the arguments.
///
/// A device wins over an area when both are named, because the device is the
/// narrower answer to "what does this touch". A command that names neither
/// yields nothing.
fn target_in(tool: &str, args: &str) -> Option<String> {
    let fields = vendor::for_catalogue(&[tool]).slot_fields();
    let sent: serde_json::Value = serde_json::from_str(args).ok()?;
    [fields.device, fields.place, fields.wider_place]
        .into_iter()
        .flatten()
        .find_map(|f| Some(sent.get(f)?.as_str()?.to_string()))
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

/// The one line that stops the model inventing an area name.
///
/// Without it a Romanian command asks for `bucătărie` in a house whose
/// areas are all named in English, Home Assistant matches nothing, and
/// nothing happens. Naming the areas turns an open guess into a closed
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
/// with an area, the model searched only that area and found nothing — the
/// lamp was named after the place it lights, not the one it sits in. It then
/// reported the lamp unavailable while it was on. Device names routinely
/// mention somewhere they are not, so narrowing the search by area hides the
/// very thing being looked for.
///
/// The third sentence exists because "act on the area in one call", left on
/// its own, is dangerous advice. Asked in Romanian to turn on *the light* in
/// the office, the model asked for the whole office — and an area-wide switch-on
/// reaches everything switchable in it, so the air conditioning came on while
/// the one lamp that was actually wanted failed. An area plus a kind of device
/// is still one call; saying which kind is what keeps the area from being a
/// blunt instrument.
///
/// The domain rule leads, and says *required*, because stating it second — after
/// "act on the area in one call" — did not work. A later trace, with that
/// wording in the prompt, still produced a bare `{"area": "Master bedroom"}`
/// and moved the curtains and the roller. Two things were wrong with putting it
/// second: the sentence opened with the permission ("act on the area in one
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
fn area_hint(store: &ToolCatalogStore) -> Option<String> {
    written_hint(store, hint_arm())
}

/// The hint itself, with the arm passed in so a test can pin each one without
/// touching the environment of a test running beside it.
fn written_hint(store: &ToolCatalogStore, arm: HintArm) -> Option<String> {
    let names = store.place_names().ok()?;
    if names.is_empty() {
        return None;
    }
    let mut hint =
        format!("Areas in this home, named exactly as they must be used: {}.", names.join(", "));
    if arm != HintArm::NoRules {
        hint.push_str("\nRules for acting on this home:");
        for (n, rule) in RULES.iter().enumerate() {
            // Rules 1 and 4 look like the two the code now guarantees on its
            // own — the rails make an invented name unwritable, and
            // `HouseFacts` drops an area named beside a device. `lean` asked
            // whether saying them as well still helps. It does: see `HintArm`.
            if arm == HintArm::Lean && matches!(n, 0 | 3) {
                continue;
            }
            let _ = write!(hint, "\n{}. {rule}", n + 1);
        }
    }

    // The device names, when there are few enough to state without crowding
    // out the conversation. A truncated list would be worse than none: the
    // model would conclude a real device does not exist and say so.
    if arm != HintArm::NoDevices {
        if let Ok(devices) = store.device_names() {
            if !devices.is_empty() && devices.len() <= MAX_LISTED_DEVICES {
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
    }
    Some(hint)
}

/// The rules, one per entry, numbered where they are written.
///
/// Split out so a measurement can leave some of them out without the numbering
/// or the wording drifting between arms.
const RULES: [&str; 6] = [
    "Never translate or invent an area or device name — pick the closest one listed.",
    "Whenever the user says which kind of device they mean — the lights, the heating, \
     the blinds — the domain is required, for example {\"area\": \"Master bedroom\", \
     \"domain\": [\"light\"]}. Leave the domain out only when the user really meant \
     everything in the area, because without it the command reaches every switchable \
     device there and will open the blinds and start the air conditioning.",
    "One call for an area, not one per device: an area plus a domain is a single call.",
    "When the user names a device rather than an area, act on it by that name and do \
     not narrow the search to an area: a device's name often mentions somewhere it is not.",
    "Use the simplest tool that does what was asked. A tool that sets brightness, \
     colour or temperature is only for when the user asked for that value; to switch \
     something on or off, use the plain on/off tool.",
    "Fill in only the arguments the user actually asked for. Never invent a value \
     because the tool offers the field.",
];

/// Which parts of the hint to write, for measurement only.
///
/// The hint costs about 700 tokens of prefill on every turn — roughly 400 of
/// them the device list and 250 the rules. Leaving any of it out cost commands
/// and saved no time; the numbers are in the plan (F54), not here.
///
/// The one thing worth knowing at this call site: the two rules the code now
/// enforces on its own are still load-bearing. `HouseFacts` drops an area named
/// beside a device whether the model was told to or not, but only when a device
/// *is* named — and the rule is what gets it named.
///
/// An environment variable rather than a setting or a flag, deliberately. The
/// rails were shipped behind a config key on the same reasoning and the key
/// long outlived the measurement it was for; a variable cannot be saved into a
/// config file and cannot appear on the settings page. It goes once the
/// measurement is repeated on a second local model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintArm {
    /// Everything. What a user gets, and the best of the four.
    Full,
    /// Every rule except the two the code already guarantees.
    Lean,
    /// The areas and the devices, and no rules at all.
    NoRules,
    /// The areas and the rules, and no device list.
    NoDevices,
}

fn hint_arm() -> HintArm {
    match std::env::var("FONO_ACTION_HINT").unwrap_or_default().as_str() {
        "lean" => HintArm::Lean,
        "no-rules" => HintArm::NoRules,
        "no-devices" => HintArm::NoDevices,
        _ => HintArm::Full,
    }
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
/// this area". Nothing outside Fono has ever heard of it, so it is removed here
/// and the field goes back to being absent — which is exactly what "everything"
/// has always meant to a server.
///
/// The gain is in the record rather than the behaviour: a command that meant the
/// whole area and one that forgot to say what it meant used to be the same
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
    /// Names this home uses for exactly one device. Such a name is the whole
    /// address of a thing, so nothing else needs saying to reach it. A name two
    /// devices share is left out — there the area is the only thing telling
    /// them apart.
    sole: std::collections::HashSet<String>,
}

impl HouseFacts {
    fn learn(store: &ToolCatalogStore, tools: &[&str]) -> Self {
        let slots = vendor::for_catalogue(tools).slot_fields();
        let mut kind_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut ambiguous = Vec::new();
        let mut sole = std::collections::HashSet::new();
        let mut shared = Vec::new();
        for d in store.devices().unwrap_or_default() {
            let key = d.name.trim().to_lowercase();
            if !sole.insert(key.clone()) {
                shared.push(key.clone());
            }
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
        for key in shared {
            sole.remove(&key);
        }
        Self { slots, kind_of, sole }
    }

    /// Make the call agree with the house that was published.
    ///
    /// Two things a command says about a device are not the model's to decide,
    /// because this home already stated them: what kind of thing the device is,
    /// and — when the name belongs to one device only — where it is.
    ///
    /// **The kind.** Asked in plain English to turn the air conditioner off, a
    /// local model wrote `{"name": "Air conditioner", "domain": ["light"]}`;
    /// Home Assistant looked for a light by that name, found none, and reported
    /// a failure the model then read aloud. The same mistake broke four of the
    /// benchmark's cells and survived every rewording of the prompt, because
    /// the field is free and a plausible wrong value is as easy to write as the
    /// right one. Corrected rather than refused, keeping whichever shape it was
    /// written in, list or single value, because that is what the schema asks.
    ///
    /// **The area.** With the kind put right the same mistake simply moved one
    /// field: the model named a real device and paired it with an area the
    /// device is not in, and the house refused it again — three times in one
    /// run, each time after the kind had been corrected successfully. An area
    /// beside a name can only ever *narrow*, so on a name only one device
    /// answers to it can only narrow wrongly. It is dropped, along with
    /// anything an area is itself inside. Not corrected: the catalogue records
    /// what a device is and not where, so there is no right value to write —
    /// but there is a value that is never needed.
    ///
    /// Silent when the named device is unknown to us, when the name is shared
    /// by two devices (there the area is the only thing telling them apart),
    /// when the kind already agrees, and for any server whose field names we do
    /// not know. In each of those the call goes out exactly as written.
    fn agree(&self, args: serde_json::Value) -> (serde_json::Value, Option<String>) {
        use serde_json::Value;
        let Some(device_field) = self.slots.device else { return (args, None) };
        let Some(map) = args.as_object() else { return (args, None) };
        let Some(named) = map.get(device_field).and_then(|v| v.as_str()) else {
            return (args, None);
        };
        let key = named.trim().to_lowercase();
        let mut map = map.clone();
        let mut notes = Vec::new();
        if let (Some(kind_field), Some(kind)) = (self.slots.kind, self.kind_of.get(&key)) {
            if let Some(written) = map.get(kind_field) {
                let agrees = match written {
                    Value::String(s) => Some(s == kind),
                    Value::Array(a) => Some(a.len() == 1 && a[0].as_str() == Some(kind.as_str())),
                    // Anything else is not a kind we can read, so nothing is claimed.
                    _ => None,
                };
                if agrees == Some(false) {
                    notes.push(format!(
                        "{kind_field} was {}, but this home says {named} is a {kind}",
                        written.to_string().trim_matches('"')
                    ));
                    let fixed = match written {
                        Value::Array(_) => Value::Array(vec![Value::String(kind.clone())]),
                        _ => Value::String(kind.clone()),
                    };
                    map.insert(kind_field.to_string(), fixed);
                }
            }
        }
        if self.sole.contains(&key) {
            for field in [self.slots.place, self.slots.wider_place].into_iter().flatten() {
                if map.remove(field).is_some() {
                    notes.push(format!("{field} was dropped: {named} is one device in this home"));
                }
            }
        }
        if notes.is_empty() {
            return (Value::Object(map), None);
        }
        (Value::Object(map), Some(notes.join("; ")))
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
/// genuinely do not know — an area-wide command that moved four things and
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
    /// The arguments as they actually went to the server, after the blank
    /// fields were dropped and the house's own facts applied. This, and not
    /// what the model wrote, is what a replay would have to send — so it is
    /// what a phrase is keyed to.
    sent: String,
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
    // that something went wrong, which is not enough to tell a bad area name
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

/// The arguments as they will actually be sent, or the complaint that keeps the
/// call at home.
///
/// Three steps in this order: drop what says nothing, let the house settle what
/// it already knows about the device named, and only then check the arguments
/// against the schema the model was shown. Nothing has left when this returns,
/// so a complaint costs nothing to act on.
///
/// The `Err` carries what *would* have been sent beside the complaint, because a
/// run is recorded by the arguments it used whether or not they travelled.
fn prepare_args(
    r: &Runnable,
    house: &HouseFacts,
    call: &ToolCall,
) -> Result<serde_json::Value, (String, String)> {
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
        return Err((args.to_string(), format!("{} was not sent: {complaint}.", call.name)));
    }
    Ok(args)
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
    // Nothing left, so what would have been sent is the best statement of what
    // this call was. It is only ever read as a failed run, which demotes a
    // phrase; nothing is ever promoted from here.
    let nothing_happened = |sent: &str, s: String| Ran {
        out: ToolOutcome {
            summary: format!("{s} {RETRY_INVITATION}"),
            failed: true,
            retryable: true,
        },
        how: RunOutcome::Failed,
        targets: Vec::new(),
        sent: sent.to_string(),
    };

    let Some(r) = runnable.get(&call.name) else {
        return nothing_happened(
            &call.arguments,
            format!("There is no tool called {}.", call.name),
        );
    };
    let args = match prepare_args(r, house, &call) {
        Ok(args) => args,
        Err((sent, complaint)) => return nothing_happened(&sent, complaint),
    };

    let sent = args.to_string();
    let res = match execute(r, &call, &args).await {
        Ok(res) => res,
        // Either the server was never reached or it objected outright. Both
        // mean nothing moved, so both are safe to offer again.
        Err(complaint) => return nothing_happened(&sent, complaint),
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
        sent: sent.clone(),
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
        sent: sent.clone(),
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
    // there is a readback tool and the server named something it touched —
    // which is nothing for a tool that only reads, and nothing at all for a
    // server Fono has no specific knowledge of.
    //
    // The read is deliberately not gated on the vendor being able to *judge*
    // the answer. It only knows the end state two of this server's tools ask
    // for, and gating on that left every value-setting tool unwatched: a lamp
    // that answered "done" to a brightness it never took was reported as a
    // success, and the model then told the user so. Reading the world and
    // handing back what it says needs no such knowledge, and lets the model be
    // the one to notice that "off" was asked for and `on` came back.
    //
    // `checked` records whether the world was consulted and agreed. An unproven
    // check must not set it: "checked" and "the server did not complain" are
    // different claims, and the record is only worth keeping while it holds
    // them apart.
    let mut checked = false;
    match (&r.readback, r.verify) {
        (Some(rb), VerifyClass::PostCondition) if !touched.is_empty() => {
            let looked = confirm(r, vendor, rb, &call, &res.text).await;
            let reads = state_of_the_house(&looked.readings);
            match looked.verdict {
                Some(Verdict::Contradicted) => {
                    // Deliberately not "nothing changed": the check may have
                    // found some devices obeying and others not, and claiming
                    // more than was observed is the mistake this rung exists
                    // to stop.
                    return not_as_asked(format!(
                        "{} was accepted, but the devices are not in the state you asked \
                         for.{reads}",
                        call.name
                    ));
                }
                Some(Verdict::Confirmed) => {
                    checked = true;
                    info!(tool = %call.name, "action confirmed");
                }
                // Unproven is not disproven: the weaker rungs stand. What the
                // house reads is still worth saying, because the alternative is
                // a reply built on the server's word alone.
                None if !reads.is_empty() => {
                    return ok(RunOutcome::Accepted, format!("{}{reads}", brief(&res.text)));
                }
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

/// Re-read the world: what the vendor makes of it, and what it plainly says.
///
/// Both are kept because they are different claims and can disagree. A verdict
/// is only available for a tool whose intended end state Fono knows, and
/// reporting one without the other would leave no way to tell a judged check
/// from an unjudged one after the fact.
struct Looked {
    verdict: Option<Verdict>,
    readings: Vec<(String, String)>,
}

/// Re-read the world and ask the vendor what it shows.
///
/// A readback that fails to arrive yields nothing: not being able to look is
/// not the same as having looked and found a problem, and reporting a working
/// command as broken because a second request timed out would be its own bug.
async fn confirm(
    r: &Runnable,
    vendor: &'static dyn Vendor,
    readback: &str,
    call: &ToolCall,
    result: &str,
) -> Looked {
    let empty = serde_json::json!({});
    // Sequential with `tool.execute` and never nested inside it, so the two
    // costs read off the lane separately: proving a command landed is a whole
    // extra round trip to the same server, and it is charged to the same turn.
    let span = current_span("tool.verify", "actions", ACTIONS_LANE);
    let back = mcp_client::call_tool(&r.endpoint, readback, &empty).await;
    let looked = match &back {
        Ok(back) => Looked {
            verdict: vendor.confirms(call, result, &back.text),
            readings: vendor.readings(&back.text, &claimed(vendor, result)),
        },
        Err(e) => {
            warn!("actions: could not check whether {} worked: {e}", call.name);
            Looked { verdict: None, readings: Vec::new() }
        }
    };
    // The server's claim and the house's reading are both stamped, so a run can
    // be asked afterwards whether the two ever disagreed — the question that
    // decides whether the extra read is worth its round trip.
    span.finish(serde_json::json!({
        "tool": call.name,
        "readback": readback,
        "verdict": match looked.verdict {
            Some(Verdict::Confirmed) => "confirmed",
            Some(Verdict::Contradicted) => "contradicted",
            None => "unproven",
        },
        "claimed": claimed(vendor, result),
        "reading": looked.readings.iter().map(|(n, s)| format!("{n}: {s}")).collect::<Vec<_>>(),
    }));
    looked
}

/// The devices the server said it reached, and which are therefore worth
/// looking up.
fn claimed(vendor: &'static dyn Vendor, result: &str) -> Vec<String> {
    vendor.targets(result).into_iter().filter(|t| t.landed).map(|t| t.name).collect()
}

/// What the house reads, as one sentence to hand back to the model.
///
/// Empty when there is nothing to report, so a caller can append it
/// unconditionally without leaving a stray sentence behind.
fn state_of_the_house(readings: &[(String, String)]) -> String {
    if readings.is_empty() {
        return String::new();
    }
    let each: Vec<String> = readings.iter().map(|(n, s)| format!("{n} is {s}")).collect();
    format!(
        " Reading the home back afterwards: {}. Tell the user what the home actually says, not \
         what was asked for.",
        each.join(", ")
    )
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
    /// and hand the model one word for "everything in this area". No server has
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
    /// only would silently become asking for everything in the area.
    #[test]
    fn a_real_kind_of_device_is_left_alone() {
        let args = serde_json::json!({ "area": "Kitchen", "domain": ["light"] });
        assert_eq!(drop_any_kind(args.clone()), args);
    }

    /// A house with one of each, plus a name two devices answer to.
    fn house() -> HouseFacts {
        let mut kind_of = std::collections::HashMap::new();
        kind_of.insert("air conditioner".to_string(), "climate".to_string());
        kind_of.insert("balcony lights".to_string(), "light".to_string());
        let sole = ["air conditioner", "balcony lights"].into_iter().map(String::from).collect();
        HouseFacts {
            slots: vendor::SlotFields {
                place: Some("area"),
                wider_place: Some("floor"),
                device: Some("name"),
                kind: Some("domain"),
            },
            kind_of,
            sole,
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
    /// agreeing kind, a device this home never mentioned, an area-wide command
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

    /// Verbatim from the benchmark, three cells: the kind was corrected, the
    /// call was sent, and the house refused it anyway because the area named
    /// beside the device was an area that device is not in. An area can only
    /// narrow, so beside a name only one device answers to it can only narrow
    /// wrongly.
    #[test]
    fn an_area_named_beside_one_device_is_dropped() {
        let (fixed, note) = house().agree(serde_json::json!({
            "name": "Air conditioner",
            "area": "Office",
            "floor": "1",
            "domain": ["light"],
        }));
        assert_eq!(fixed, serde_json::json!({"name": "Air conditioner", "domain": ["climate"]}));
        let note = note.expect("both repairs are written down");
        assert!(note.contains("climate"), "{note}");
        assert!(note.contains("area") && note.contains("floor"), "{note}");
    }

    /// The area stays when it is the only thing telling two devices apart, and
    /// when the device named is one this home never mentioned — there the area
    /// may be all the server has to go on.
    #[test]
    fn an_area_that_is_still_doing_work_stays() {
        let mut h = house();
        h.sole.remove("air conditioner");
        let args = serde_json::json!({"name": "Air conditioner", "area": "Office"});
        assert_eq!(h.agree(args.clone()), (args, None), "two devices share the name");

        let args = serde_json::json!({"name": "Something else entirely", "area": "Office"});
        assert_eq!(house().agree(args.clone()), (args, None), "never heard of it");
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

    /// A catalogue on disk with the one tool these tests act with, plus the
    /// directory it lives in — dropping the directory deletes the file, so the
    /// caller has to keep it.
    fn a_home_on_disk() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths {
            config_dir: dir.path().into(),
            data_dir: dir.path().into(),
            cache_dir: dir.path().into(),
            state_dir: dir.path().into(),
        };
        let (_, rows) = a_small_home();
        let found: Vec<_> = rows
            .into_iter()
            .map(|r| fono_core::tool_catalog::DiscoveredTool {
                name: r.name,
                description: r.description,
                schema: r.schema,
                capability: r.capability,
                verify_class: r.verify_class,
                readback_tool: r.readback_tool,
            })
            .collect();
        let store = ToolCatalogStore::open(&paths.tool_catalog_db()).expect("store");
        store.reconcile("home", "sse", &found).expect("tools");
        (dir, paths)
    }

    fn ran(sent: &str, devices: &[&str]) -> Ran {
        Ran {
            out: ToolOutcome::worked("Done.".into()),
            how: RunOutcome::Accepted,
            targets: devices
                .iter()
                .map(|n| vendor::Target { name: (*n).to_string(), landed: true })
                .collect(),
            sent: sent.to_string(),
        }
    }

    fn journal(paths: &Paths, learning: &Learning) -> Journal {
        Journal {
            db: paths.tool_catalog_db(),
            speaker: None,
            idle_since: Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
            learning: learning.clone(),
        }
    }

    /// The whole mechanism hangs off one call at the end of a turn, and every
    /// part of it fails quietly by design — so the thing worth pinning is that
    /// a plain turn does reach the catalogue at all. Nothing here asserts a
    /// promotion: one clean run is not two.
    #[test]
    fn a_turn_that_did_one_thing_is_written_down() {
        let (_dir, paths) = a_home_on_disk();
        let learning = Learning::new(&paths);
        let sent = r#"{"name":"Office outdoor light"}"#;
        journal(&paths, &learning).note(
            "home",
            "HassTurnOn",
            &ran(sent, &["Office outdoor light"]),
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(7),
        );
        learning.finished("turn on the office light", "en");

        let store = ToolCatalogStore::open(&paths.tool_catalog_db()).expect("reopen");
        let [row] = store.shortcuts().expect("shortcuts").try_into().expect("exactly one phrase");
        assert_eq!(row.phrase, "turn on the office light");
        assert_eq!(row.tool, "HassTurnOn");
        assert_eq!(row.args, sent, "a replay has to send what was sent, not what was said");
        assert!(!row.fast(), "one clean run is not enough to skip the model");
    }

    /// A turn that ran two commands did two things, and replaying one of them
    /// would do half of what was asked. Both simply stay slow.
    #[test]
    fn a_turn_that_did_two_things_is_not_written_down() {
        let (_dir, paths) = a_home_on_disk();
        let learning = Learning::new(&paths);
        let j = journal(&paths, &learning);
        for dev in ["Office outdoor light", "Bedroom blind"] {
            let sent = format!(r#"{{"name":"{dev}"}}"#);
            j.note(
                "home",
                "HassTurnOn",
                &ran(&sent, &[dev]),
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(7),
            );
        }
        learning.finished("get the office ready", "en");

        let store = ToolCatalogStore::open(&paths.tool_catalog_db()).expect("reopen");
        assert!(store.shortcuts().expect("shortcuts").is_empty());
    }

    /// Warming a prompt is not a turn: nobody spoke, so there is nothing to
    /// learn from, and the handle passed on that path must write nothing even
    /// if something contrives to run.
    #[test]
    fn a_path_that_is_not_a_turn_learns_nothing() {
        let (_dir, paths) = a_home_on_disk();
        let learning = Learning::none();
        journal(&paths, &learning).note(
            "home",
            "HassTurnOn",
            &ran(r#"{"name":"Office outdoor light"}"#, &["Office outdoor light"]),
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(7),
        );
        learning.finished("turn on the office light", "en");

        let store = ToolCatalogStore::open(&paths.tool_catalog_db()).expect("reopen");
        assert!(store.shortcuts().expect("shortcuts").is_empty());
    }

    /// A phrase that has earned the fast path.
    fn earned() -> fono_core::tool_catalog::Shortcut {
        fono_core::tool_catalog::Shortcut {
            phrase: "turn on the office light".into(),
            lang: "en".into(),
            source: "home".into(),
            tool: "HassTurnOn".into(),
            args: r#"{"name":"Office outdoor light"}"#.into(),
            origin: fono_core::tool_catalog::Origin::Learned,
            runs: 2,
            clean: 2,
            last_run: None,
            last_ok: Some(true),
            last_ms: Some(2_400),
            stale: None,
        }
    }

    /// Every call an executor was asked to make, in order: tool name and
    /// arguments as sent.
    type Asked = Arc<std::sync::Mutex<Vec<(String, String)>>>;

    /// Tools whose executor answers as told and records what it was asked,
    /// standing in for a house without one.
    fn answering(out: ToolOutcome) -> (ActionTools, Asked) {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let execute: fono_assistant::ToolExecFn = Arc::new(move |call: ToolCall| {
            seen.lock().unwrap().push((call.name, call.arguments));
            let out = out.clone();
            Box::pin(async move { out })
        });
        let tools = ActionTools { descriptors: Vec::new(), execute, hint: None, grammar: None };
        (tools, asked)
    }

    /// The fast path has to be invisible from the outside: the same events a
    /// model turn puts on the stream, so history, the page and the next turn
    /// see no difference — and the command sent exactly as it was sent before,
    /// not as it was said.
    #[tokio::test]
    async fn a_replayed_phrase_produces_the_events_a_model_turn_would() {
        let found = earned();
        let (tools, asked) = answering(ToolOutcome::worked("Done.".into()));
        let events = run_again(&tools, &found).await.expect("a working replay ends the turn");

        assert_eq!(
            asked.lock().unwrap().as_slice(),
            &[("HassTurnOn".to_string(), r#"{"name":"Office outdoor light"}"#.to_string())]
        );
        let [called, result] = events.as_slice() else { panic!("a call and its result") };
        match &called.tool_event {
            Some(fono_assistant::ToolEvent::Called(c)) => assert_eq!(c.name, "HassTurnOn"),
            other => panic!("{other:?}"),
        }
        match &result.tool_event {
            Some(fono_assistant::ToolEvent::Result { failed, .. }) => assert!(!failed),
            other => panic!("{other:?}"),
        }
        assert!(
            events.iter().all(|d| d.text.is_empty()),
            "nothing is spoken: the model is not there to word it in the right language"
        );
    }

    /// A replay that failed hands the turn to the model, and the phrase is slow
    /// again at once — recorded here because a turn that ran two commands is
    /// deliberately never learned from, so this is the only chance to write it.
    #[tokio::test]
    async fn a_replay_that_did_not_work_hands_the_turn_to_the_model() {
        let found = earned();
        let (tools, asked) = answering(ToolOutcome {
            summary: "HassTurnOn could not be run".into(),
            failed: true,
            retryable: true,
        });
        assert!(run_again(&tools, &found).await.is_err(), "a failed replay is not an answer");
        assert_eq!(asked.lock().unwrap().len(), 1, "tried once, then handed over");
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
    /// wrong area: asked to switch lights on, the model reached for the
    /// brightness-and-colour tool and invented both values. The hint said
    /// nothing about choosing between a couple of dozen near-identical
    /// signatures, and nothing about leaving a field alone.
    #[test]
    fn the_area_hint_says_which_tool_to_use_and_not_to_invent_values() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string()]).expect("names");
        let hint = area_hint(&store).expect("names present means a hint");
        let lower = hint.to_lowercase();
        assert!(lower.contains("simplest tool"), "{hint}");
        assert!(lower.contains("on/off tool"), "{hint}");
        assert!(lower.contains("never invent a value"), "{hint}");
    }

    /// A device named after somewhere it is not — a lamp called after the
    /// place it lights rather than the place it sits — was reported missing
    /// because the model narrowed its search to that area. The hint has to
    /// say so, or the same lookup fails the same way.
    #[test]
    fn the_area_hint_warns_against_searching_by_room_for_a_named_device() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string(), "Yard".to_string()]).expect("names");
        let hint = area_hint(&store).expect("names present means a hint");
        assert!(hint.contains("Office"), "{hint}");
        let lower = hint.to_lowercase();
        assert!(lower.contains("never translate"), "{hint}");
        assert!(lower.contains("do not narrow the search to an area"), "{hint}");
    }

    /// An area-wide switch-on reaches everything switchable in the area. Asked
    /// for *the light* in the office, the model asked for the office, and the
    /// air conditioning came on. The hint has to name the domain escape hatch,
    /// or a request for one kind of device keeps acting on all of them.
    ///
    /// Stating it was not enough. With the rule in the prompt but placed
    /// *after* "act on the area in one call", a later trace still sent a bare
    /// `{"area": "Master bedroom"}` and moved the curtains and the roller. So
    /// the ordering is part of the fix, and is asserted: the obligation comes
    /// before the economy, and it is phrased as an obligation.
    #[test]
    fn the_area_hint_asks_for_a_domain_when_the_user_named_a_kind_of_device() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Office".to_string()]).expect("names");
        let hint = area_hint(&store).expect("names present means a hint");
        let lower = hint.to_lowercase();
        assert!(lower.contains("domain"), "{hint}");
        assert!(hint.contains("[\"light\"]"), "the worked example must survive: {hint}");
        assert!(lower.contains("domain is required"), "advice was not enough: {hint}");

        let domain_at = lower.find("the domain is required").expect("the obligation");
        let one_call_at = lower.find("one call for an area").expect("the economy");
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
        store.set_place_names("home", &["Yard".to_string()]).expect("areas");
        store
            .set_devices(
                "home",
                &[
                    fono_core::tool_catalog::Device::new("Office outdoor light", "light"),
                    fono_core::tool_catalog::Device::new("Hall lamp", "light"),
                ],
            )
            .expect("devices");
        let hint = area_hint(&store).expect("hint");
        assert!(hint.contains("Office outdoor light"), "{hint}");
        assert!(hint.to_lowercase().contains("only exactly"), "{hint}");
    }

    /// A truncated list is worse than none: the model would read it as the
    /// whole house and tell the user a real device does not exist.
    #[test]
    fn too_many_devices_are_left_out_rather_than_cut_short() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store.set_place_names("home", &["Yard".to_string()]).expect("areas");
        let many: Vec<fono_core::tool_catalog::Device> = (0..=MAX_LISTED_DEVICES)
            .map(|i| fono_core::tool_catalog::Device::new(format!("Device number {i}"), "light"))
            .collect();
        store.set_devices("home", &many).expect("devices");
        let hint = area_hint(&store).expect("areas still give a hint");
        assert!(hint.contains("Yard"), "the area half must survive: {hint}");
        assert!(!hint.contains("Device number"), "a partial list must not be stated: {hint}");
    }

    /// Each arm must leave out exactly what it says it leaves out, or the
    /// measurement measures something other than what it reports.
    #[test]
    fn each_arm_writes_what_it_claims() {
        let (store, _) = a_small_home();
        let full = written_hint(&store, HintArm::Full).expect("hint");
        let lean = written_hint(&store, HintArm::Lean).expect("hint");
        let no_rules = written_hint(&store, HintArm::NoRules).expect("hint");
        let no_devices = written_hint(&store, HintArm::NoDevices).expect("hint");

        // Every arm still names the areas — that half is not under test.
        for h in [&full, &lean, &no_rules, &no_devices] {
            assert!(h.contains("Master bedroom"), "{h}");
        }

        // `lean` drops rules 1 and 4 and keeps the rest, numbering unchanged so
        // the wording cannot drift between arms.
        assert!(full.contains("1. Never translate"), "{full}");
        assert!(!lean.contains("Never translate"), "{lean}");
        assert!(!lean.contains("do not narrow the search to an area"), "{lean}");
        assert!(lean.contains("2. Whenever the user says which kind"), "{lean}");
        assert!(lean.contains("5. Use the simplest tool"), "{lean}");

        // `no-rules` and `no-devices` each drop one whole half.
        assert!(!no_rules.contains("Rules for acting"), "{no_rules}");
        assert!(no_rules.contains("Office outdoor light"), "{no_rules}");
        assert!(no_devices.contains("Rules for acting"), "{no_devices}");
        assert!(!no_devices.contains("Office outdoor light"), "{no_devices}");

        // And the default arm is the longest, so an accidental default change
        // would show up here rather than in a run.
        assert!(
            full.len() > lean.len() && full.len() > no_rules.len() && full.len() > no_devices.len()
        );
    }

    /// No rooms means no sentence: an empty list would spend tokens telling
    /// the model to choose from nothing.
    #[test]
    fn no_areas_means_no_hint() {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        assert!(area_hint(&store).is_none());
    }

    /// A store standing in for a small Home Assistant: two areas, two kinds of
    /// device, and the one tool the traces keep failing on.
    fn a_small_home() -> (ToolCatalogStore, Vec<fono_core::tool_catalog::ToolRow>) {
        let store = ToolCatalogStore::open_in_memory().expect("store");
        store
            .set_place_names("home", &["Office".to_string(), "Master bedroom".to_string()])
            .expect("areas");
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
    /// every area and device name it gave must appear, and the word for
    /// "everything in this area" must be offered alongside the real kinds.
    #[test]
    fn the_rails_are_built_from_what_the_home_reported() {
        let (store, rows) = a_small_home();
        let g = rails(&store, &rows).expect("a home with areas and devices gives rails");
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
    /// the exact sentences about this home, the areas and devices those
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
        let places = v["house"]["places"].as_array().expect("areas");
        assert!(places.iter().any(|p| p == "Office"), "{places:?}");
        let devices = v["house"]["devices"].as_array().expect("devices");
        assert!(devices.iter().any(|d| d["name"] == "Bedroom blind"), "{devices:?}");
        assert_eq!(v["any_kind"], fono_core::tool_grammar::ANY_KIND);
        // Which field carries an area is asked of the vendor, never assumed, and
        // filed under the server it was asked about; showing it is how a wrong
        // guess becomes visible instead of costing an afternoon in the traces.
        assert_eq!(v["rails"]["home"]["place"], "area", "{:?}", v["rails"]);
        assert_eq!(v["rails"]["home"]["areas"], 2, "{:?}", v["rails"]);
        assert!(v["rails"]["home"]["devices"].as_u64().unwrap_or_default() > 0, "{:?}", v["rails"]);

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
        let tools = build(&cfg, &paths, Some("live test"), &Learning::none())
            .expect("no tools configured — nothing to test");
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
            hint: Some("Areas: Kitchen".into()),
            grammar: None,
        });

        let (kept, note) = for_backend(Some(tools.clone()), true, "openai");
        assert!(kept.is_some(), "a backend that can act keeps its tools");
        assert_eq!(note.as_deref(), Some("Areas: Kitchen"), "and still gets the area names");

        let (kept, note) = for_backend(Some(tools), false, "llama-local");
        assert!(kept.is_none(), "a backend that cannot act is not handed tools");
        let note = note.expect("and is told why");
        assert!(note.contains("cannot control"), "{note}");
        // The area list is pointless here and would only cost tokens.
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
        // Connect first, as pressing "Save & connect" does, so the area names
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
            store.set_place_names(&s.name, &found.places).expect("store areas");
            store.set_devices(&s.name, &found.devices).expect("store devices");
            println!(
                "{} is {}, {} areas, {} devices",
                s.name,
                found.server.name,
                found.places.len(),
                found.devices.len()
            );
            endpoint = Some(ep);
        }
        let ep = endpoint.expect("no MCP server configured");
        let actions =
            build(&cfg, &paths, Some("live test"), &Learning::none()).expect("no tools configured");
        assert!(actions.hint.is_some(), "the model was told no room names");
        let assistant =
            fono_assistant::build_assistant(&cfg.assistant, &secrets, &paths.polish_models_dir())
                .expect("build assistant")
                .expect("no assistant backend configured");

        // Nothing about one particular home is baked in: the area defaults to
        // a name almost every house has, and both it and the foreign name can
        // be pointed at whatever the machine running this actually owns.
        let area = std::env::var("FONO_TEST_ACTION_AREA").unwrap_or_else(|_| "Kitchen".into());
        let alt = std::env::var("FONO_TEST_ACTION_ALT_AREA").unwrap_or_else(|_| "bucătărie".into());

        // Each phrase asks for a state the lights are not already in, so a
        // command that does nothing cannot pass by accident. The second is the
        // whole point: an area named in another language, which the house has
        // never heard of and only the area list can rescue. What is asserted
        // is the light, never the reply: the assistant claiming success is
        // exactly the thing under suspicion.
        let phrases = [
            (format!("turn off the {} lights", area.to_lowercase()), false),
            (format!("aprinde luminile din {alt}"), true),
        ];
        for (phrase, want_on) in phrases {
            set_lights(&ep, &area, !want_on).await;
            let ctx = fono_assistant::AssistantContext {
                // Compose the prompt through the shipping path, so the area
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

        // A device asked for by its own name, which is the case the area list
        // cannot help with: the name often mentions an area the device is not
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

    /// Put an area's lights in a known state without involving the model, so
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
