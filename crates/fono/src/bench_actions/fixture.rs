// SPDX-License-Identifier: GPL-3.0-only
//! What a fixture asks for, and how an answer is scored.
//!
//! A fixture names no device. It states a requirement, an utterance with a
//! slot in it, and what must be true of the house afterwards — including what
//! must **not** have changed. The names are filled in from whatever house the
//! suite is pointed at (see [`super::house`]), which is what makes these files
//! safe to commit and portable to somebody else's home.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::house::Requirement;

/// A fixture file: a handful of cases sharing one server.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Bumped whenever a fixture changes, so an old report is never compared
    /// against a new suite and read as a regression.
    pub suite_version: u32,
    /// Which configured MCP server these cases need, by the name the user
    /// gave it in their config.
    pub server: String,
    #[serde(default)]
    pub cases: Vec<Case>,
}

/// One thing to say, in one language, and what should follow.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    /// Stable identifier. Appears in the shareable report, so it must not
    /// describe the user's house — `room_command_without_domain`, not
    /// `office_ac_stays_off`.
    pub id: String,
    /// Which failure class this defends against, for grouping in the report.
    pub class: Class,
    /// What the house must provide for this case to mean anything.
    pub requires: Requirement,
    /// Per-language utterances, keyed by language tag. The device and area
    /// slots are filled in from the resolved target.
    pub utterances: BTreeMap<String, String>,
    /// State the target must be driven into before the utterance, so the
    /// command has something to change. Without this the same case scores
    /// differently depending on what happened before it.
    #[serde(default)]
    pub precondition: Option<String>,
    /// What the target's state must be afterwards.
    #[serde(default)]
    pub expect_device: Option<String>,
    /// What the target's *level* must be afterwards, as a percentage — the
    /// brightness of a lamp, the volume of a speaker, how far open a blind is.
    ///
    /// Needed because [`Self::expect_device`] can only say `on` or `off`, and a
    /// command like "set the volume to seventy" is not about either. Without
    /// this a case that names a number has nothing to assert against the house,
    /// so a call the server refused outright still scored as a pass: the tool
    /// name was right, the argument was right, and nobody checked whether the
    /// speaker heard about it.
    ///
    /// Compared with [`Level::differs_from`]'s tolerance, because a device
    /// reports its level rounded and demanding an exact match would fail a
    /// command that worked.
    #[serde(default)]
    pub expect_level: Option<u8>,
    /// What the bystander's state must **still** be afterwards.
    ///
    /// This is the assertion the whole domain-less-room-command class rests
    /// on, and it cannot be made by inspecting the tool call: a room-wide
    /// switch-on is a perfectly well-formed call that also starts the air
    /// conditioning.
    #[serde(default)]
    pub expect_bystander_unchanged: bool,
    /// The tool the model ought to reach for. Scored on the **first** call
    /// only — whether the ladder later recovered is a separate number.
    #[serde(default)]
    pub expect_tool: Option<String>,
    /// Arguments the first call must carry, as a subset. Values are compared
    /// set-wise for arrays so `["light"]` matches regardless of ordering.
    #[serde(default)]
    pub expect_args: Option<serde_json::Value>,
    /// Arguments the first call must **not** carry — the invented-value case,
    /// where nobody mentioned brightness and the model filled the field in
    /// because it was there.
    #[serde(default)]
    pub forbid_args: Vec<String>,
    /// When true, this request must leave the house exactly as it found it: an
    /// ambiguous request, a question about state, a request to explain
    /// something. Without cases like these a benchmark rewards
    /// trigger-happiness.
    ///
    /// Judged on the house, not on the tool call. Answering *is the balcony
    /// light on?* takes a lookup, and the harness used to fail the model for
    /// making it — scoring correct behaviour as trigger-happiness. Which tools
    /// only read is a thing no server here states: Home Assistant publishes no
    /// annotations at all, so the only honest evidence is whether anything
    /// moved.
    #[serde(default)]
    pub expect_no_change: bool,
    /// When false, a second call to the same tool is itself a failure — the
    /// relative-change case, where asking twice for two degrees warmer is
    /// four degrees.
    #[serde(default = "yes")]
    pub retry_allowed: bool,
}

