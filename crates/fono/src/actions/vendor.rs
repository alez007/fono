// SPDX-License-Identifier: GPL-3.0-only
//! Per-vendor knowledge of what a tool server's own answers mean.
//!
//! MCP says how to *call* a tool and whether the server raised an error. It
//! says nothing about what a tool was trying to do, so there is no
//! protocol-level way to tell "the light is on now" from "nothing happened".
//! Proving an action landed therefore needs a little knowledge of each
//! server's payloads, and this is the only place it is allowed to live.
//!
//! Two measurements shaped the interface.
//!
//! First, a server can answer cheerfully having done nothing at all: Home
//! Assistant returns an error-free result for a command naming a room it does
//! not have. So [`Vendor::admission`] exists, and "no error" is never
//! treated as proof. It also has to tell *nothing worked* from *some of it
//! worked*: a room-wide switch-on that started the air conditioning and left
//! the one lamp that was wanted untouched is neither a success nor a total
//! failure, and calling it either one misinforms the reply.
//!
//! Second, the obvious generic check — read the world before and after and see
//! whether anything changed — is unsound. Two readings of one real house three
//! seconds apart, with nothing happening, already differed: a soil-temperature
//! probe had drifted two tenths of a degree. A change detector would have
//! called that a successful light switch.
//!
//! [`Vendor::confirms`] avoids that trap by asking a different question. Not
//! *did anything change* but *is the world now as the user asked*, looking only
//! at the things the server itself claimed to have touched. Sensors drifting
//! elsewhere are irrelevant, only one extra read is needed rather than two, and
//! a command that asked for something already true is correctly confirmed
//! instead of being reported as having done nothing.
//!
//! Adding a vendor means writing one implementation here and one line in
//! [`for_server`]. Nothing outside this module knows any vendor's name.

use fono_assistant::ToolCall;

/// What a server's own error-free result admits about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Every target the command named was acted on.
    Worked,
    /// Nothing was touched — typically a name that matched no device.
    NothingWorked,
    /// Some targets were acted on and others were not, named here so the reply
    /// can say which. A room-wide command routinely lands here.
    PartlyWorked { failed: Vec<String> },
}

/// What a post-condition check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The world is as the user asked. This is the only rung that proves it.
    Confirmed,
    /// The server accepted the command and the world disagrees.
    Contradicted,
}

/// What one family of tool servers' answers mean.
///
/// Every method may decline to answer. Declining is safe: the weaker rungs of
/// the ladder still apply, and Fono's wording drops to what they support.
pub trait Vendor: Send + Sync {
    /// Short identifier, for logs and traces.
    fn id(&self) -> &'static str;

    /// Is this result one of ours?
    ///
    /// Must be specific enough that another vendor's answer never matches, and
    /// is allowed to be wrong only in the direction of saying no.
    fn recognises(&self, result: &serde_json::Value) -> bool;

    /// What does an error-free result admit about what actually happened?
    ///
    /// `None` means this vendor cannot tell from the payload, which must not
    /// be read as success.
    fn admission(&self, _result: &str) -> Option<Admission> {
        None
    }

    /// Is a post-condition check worth the extra round trip for this tool?
    ///
    /// False for anything that only reads, or whose intent this vendor cannot
    /// infer — asking the house about itself is not free.
    fn checks(&self, _tool: &str) -> bool {
        false
    }

    /// Is running this tool a second time the same request as running it once?
    ///
    /// This is what decides whether a command that did not land may be handed
    /// back to the model for one more go. "Be on" and "be off" name a state the
    /// world should end in, so asking twice changes nothing; "two degrees
    /// warmer" names a change, and asking twice is four degrees.
    ///
    /// Defaults to false, which costs only the retry: a vendor that cannot tell
    /// gets the honest failure sentence instead of a second attempt, and that
    /// is the safe direction to be wrong in.
    fn repeatable(&self, _tool: &str) -> bool {
        false
    }

    /// Given a fresh reading of the world, is it as the user asked?
    ///
    /// `None` when this vendor cannot tell, which is not a failure.
    fn confirms(&self, _call: &ToolCall, _result: &str, _readback: &str) -> Option<Verdict> {
        None
    }

    /// Which individual things in the home this call actually reached, and
    /// whether each one landed.
    ///
    /// This is what lets Fono say "the office lamp has worked eleven times and
    /// the bedroom blind has never once" — a per-device history rather than a
    /// per-tool one. It has to be vendor knowledge and cannot be read off the
    /// arguments: one command naming a room reaches six devices the arguments
    /// never mention, and the reply is the only place their names appear.
    ///
    /// Empty by default, and empty is not "nothing worked" — it is "this server
    /// does not say", which is the truth for every server Fono has no specific
    /// knowledge of. Nothing is recorded in that case, rather than a row of
    /// zeroes that would read as failure.
    fn targets(&self, _result: &str) -> Vec<Target> {
        Vec::new()
    }

    /// Which argument of a tool holds a room, which holds a device, and which
    /// holds a kind of device.
    ///
    /// This is the only vendor knowledge the rails need, and it is deliberately
    /// three *field names* rather than a list of tools. A table of tool names
    /// would have to be corrected every time the server gained or renamed one —
    /// maintenance nobody signed up for — while a field name is part of the
    /// server's published interface and cannot move without breaking every
    /// other client too. A tool that has none of these fields is unaffected,
    /// which is why an unfamiliar server keeps exactly today's freedom.
    ///
    /// The default is empty, so a vendor that says nothing gets constraints
    /// derived from published schemas alone.
    fn slot_fields(&self) -> SlotFields {
        SlotFields::default()
    }

    /// Is this catalogue of tool names one of ours?
    ///
    /// Needed because the rails are built before any tool has run, so there is
    /// no result payload to recognise. Same one-sided rule as
    /// [`Self::recognises`]: it may be wrong only by saying no.
    fn recognises_catalogue(&self, _tools: &[&str]) -> bool {
        false
    }
}

/// One thing in the home a call reached, and whether it landed.
///
/// The name is whatever the server called it, passed through untouched. Fono
/// matches it against the device list it already learned from the same server,
/// so a name that does not match is dropped rather than invented as a new
/// device — a reply is evidence about the home, not a source of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    /// The server put this one under `success` rather than `failed`.
    pub landed: bool,
}

/// The argument names a server uses for the three things a house is made of.
///
/// `None` means "this server has no such field", and nothing is constrained
/// for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotFields {
    /// Holds a room name.
    pub place: Option<&'static str>,
    /// Holds a device name.
    pub device: Option<&'static str>,
    /// Holds a kind of device.
    pub kind: Option<&'static str>,
}

/// Pick the implementation for whichever software produced a result.
///
/// Recognition is by the shape of the answer rather than by a name the server
/// gave us earlier, for two reasons. It needs nothing remembered, so an
/// existing installation does not quietly lose its failure detection until the
/// next time it reconnects. And a server that declines to name itself, or names
/// itself something new, is still understood.
///
/// Falls back to [`Unknown`], which claims nothing — an unrecognised server
/// still works, it simply cannot be proved to have worked, and Fono says so
/// rather than pretending otherwise.
pub fn for_result(result: &str) -> &'static dyn Vendor {
    const KNOWN: &[&dyn Vendor] = &[&HomeAssistant];
    let parsed = serde_json::from_str::<serde_json::Value>(result).ok();
    for v in KNOWN {
        if parsed.as_ref().is_some_and(|p| v.recognises(p)) {
            return *v;
        }
    }
    &Unknown
}

/// Pick the implementation for a server we only know by the tools it offers.
///
/// The rails have to be built before anything has run, so there is no result
/// payload to go on — only the catalogue. Falls back to [`Unknown`], which
/// claims no field names, so an unrecognised catalogue is constrained by its
/// own published schemas and nothing else.
pub fn for_catalogue(tools: &[&str]) -> &'static dyn Vendor {
    const KNOWN: &[&dyn Vendor] = &[&HomeAssistant];
    for v in KNOWN {
        if v.recognises_catalogue(tools) {
            return *v;
        }
    }
    &Unknown
}

/// A server we have no specific knowledge of.
pub struct Unknown;

impl Vendor for Unknown {
    fn id(&self) -> &'static str {
        "unknown"
    }

    fn recognises(&self, _result: &serde_json::Value) -> bool {
        false
    }
}

/// Home Assistant, via its built-in MCP server.
pub struct HomeAssistant;