fn yes() -> bool {
    true
}

/// The failure class a case defends against.
///
/// Grouping by class is what turns the report from a score into a diagnosis:
/// six failures spread across six classes is a weak model, and six failures in
/// one class is a broken rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// The ordinary case: switch a named thing on or off.
    PlainCommand,
    /// A room command where the user said which kind of device they meant.
    RoomCommand,
    /// The wrong tool chosen among near-identical signatures.
    ToolChoice,
    /// A value nobody asked for, filled in because the field existed.
    InventedArgument,
    /// A device whose name mentions a room it is not in.
    MisleadingName,
    /// A command that must never be repeated.
    NonIdempotent,
    /// A request that must leave the house exactly as it found it.
    MustNotAct,
}

/// How one case came out.
///
/// Deliberately more than pass/fail. `Drifted` exists because a real house has
/// other actors in it — schedules, other people, a thermostat with opinions —
/// and calling that a failure is how a real-house suite becomes noise nobody
/// trusts. `Skipped` exists because a house without a media player has nothing
/// to say about media routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Right tool, right arguments, first try, and the world agrees.
    Passed,
    /// Wrong first try, but Fono's ladder recovered inside the same turn.
    /// The count of these *is* the measured value of the recovery machinery.
    Recovered,
    /// Still wrong when the turn ended.
    Failed,
    /// Something moved that this turn never touched.
    Drifted,
    /// The house has nothing this case could be run against.
    Skipped,
}

/// Everything scored about one case, keyed so it can be shared.
///
/// Nothing here names a device, an area, or repeats what was said — those live
/// in the detail file beside it. This is the layer a regression comparison
/// reads, which is why it has to be safe to publish: comparing runs must be
/// possible on a machine that has never seen the house.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseReport {
    pub id: String,
    pub class: Class,
    pub language: String,
    pub verdict: Verdict,
    /// Did the model's **first** call have the right name and arguments?
    pub routed_first_try: bool,
    /// Did the world end up as asked, whatever it took to get there?
    pub outcome_correct: bool,
    /// Did the bystander stay put? `None` when the case has no bystander.
    pub bystander_held: Option<bool>,
    /// Did the reply describe what actually happened?
    ///
    /// The worst available failure is a fluent claim of success over a dark
    /// lamp, and nothing else in the suite would catch it.
    pub reply_truthful: Option<bool>,
    /// Was the reply in the language the command was given in?
    pub reply_language_matched: Option<bool>,
    /// How many tool calls the model made. More than one means the ladder ran.
    pub calls: usize,
    pub elapsed_ms: u64,
    /// Why the case was skipped, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_because: Option<String>,
}

/// Does the first call carry everything the fixture asked for?
///
/// A subset comparison, not an equality one: a model that passes an extra
/// harmless field has not made the mistake this is looking for, and demanding
/// an exact payload would fail correct commands for no reason.
///
/// Arrays are compared as sets, because `["light"]` and `["light"]` in a
/// different order are the same request — while `["light", "switch"]` is not,
/// and that distinction is the whole domain-less-command class.
#[must_use]
pub fn args_match(expected: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::Object(want), serde_json::Value::Object(got)) => {
            want.iter().all(|(k, v)| got.get(k).is_some_and(|g| args_match(v, g)))
        }
        (serde_json::Value::Array(want), serde_json::Value::Array(got)) => {
            want.len() == got.len() && want.iter().all(|w| got.iter().any(|g| args_match(w, g)))
        }
        (serde_json::Value::String(want), serde_json::Value::String(got)) => {
            want.eq_ignore_ascii_case(got)
        }
        _ => expected == actual,
    }
}

/// The slots an utterance template may carry.
///
/// Named constants rather than literals so they read as data, not as an
/// accidental format string.
const DEVICE_SLOT: &str = "{device}";
const AREA_SLOT: &str = "{area}";