impl Vendor for HomeAssistant {
    fn id(&self) -> &'static str {
        "home-assistant"
    }

    /// Every intent result carries a `response_type`, and the ones worth
    /// judging carry a `data` object listing what was and was not touched.
    fn recognises(&self, result: &serde_json::Value) -> bool {
        result.get("response_type").is_some()
            && (result.pointer("/data/success").is_some()
                || result.pointer("/data/failed").is_some())
    }

    /// Home Assistant reports per-target outcomes inside an otherwise
    /// successful result: `{"data": {"success": [...], "failed": [...]}}`.
    ///
    /// An empty `success` where the field exists means no device was touched —
    /// which is exactly what happened when a Romanian command asked for a room
    /// named `bucătărie` in a house whose rooms are all named in English.
    ///
    /// A non-empty `failed` beside a non-empty `success` is a different animal
    /// and used to be reported as the same thing. Asked to turn on the light in
    /// the office, the house switched on the air conditioning, failed on the
    /// one lamp that was wanted, and Fono told the model the command had simply
    /// not worked. Both halves are news, so both are carried.
    ///
    /// Deliberately one-sided: a payload without these fields yields `None`
    /// rather than a verdict. Guessing the other way would report working
    /// commands as broken.
    fn admission(&self, result: &str) -> Option<Admission> {
        let v: serde_json::Value = serde_json::from_str(result).ok()?;
        let data = v.get("data").unwrap_or(&v);
        let failed = data.get("failed").and_then(|f| f.as_array());
        let success = data.get("success").and_then(|s| s.as_array());
        if failed.is_none() && success.is_none() {
            return None;
        }
        // An area is a grouping, not a device: a result whose only success is
        // the room itself touched nothing.
        let switched = success.is_some_and(|a| a.iter().any(is_entity));
        let missed: Vec<String> = failed
            .map(|a| a.iter().filter(|e| is_entity(e)).filter_map(name_of).collect())
            .unwrap_or_default();
        Some(match (switched, missed.is_empty()) {
            (false, _) => Admission::NothingWorked,
            (true, true) => Admission::Worked,
            (true, false) => Admission::PartlyWorked { failed: missed },
        })
    }

    fn checks(&self, tool: &str) -> bool {
        desired_state(tool).is_some()
    }

    /// The same two intents, and for the same reason: they name a state the
    /// world should end in rather than a change to make, so asking twice is
    /// asking once. Everything else — brightness, position, a temperature step
    /// — is left alone, because a wrong guess here doubles a real-world effect.
    fn repeatable(&self, tool: &str) -> bool {
        desired_state(tool).is_some()
    }

    fn confirms(&self, call: &ToolCall, result: &str, readback: &str) -> Option<Verdict> {
        let want = desired_state(&call.name)?;
        let touched = claimed_entities(result);
        // Nothing was claimed, so there is nothing to look up. The weaker rung
        // has already dealt with that case; saying "contradicted" here would
        // report the same failure twice in different words.
        if touched.is_empty() {
            return None;
        }
        let states = observed_states(readback, &touched);
        if states.is_empty() {
            return None;
        }
        // Every device the server said it switched must actually be in the
        // asked-for state. One lamp left behind is a half-done command, and
        // the user needs to hear that rather than "done".
        Some(if states.iter().all(|s| s == want) {
            Verdict::Confirmed
        } else {
            Verdict::Contradicted
        })
    }

    /// Read off the same two lists [`Self::admission`] judges, one entry at a
    /// time instead of one verdict for the lot.
    ///
    /// Areas are skipped for the reason they are skipped everywhere else here: a
    /// room is a grouping with no state of its own, so recording a run against
    /// it would put a history on something that cannot have one.
    fn targets(&self, result: &str) -> Vec<Target> {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(result) else { return Vec::new() };
        let data = v.get("data").unwrap_or(&v);
        let mut out = Vec::new();
        for (field, landed) in [("success", true), ("failed", false)] {
            let Some(list) = data.get(field).and_then(|f| f.as_array()) else { continue };
            out.extend(
                list.iter()
                    .filter(|e| is_entity(e))
                    .filter_map(name_of)
                    .map(|name| Target { name, landed }),
            );
        }
        out
    }

    /// The three words Home Assistant uses across its whole intent interface.
    ///
    /// They are part of its public API — every voice integration in existence
    /// sends these names — so they cannot change without breaking far more than
    /// Fono. That is the entire reason this is three field names and not a list
    /// of the couple of dozen tools a house currently exposes: a new release can
    /// add and rename tools freely and this stays correct, whereas a tool table
    /// would rot silently at every upgrade.
    fn slot_fields(&self) -> SlotFields {
        SlotFields { place: Some("area"), device: Some("name"), kind: Some("domain") }
    }

    /// Recognised by the intent-name prefix every one of its tools carries.
    ///
    /// One match is enough, and the prefix is specific enough that no other
    /// server would collide with it.
    fn recognises_catalogue(&self, tools: &[&str]) -> bool {
        tools.iter().any(|t| t.starts_with("Hass"))
    }
}

/// The state a tool is asking for, when its name says.
///
/// Only the plain on/off intents are covered. Brightness, colour and position
/// are deliberately absent: a wrong guess about what "set" meant would be a
/// confident false verdict, which is worse than no verdict.
fn desired_state(tool: &str) -> Option<&'static str> {
    match tool {
        "HassTurnOn" => Some("on"),
        "HassTurnOff" => Some("off"),
        _ => None,
    }
}

/// A device, as opposed to an area — which is a grouping with no state of its
/// own, and so neither evidence that anything happened nor a thing to read.
///
/// Anything that does not call itself an area counts, rather than only what
/// calls itself an entity: a target whose kind the server left out is still a
/// thing that was or was not switched, and dropping it would quietly turn a
/// half-done command back into a clean success.
fn is_entity(target: &serde_json::Value) -> bool {
    target.get("type").and_then(|t| t.as_str()) != Some("area")
}

fn name_of(target: &serde_json::Value) -> Option<String> {
    target.get("name").and_then(|n| n.as_str()).map(str::to_string)
}

/// The devices the server itself said it switched.
fn claimed_entities(result: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(result) else { return Vec::new() };
    let data = v.get("data").unwrap_or(&v);
    let Some(list) = data.get("success").and_then(|s| s.as_array()) else { return Vec::new() };
    list.iter().filter(|e| is_entity(e)).filter_map(name_of).collect()
}