/// Fill the device and area slots into an utterance template.
#[must_use]
pub fn render(template: &str, device: &str, area: Option<&str>) -> String {
    template.replace(DEVICE_SLOT, device).replace(AREA_SLOT, area.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_subset_of_arguments_matches() {
        let want = json!({"area": "Office"});
        assert!(args_match(&want, &json!({"area": "Office", "domain": ["light"]})));
    }

    #[test]
    fn a_missing_argument_does_not_match() {
        assert!(!args_match(&json!({"area": "Office"}), &json!({"name": "Lamp"})));
    }

    /// Array order is not part of the request.
    #[test]
    fn arrays_compare_as_sets() {
        assert!(args_match(&json!({"d": ["light", "fan"]}), &json!({"d": ["fan", "light"]})));
    }

    /// An over-broad domain list is a different request and must be caught —
    /// this is the recorded failure where asking for a room's lights also
    /// started the air conditioning.
    #[test]
    fn an_over_broad_array_does_not_match() {
        assert!(!args_match(&json!({"d": ["light"]}), &json!({"d": ["light", "switch"]})));
    }

    /// Home Assistant matches area names case-insensitively and models vary
    /// in how they capitalise; that is not the failure being measured.
    #[test]
    fn strings_ignore_case() {
        assert!(args_match(&json!({"area": "office"}), &json!({"area": "Office"})));
    }

    /// Only some requirements guarantee a room name, and an `{area}` slot
    /// filled from a requirement that has none renders an empty string —
    /// which turns into a command about nothing.
    fn supplies_an_area(r: &Requirement) -> bool {
        matches!(r, Requirement::AreaWithBystander { .. } | Requirement::NamedArea { .. })
    }

    /// Requirements that pin a real device or room, and so belong only to a
    /// local fixture kept outside the repository.
    fn names_a_place(r: &Requirement) -> bool {
        matches!(r, Requirement::NamedDevice { .. } | Requirement::NamedArea { .. })
    }

    #[test]
    fn renders_both_slots() {
        assert_eq!(render("turn on the {device}", "Hall lamp", None), "turn on the Hall lamp");
        assert_eq!(
            render("aprinde luminile din {area}", "x", Some("Office")),
            "aprinde luminile din Office"
        );
    }

    /// The committed suite must parse, and every case must be sane.
    ///
    /// Worth a test because the alternative way to find a typo in a fixture is
    /// to discover it halfway through a run that is switching real lights on
    /// and off in someone's home.
    #[test]
    fn the_committed_suite_is_well_formed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/bench_actions");
        let mut files = 0;
        for entry in std::fs::read_dir(&dir).expect("the fixture directory exists") {
            let path = entry.expect("readable entry").path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            files += 1;
            let text = std::fs::read_to_string(&path).expect("readable fixture");
            let m: Manifest = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
            assert!(!m.cases.is_empty(), "{} has no cases", path.display());

            let mut seen = std::collections::BTreeSet::new();
            for c in &m.cases {
                assert!(seen.insert(c.id.clone()), "duplicate case id `{}`", c.id);
                assert!(
                    c.utterances.contains_key("en"),
                    "`{}` has no English utterance to fall back on",
                    c.id
                );
                // A case that expects no call cannot also demand a tool: the
                // two would contradict each other and the case could never
                // pass however the model behaved.
                assert!(
                    !(c.expect_no_change && c.expect_tool.is_some()),
                    "`{}` both forbids and requires a tool call",
                    c.id
                );
                // A bystander assertion needs a requirement that supplies one,
                // or it silently asserts nothing.
                if c.expect_bystander_unchanged {
                    assert!(
                        matches!(c.requires, Requirement::AreaWithBystander { .. }),
                        "`{}` checks a bystander but asks for no room that has one",
                        c.id
                    );
                }
                // An area slot only means something when a room was resolved.
                for (lang, text) in &c.utterances {
                    if text.contains(AREA_SLOT) {
                        assert!(
                            supplies_an_area(&c.requires),
                            "`{}` [{lang}] names a room but asks for no area",
                            c.id
                        );
                    }
                }
                // A committed fixture must not name a device or a room: that
                // is what makes the suite portable, and one leaked name is in
                // the history forever.
                assert!(
                    !names_a_place(&c.requires),
                    "`{}` names a specific device or area; keep that in a local fixture",
                    c.id
                );
            }
        }
        assert!(files > 0, "no fixtures found in {}", dir.display());
    }
}