/// Look up those devices in a fresh reading of the house.
///
/// `GetLiveContext` returns a block per device, keyed by the same name the
/// result used:
///
/// ```text
/// - names: Hall lamp
///   domain: light
///   state: 'on'
/// ```
///
/// Two kinds of silence are skipped rather than counted as wrong, because a
/// thing that cannot tell us about itself is missing evidence, not evidence of
/// failure: a device the reading does not mention, and one that is offline. The
/// second is not hypothetical — a real kitchen turned out to contain a lamp
/// Home Assistant happily reports switching while it sits `unavailable`, so
/// counting that as a contradiction would call a working command broken every
/// single time.
fn observed_states(readback: &str, wanted: &[String]) -> Vec<String> {
    // `GetLiveContext` hands the dump back as a JSON string under `result`,
    // where the newlines are escaped. Read through that when it is there: the
    // block-per-device parsing below needs real line breaks, and without this
    // every lookup silently finds nothing — which would read as "no evidence"
    // and quietly disable the whole check.
    let unwrapped = serde_json::from_str::<serde_json::Value>(readback)
        .ok()
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(str::to_string));
    let text = unwrapped.as_deref().unwrap_or(readback);

    let mut out = Vec::new();
    for block in text.split("- names: ").skip(1) {
        let (name, rest) = block.split_once('\n').unwrap_or((block, ""));
        if !wanted.iter().any(|w| w == name.trim()) {
            continue;
        }
        if let Some(state) = rest.lines().find_map(|l| l.trim().strip_prefix("state:")) {
            let state = state.trim().trim_matches('\'');
            if state == "unavailable" || state == "unknown" {
                continue;
            }
            out.push(state.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Payload shapes copied from a real house, with the device names changed.
    const SWITCHED_A_LAMP: &str = r#"{"speech": {}, "response_type": "action_done", "data": {"success": [{"name": "Hall", "type": "area"}, {"name": "Hall lamp", "type": "entity"}], "failed": []}}"#;
    const TOUCHED_NOTHING: &str =
        r#"{"response_type": "action_done", "data": {"success": [], "failed": []}}"#;
    /// A real office: the climate came on, the light did not.
    const HALF_DONE_ROOM: &str = r#"{"speech": {}, "response_type": "action_done", "data": {"success": [{"name": "Office", "type": "area"}, {"name": "Office air conditioner", "type": "entity"}], "failed": [{"name": "Office TV Light", "type": "entity"}]}}"#;

    fn call(name: &str) -> ToolCall {
        ToolCall { id: "1".into(), name: name.into(), arguments: "{}".into() }
    }

    #[test]
    fn a_server_we_do_not_know_claims_nothing() {
        let v = for_result(r#"{"status":"queued","id":7}"#);
        assert_eq!(v.id(), "unknown");
        assert_eq!(v.admission(SWITCHED_A_LAMP), None, "no opinion, not a verdict");
        assert!(!v.checks("HassTurnOn"), "no point paying for a read we cannot interpret");
        assert_eq!(v.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, ""), None);
        // Prose, or nothing at all, is nobody's payload.
        assert_eq!(for_result("Done.").id(), "unknown");
        assert_eq!(for_result("").id(), "unknown");
    }

    /// Recognition comes from the answer itself, not from a name the server
    /// gave us earlier. An installation that predates this check therefore
    /// keeps working without having to reconnect first — otherwise it would
    /// silently lose its failure detection, which is the exact bug this whole
    /// module exists to prevent.
    #[test]
    fn the_house_is_recognised_by_its_answer() {
        assert_eq!(for_result(SWITCHED_A_LAMP).id(), "home-assistant");
        assert_eq!(for_result(TOUCHED_NOTHING).id(), "home-assistant");
    }

    /// The first payload is the one that caused a complaint: it *worked* — a
    /// lamp came on — yet its trailing `"failed": []` made keyword matching
    /// log it as a failure.
    #[test]
    fn a_working_command_and_an_empty_one_are_told_apart() {
        let ha = HomeAssistant;
        assert_eq!(ha.admission(SWITCHED_A_LAMP), Some(Admission::Worked));
        assert_eq!(ha.admission(TOUCHED_NOTHING), Some(Admission::NothingWorked));
        assert_eq!(
            ha.admission(r#"{"data": {"success": [], "failed": [{"name": "x"}]}}"#),
            Some(Admission::NothingWorked)
        );
        // Nothing recognisable in it, so no verdict either way.
        assert_eq!(ha.admission("Done."), None);
        assert_eq!(ha.admission(r#"{"speech": {"plain": {"speech": "ok"}}}"#), None);
    }

    /// A per-device history needs the names out of the *reply*, not the
    /// arguments: the half-done office command named a room, and both device
    /// names it actually reached appear nowhere else. The room itself is not a
    /// device and must not collect a history of its own.
    #[test]
    fn each_thing_the_house_touched_is_named_separately() {
        let ha = HomeAssistant;
        assert_eq!(
            ha.targets(HALF_DONE_ROOM),
            vec![
                Target { name: "Office air conditioner".into(), landed: true },
                Target { name: "Office TV Light".into(), landed: false },
            ],
            "the area is skipped, and the one that failed is kept with its verdict"
        );
        assert_eq!(ha.targets(TOUCHED_NOTHING), vec![], "nothing was reached");
        // A server we do not know says nothing rather than guessing, so no
        // device anywhere gets a run recorded against it.
        assert_eq!(for_result(r#"{"ok":true}"#).targets(HALF_DONE_ROOM), vec![]);
        assert_eq!(ha.targets("Done."), vec![]);
    }

    /// The office payload that started this: asked for the light, the house
    /// switched on the air conditioning and failed on the lamp. Reporting that
    /// as "did not work" is as wrong as reporting it as done, and the names of
    /// the devices that were missed are the part the reply needs.
    #[test]
    fn a_half_done_command_is_neither_a_success_nor_a_failure() {
        let ha = HomeAssistant;
        let Some(Admission::PartlyWorked { failed }) = ha.admission(HALF_DONE_ROOM) else {
            panic!("a room with successes and failures worked in part");
        };
        assert_eq!(failed, vec!["Office TV Light".to_string()]);
    }

    /// The room itself always comes back as a success, so counting it as a
    /// device would report a command that matched nothing as half-done.
    #[test]
    fn a_room_on_its_own_is_not_a_device_that_was_switched() {
        let ha = HomeAssistant;
        let only_the_area = r#"{"response_type": "action_done", "data": {"success": [{"name": "Hall", "type": "area"}], "failed": []}}"#;
        assert_eq!(ha.admission(only_the_area), Some(Admission::NothingWorked));
    }

    /// A switch-on that partly failed may be asked for again — the lamps that
    /// obeyed are already in the state asked for, so a repeat cannot double
    /// anything. A relative change may not, because twice is twice as much.
    #[test]
    fn only_a_command_naming_an_end_state_may_be_asked_for_twice() {
        let ha = HomeAssistant;
        assert!(ha.repeatable("HassTurnOn"), "being on twice is being on");
        assert!(ha.repeatable("HassTurnOff"));
        assert!(!ha.repeatable("HassLightSet"), "a brightness step must not be doubled");
        assert!(!ha.repeatable("HassClimateSetTemperature"));
        // A server we do not recognise gets no second attempt: it is the safe
        // direction to be wrong in.
        assert!(!Unknown.repeatable("HassTurnOn"));
    }

    /// The point of the whole rung: the server said it switched a lamp, and
    /// the house is asked whether that is true.
    #[test]
    fn the_house_can_agree_or_disagree_with_the_server() {
        let ha = HomeAssistant;
        let lit = "- names: Hall lamp\n  domain: light\n  state: 'on'\n  areas: Hall\n";
        let dark = "- names: Hall lamp\n  domain: light\n  state: 'off'\n  areas: Hall\n";

        assert_eq!(
            ha.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, lit),
            Some(Verdict::Confirmed)
        );
        assert_eq!(
            ha.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, dark),
            Some(Verdict::Contradicted),
            "the server said it switched a lamp that is still off"
        );
        assert_eq!(
            ha.confirms(&call("HassTurnOff"), SWITCHED_A_LAMP, dark),
            Some(Verdict::Confirmed)
        );
    }

    /// Sensor readings drift on their own — two readings of one real house
    /// three seconds apart differed by two tenths of a degree with nothing
    /// happening. Looking only at what the server claimed to touch, and only
    /// at whether it is in the asked-for state, makes that irrelevant.
    #[test]
    fn a_drifting_sensor_cannot_fake_a_successful_switch() {
        let ha = HomeAssistant;
        let house = "- names: Soil temperature\n  domain: sensor\n  state: '25.5'\n\
                     - names: Hall lamp\n  domain: light\n  state: 'off'\n";
        assert_eq!(
            ha.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, house),
            Some(Verdict::Contradicted),
            "the lamp is off; a sensor moving elsewhere is not evidence"
        );
    }

    /// Three ways to have no opinion, none of which may be reported as a
    /// failure: a tool whose intent we cannot infer, a result claiming
    /// nothing, and a device the reading does not mention.
    #[test]
    fn what_cannot_be_judged_is_left_alone() {
        let ha = HomeAssistant;
        let lit = "- names: Hall lamp\n  domain: light\n  state: 'on'\n";
        assert!(!ha.checks("HassLightSet"), "brightness intent is not guessed at");
        assert_eq!(ha.confirms(&call("HassLightSet"), SWITCHED_A_LAMP, lit), None);
        assert_eq!(ha.confirms(&call("HassTurnOn"), TOUCHED_NOTHING, lit), None);
        assert_eq!(
            ha.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, "- names: Other\n  state: 'on'\n"),
            None,
            "a device the house did not mention is missing evidence, not failure"
        );
        assert!(ha.checks("HassTurnOn") && ha.checks("HassTurnOff"));
    }

    /// A device can be listed as switched and be offline, which a real kitchen
    /// demonstrated on the first attempt. It has no state to disagree with, so
    /// it must not drag a working command down with it.
    #[test]
    fn a_device_that_is_offline_does_not_condemn_the_rest() {
        let ha = HomeAssistant;
        let house = "- names: Hall lamp\n  domain: light\n  state: 'on'\n\
                     - names: Broken lamp\n  domain: light\n  state: 'unavailable'\n";
        let claimed = r#"{"response_type": "action_done", "data": {"success": [{"name": "Hall lamp", "type": "entity"}, {"name": "Broken lamp", "type": "entity"}], "failed": []}}"#;
        assert_eq!(
            ha.confirms(&call("HassTurnOn"), claimed, house),
            Some(Verdict::Confirmed),
            "one lamp is on and the other cannot say; that is not a contradiction"
        );
    }

    /// The house sends its state as an escaped JSON string, not as loose text.
    /// Failing to read through that wrapper would find no device anywhere,
    /// which looks exactly like "no evidence" — so the check would turn itself
    /// off and nobody would notice.
    #[test]
    fn the_reading_is_understood_however_the_house_wraps_it() {
        let ha = HomeAssistant;
        let dump = "Live Context:\n- names: Hall lamp\n  domain: light\n  state: 'on'\n";
        let wrapped = serde_json::json!({"result": dump}).to_string();
        assert_eq!(
            ha.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, &wrapped),
            Some(Verdict::Confirmed)
        );
        assert_eq!(
            ha.confirms(&call("HassTurnOn"), SWITCHED_A_LAMP, dump),
            Some(Verdict::Confirmed),
            "and plain text still works"
        );
    }
}
