// SPDX-License-Identifier: GPL-3.0-only
//! Find the devices a fixture needs in whatever house it is pointed at.
//!
//! This is what lets the fixtures be committed. A fixture never names a
//! device — it states a requirement ("any light", "an area with a light and
//! something else switchable in it") and the requirement is resolved here
//! against the live house. Nothing private is written down, and the same
//! suite runs on a one-lamp flat and a two-hundred-entity home without
//! anybody porting anything.
//!
//! It also keeps working when the house changes, which a hand-written map of
//! device names does not: a renamed lamp silently invalidates a map, and the
//! fixture that used it fails for a reason that has nothing to do with Fono.
//!
//! Two properties matter more than they look:
//!
//! - **Selection is deterministic.** Candidates are sorted and the first is
//!   taken. Two runs that pick different lamps are two runs that cannot be
//!   compared, which would defeat the whole harness.
//! - **An unsatisfiable requirement is a skip, not a failure.** A house with
//!   no media player has nothing to say about media routing, and scoring it
//!   zero would be a lie about the model.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use fono_assistant::mcp_client::{self, McpEndpoint};

/// The tool a Home Assistant offers for describing itself.
///
/// Kept here rather than imported because this module asks for the *whole*
/// world state, including domains the prompt never mentions — a benchmark has
/// to observe the air conditioning it must not disturb.
const LIVE_CONTEXT: &str = "GetLiveContext";

/// One entity as the house describes itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entity {
    /// The first name the dump lists, and the only one sent back to the
    /// server. Sorted first so selection order is by name and therefore
    /// stable.
    pub name: String,
    pub domain: String,
    /// The remaining names on the same entity.
    ///
    /// A `names:` field is a comma-separated alias list, exactly like
    /// `areas:` — a speaker recorded as `Office display, Boxa birou` is one
    /// device with an English name and a Romanian one, not two devices. Kept
    /// so [`House::find_by_name`] can be asked for either, while `name` stays
    /// something the server will actually match: sending the joined string as
    /// a `name` argument is refused outright, which is what made a fixture
    /// naming the Romanian alias fail to be put back after it ran.
    pub aliases: Vec<String>,
    /// Every area the entity is listed under, aliases included.
    pub areas: Vec<String>,
    /// The state string exactly as reported, when the dump carried one.
    pub state: Option<String>,
    /// Attributes seen on the block, used to tell a dimmable lamp from a
    /// plain one. Values are kept verbatim; only presence is usually needed.
    pub attributes: BTreeMap<String, String>,
}

impl Entity {
    /// Every name this entity answers to, leading name first.
    pub fn every_name(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    /// Whether this entity is one a benchmark may aim at.
    ///
    /// The exclusion is on *targeting only* and says nothing about what the
    /// model may call: the tool catalogue handed to the model is the one the
    /// user's assistant gets every day, untouched. Narrowing that would
    /// change the routing problem and make the benchmark measure something
    /// easier than the real thing.
    ///
    /// What is different about a benchmark is that it runs unattended and
    /// repeatedly. Choosing the front door as the fixture target under those
    /// conditions is not a risk worth taking for a number.
    #[must_use]
    pub fn safe_to_target(&self) -> bool {
        if UNSAFE_DOMAINS.contains(&self.domain.as_str()) {
            return false;
        }
        // A reading is not a control. A motion sensor reports `on` and `off`
        // exactly as a lamp does, so the restore pass — which reaches every
        // entity, not just the ones a fixture named — tried to switch one back
        // and the house refused outright, ending an otherwise clean run with a
        // warning that the home might not have been put back.
        if self.is_reading() {
            return false;
        }
        // A cover is usually a blind and occasionally a garage door. The
        // dump does not distinguish them, so the name has to — every name,
        // since a door recorded with a second-language alias is still a door
        // and this is a safety check, not a lookup.
        if self.domain == "cover" && self.every_name().any(looks_like_a_garage) {
            return false;
        }
        true
    }

    /// Whether this entity reports the world rather than changes it.
    ///
    /// Two callers want exactly this question and would otherwise each keep
    /// their own list: targeting (a reading cannot be commanded) and drift
    /// detection (a reading that moves is a thermometer doing its job, not
    /// somebody else in the house).
    #[must_use]
    pub fn is_reading(&self) -> bool {
        READ_ONLY_DOMAINS.contains(&self.domain.as_str())
    }

    /// Whether the entity reports a brightness level, which is how a
    /// dimmable lamp is told from a plain one without knowing the house.
    #[must_use]
    pub fn is_dimmable(&self) -> bool {
        self.attributes.contains_key("brightness")
    }

    /// The setting this entity carries beyond being on or off.
    ///
    /// Restoring on/off is not putting a house back: a lamp dimmed to a tenth,
    /// a speaker at one volume, a thermostat aimed at one temperature and a
    /// blind at one height are all states a run can change and a naive restore
    /// would silently round off to "on".
    ///
    /// Units are converted here, once, because **the dump and the tools that
    /// set these values disagree** and a restore that ignored that would be
    /// worse than none at all — it would confidently write the wrong number:
    ///
    /// - brightness is reported raw (0–255) and set as a percentage (0–100),
    ///   so restoring a lamp at `128` verbatim would leave it at half of half
    /// - volume is reported as a fraction (`0.1`) and set as a percentage,
    ///   so restoring `0.1` verbatim would mute a speaker that was audible
    #[must_use]
    pub fn level(&self) -> Option<Level> {
        let num = |k: &str| -> Option<f64> { self.attributes.get(k)?.trim().parse::<f64>().ok() };
        match self.domain.as_str() {
            "light" => Some(Level::BrightnessPct(as_pct(num("brightness")? / 255.0))),
            "media_player" => Some(Level::VolumePct(as_pct(num("volume_level")?))),
            "climate" => Some(Level::TargetTemperature(num("temperature")?)),
            "cover" => Some(Level::PositionPct(as_pct(num("current_position")? / 100.0))),
            _ => None,
        }
    }
}

/// A setting beyond on/off, already in the units the tool that sets it wants.
///
/// Percentages are held as integers because that is what the schemas take
/// (`minimum: 0, maximum: 100, type: integer`); keeping a float here would
/// invite a rounding difference between what was read and what is written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    /// Brightness, as `HassLightSet` wants it.
    BrightnessPct(u8),
    /// Volume, as `HassSetVolume` wants it.
    VolumePct(u8),
    /// The temperature a climate device is aiming at, in its own units.
    TargetTemperature(f64),
    /// How far open a cover is, as `HassSetPosition` wants it.
    PositionPct(u8),
}

impl Level {
    /// Whether two readings are far enough apart to be worth writing back.
    ///
    /// A lamp reports its brightness rounded, so reading `128` and writing
    /// `50%` reads back as `127` — restoring on every such difference would
    /// mean every run ends by nudging every device it looked at, forever.
    #[must_use]
    pub fn differs_from(self, other: Self) -> bool {
        match (self, other) {
            (Self::BrightnessPct(a), Self::BrightnessPct(b))
            | (Self::VolumePct(a), Self::VolumePct(b))
            | (Self::PositionPct(a), Self::PositionPct(b)) => a.abs_diff(b) > 1,
            (Self::TargetTemperature(a), Self::TargetTemperature(b)) => (a - b).abs() > 0.05,
            _ => true,
        }
    }
}

/// A 0.0–1.0 fraction as a whole percentage, clamped.
fn as_pct(fraction: f64) -> u8 {
    // `as` on a float saturates at the integer bounds in Rust, but clamping
    // first keeps a malformed reading from becoming a confident 0 or 100.
    (fraction * 100.0).round().clamp(0.0, 100.0) as u8
}

/// Domains a benchmark never selects as a target, whatever the house says.
///
/// Hardcoded rather than configurable on purpose. A setting that has to be
/// switched on to be safe is a setting that is off on the machine where it
/// mattered.
const UNSAFE_DOMAINS: &[&str] = &["lock", "alarm_control_panel"];

/// Domains that report the world rather than change it.
///
/// Excluded from targeting because there is nothing to target: a command
/// addressed to one is refused by the house, whoever sends it. Kept separate
/// from [`UNSAFE_DOMAINS`] because the reason differs — those are things a
/// benchmark *could* move and must not, these are things nobody can move.
const READ_ONLY_DOMAINS: &[&str] =
    &["sensor", "binary_sensor", "person", "sun", "device_tracker", "weather", "todo"];

/// Words that mark a cover as something heavier than a blind.
const GARAGE_WORDS: &[&str] = &["garage", "gate", "door", "poarta", "poartă", "garaj"];

fn looks_like_a_garage(name: &str) -> bool {
    let lower = name.to_lowercase();
    GARAGE_WORDS.iter().any(|w| lower.contains(w))
}

/// The house as it currently is.
#[derive(Debug, Clone, Default)]
pub struct House {
    pub entities: Vec<Entity>,
}

impl House {
    /// Read the whole world state from the server.
    pub async fn read(ep: &McpEndpoint) -> Result<Self> {
        let res = mcp_client::call_tool(ep, LIVE_CONTEXT, &serde_json::json!({}))
            .await
            .with_context(|| format!("ask the server for {LIVE_CONTEXT}"))?;
        if res.is_error {
            anyhow::bail!("{LIVE_CONTEXT} failed: {}", res.text);
        }
        Ok(Self::parse(&res.text))
    }

    /// Parse a live-context dump.
    ///
    /// A block-walking scrape rather than a schema, for the same reason
    /// [`mcp_client`] does it that way: the dump is prose-ish YAML written for
    /// a model to read. Anything unrecognised yields no entities, which
    /// surfaces as every fixture skipping — visibly wrong, rather than
    /// quietly scoring against an empty house.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let inner = inner_dump(text);
        let mut entities = Vec::new();
        let mut current: Option<Entity> = None;
        for line in inner.lines() {
            let t = line.trim();
            if let Some(n) = t.strip_prefix("- names:") {
                if let Some(e) = current.take() {
                    push_if_complete(&mut entities, e);
                }
                // Split on the same convention `areas:` uses. The first name
                // is the one sent back to the server; the rest are only ever
                // matched against.
                let mut names = n.split(',').map(unquote).filter(|n| !n.is_empty());
                current = Some(Entity {
                    name: names.next().unwrap_or_default(),
                    domain: String::new(),
                    aliases: names.collect(),
                    areas: Vec::new(),
                    state: None,
                    attributes: BTreeMap::new(),
                });
                continue;
            }
            let Some(e) = current.as_mut() else { continue };
            if let Some(d) = t.strip_prefix("domain:") {
                e.domain = unquote(d);
            } else if let Some(a) = t.strip_prefix("areas:") {
                e.areas =
                    a.split(',').map(unquote).filter(|n| !n.is_empty() && n.len() < 64).collect();
            } else if let Some(s) = t.strip_prefix("state:") {
                e.state = Some(unquote(s));
            } else if let Some((k, v)) = t.split_once(':') {
                let k = k.trim();
                // Only simple scalar attributes; nested structures are not
                // needed and would confuse the flat map.
                if !k.is_empty() && !k.contains(' ') {
                    e.attributes.insert(k.to_string(), unquote(v));
                }
            }
        }
        if let Some(e) = current.take() {
            push_if_complete(&mut entities, e);
        }
        entities.sort();
        entities.dedup();
        Self { entities }
    }

    /// Every entity in a given area, by exact area name.
    pub fn in_area<'a>(&'a self, area: &'a str) -> impl Iterator<Item = &'a Entity> + 'a {
        self.entities.iter().filter(move |e| e.areas.iter().any(|a| a == area))
    }

    /// All area names, sorted and deduplicated.
    #[must_use]
    pub fn areas(&self) -> Vec<String> {
        let mut a: Vec<String> =
            self.entities.iter().flat_map(|e| e.areas.iter().cloned()).collect();
        a.sort();
        a.dedup();
        a
    }

    /// Look up one entity by exact name.
    ///
    /// Aliases count: `get` is also how the harness re-finds an entity in a
    /// later reading of the house, and a fixture that named the Romanian alias
    /// must find the same device both times.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Entity> {
        self.entities.iter().find(|e| e.every_name().any(|n| n == name))
    }

    /// Look up one entity by a name a person would say.
    ///
    /// Three passes, narrowing: exact, then loose equality, then "every word
    /// the fixture gave appears in the name". The last is what finds
    /// `Salt-Lamp Salt Lamp` from `salt lamp` — houses accumulate integration
    /// prefixes and duplicated words that nobody says out loud.
    ///
    /// Every pass considers the aliases as well as the leading name, because a
    /// bilingual house records both on one entity and the fixture is entitled
    /// to name either. What comes back is still the entity, whose `name` is
    /// what the server will match.
    ///
    /// Ties break on the shortest name, which prefers the lamp itself over a
    /// same-named sensor or a second relay. Candidates are already sorted, so
    /// the answer is stable.
    #[must_use]
    pub fn find_by_name(&self, name: &str) -> Option<&Entity> {
        if let Some(e) = self.get(name) {
            return Some(e);
        }
        if let Some(e) = self.entities.iter().find(|e| e.every_name().any(|n| loose_eq(n, name))) {
            return Some(e);
        }
        let wanted = words(name);
        if wanted.is_empty() {
            return None;
        }
        self.entities
            .iter()
            .filter(|e| {
                e.every_name().any(|n| {
                    let have = words(n);
                    wanted.iter().all(|w| have.contains(w))
                })
            })
            .min_by_key(|e| e.name.len())
    }

    /// Whether the server can be asked for this entity by name at all.
    ///
    /// Home Assistant refuses a name that two entities answer to rather than
    /// picking one, and a real house has plenty: a `light` called `Couch` and
    /// a `media_player` called `Couch` are one word to a person and an
    /// impossible request to the server. Nobody — model, harness or user —
    /// can command such a device by name, so a fixture aimed at one measures
    /// the house rather than the model. Worse, it took the whole run down
    /// with it: the *staging* step could not switch the device either.
    #[must_use]
    pub fn addressable(&self, e: &Entity) -> bool {
        // One match is the entity itself; a second is a twin.
        self.entities.iter().filter(|o| o.every_name().any(|n| loose_eq(n, &e.name))).count() <= 1
    }

    /// Whether a fixture may aim at this entity: safe to move *and* reachable
    /// by name. The two questions are always asked together.
    #[must_use]
    pub fn targetable(&self, e: &Entity) -> bool {
        e.safe_to_target() && self.addressable(e)
    }

    /// The same house with some devices taken out of consideration.
    ///
    /// Used when the server refuses to act on a device it nonetheless listed —
    /// the run learns that the hard way, and re-resolves the fixture against
    /// the house minus the offender rather than losing the case. Matching is
    /// loose and covers aliases: the same physical device is out however the
    /// requirement asks for it.
    #[must_use]
    pub fn without(&self, names: &BTreeSet<String>) -> Self {
        if names.is_empty() {
            return self.clone();
        }
        Self {
            entities: self
                .entities
                .iter()
                .filter(|e| !e.every_name().any(|n| names.iter().any(|d| loose_eq(n, d))))
                .cloned()
                .collect(),
        }
    }
}

/// Split into lowercase alphanumeric words.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Equal once case, spacing and punctuation are set aside.
fn loose_eq(a: &str, b: &str) -> bool {
    words(a) == words(b)
}

/// A block with no domain is dropped rather than guessed at — the same rule
/// [`mcp_client::parse_devices`] follows, and for the same reason: aiming a
/// benchmark at an entity whose kind we invented is worse than skipping.
fn push_if_complete(out: &mut Vec<Entity>, e: Entity) {
    if !e.name.is_empty() && !e.domain.is_empty() {
        out.push(e);
    }
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['\'', '"']).trim().to_string()
}

/// Unwrap a live-context payload: JSON with the dump under `result`, or a
/// bare dump from a differently-shaped server.
fn inner_dump(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("result").and_then(serde_json::Value::as_str).map(str::to_string))
        .unwrap_or_else(|| text.to_string())
}

/// What a fixture needs, stated without naming anything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Requirement {
    /// Any single entity in a domain — "a light", "a media player".
    Device { domain: String },
    /// A device in a domain that also reports brightness. The dimmable-lamp
    /// case, which is where the wrong-tool failure lives: a model reaching
    /// for the brightness tool when asked simply to switch something on.
    DimmableDevice { domain: String },
    /// An area containing a device in `domain` **and** at least one device in
    /// a different switchable domain.
    ///
    /// This resolves the whole domain-less room command by itself, and it is
    /// the reason discovery beats a hand-written map: the bystander — the
    /// thing that must **not** move when the user asks for the lights — is
    /// found automatically, and it is whatever that house actually has.
    AreaWithBystander { domain: String },
    /// One device named outright.
    ///
    /// The committed suite must never use this — a fixture that names a
    /// device is a fixture about one house. It exists for **local** fixtures
    /// kept outside the repository, where the whole point is to reproduce a
    /// specific command that a specific device is known to answer.
    ///
    /// Matched loosely, because the name a person says ("the salt lamp") is
    /// rarely the name the house records ("Salt-Lamp Salt Lamp").
    NamedDevice { name: String },
    /// Every device of one domain in one named area, as a group.
    ///
    /// A room command is not a command to one device: "turn on the light in
    /// the master bedroom" is satisfied only when the room's lights are on,
    /// and a harness that watched a single lamp would call a half-lit room a
    /// pass. The group is what gets staged, scored and restored together.
    NamedArea { area: String, domain: String },
}

/// A requirement resolved against a real house.
#[derive(Debug, Clone)]
pub struct Target {
    /// The entity the utterance will name, and the one whose name fills the
    /// `{device}` slot. For a group this is the first member, chosen only so
    /// there is something to print.
    pub device: Entity,
    /// Every entity the command is expected to move.
    ///
    /// Usually just `device`. For a room command it is all the room's lights,
    /// because "turn on the light in the bedroom" is not satisfied by one of
    /// three lamps coming on — and a harness watching a single lamp would
    /// score a half-lit room as a pass.
    pub group: Vec<Entity>,
    /// The area to use when the utterance names a room.
    pub area: Option<String>,
    /// The entity that must be unaffected, for requirements that have one.
    pub bystander: Option<Entity>,
}

impl Target {
    /// One device, standing alone.
    fn single(device: Entity, area: Option<String>, bystander: Option<Entity>) -> Self {
        Self { group: vec![device.clone()], device, area, bystander }
    }

    /// Whether this target is a group rather than a single device.
    #[must_use]
    pub fn is_group(&self) -> bool {
        self.group.len() > 1
    }

    /// Names of everything the command is expected to move.
    #[must_use]
    pub fn group_names(&self) -> Vec<&str> {
        self.group.iter().map(|e| e.name.as_str()).collect()
    }
}

/// Domains that are switchable and therefore make a meaningful bystander —
/// something a room-wide command would wrongly reach.
const SWITCHABLE: &[&str] = &["light", "switch", "climate", "fan", "media_player", "cover"];

/// Words that make an entity *read* as belonging to a domain, whatever the
/// domain field says.
///
/// Needed because a bystander is chosen to be the thing that must **not**
/// move, and that assertion is only fair if no reasonable person would expect
/// it to. A real house turned up a `switch` named "Basement lights": the
/// domain differs from `light`, so it looked like a valid bystander, but
/// "turn on the lights in the Basement" arguably *should* move it. Asserting
/// otherwise would score the model wrong for agreeing with the user.
///
/// Includes the other suite languages, since a house may be named in any of
/// them.
const DOMAIN_WORDS: &[(&str, &[&str])] = &[
    ("light", &["light", "lights", "lamp", "lamps", "lumina", "lumini", "lumière", "luz", "spot"]),
    (
        "climate",
        &["ac", "aircon", "air conditioner", "climate", "thermostat", "clima", "termostat"],
    ),
    ("fan", &["fan", "ventilator", "ventilateur"]),
    ("media_player", &["speaker", "speakers", "tv", "media", "boxa", "boxă", "difuzor"]),
    ("cover", &["blind", "blinds", "shutter", "shutters", "curtain", "jaluzea", "rulou"]),
];

/// Whether `name` reads as belonging to `domain`.
///
/// Matched on whole words, not substrings: `ac` is a real marker for an air
/// conditioner and a disastrous substring, matching "Rack" and "Terrace"
/// alike. Multi-word markers are matched against the joined name.
fn name_suggests_domain(name: &str, domain: &str) -> bool {
    let lower = name.to_lowercase();
    let words: Vec<&str> =
        lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let joined = words.join(" ");
    DOMAIN_WORDS.iter().find(|(d, _)| *d == domain).is_some_and(|(_, markers)| {
        markers.iter().any(|m| if m.contains(' ') { joined.contains(m) } else { words.contains(m) })
    })
}

impl Requirement {
    /// Resolve against a house, or explain why it cannot be.
    ///
    /// Deterministic: every candidate list is ordered by entity name (the
    /// house is sorted at parse time) and the first is taken.
    pub fn resolve(&self, house: &House) -> std::result::Result<Target, Unsatisfied> {
        match self {
            Self::Device { domain } => {
                let device = house
                    .entities
                    .iter()
                    .find(|e| &e.domain == domain && house.targetable(e))
                    .ok_or_else(|| Unsatisfied(format!("no {domain} in this home")))?;
                Ok(Target::single(device.clone(), device.areas.first().cloned(), None))
            }
            Self::DimmableDevice { domain } => {
                let device = house
                    .entities
                    .iter()
                    .find(|e| &e.domain == domain && e.is_dimmable() && house.targetable(e))
                    .ok_or_else(|| Unsatisfied(format!("no dimmable {domain} in this home")))?;
                Ok(Target::single(device.clone(), device.areas.first().cloned(), None))
            }
            Self::AreaWithBystander { domain } => {
                for area in house.areas() {
                    let here: Vec<&Entity> =
                        house.in_area(&area).filter(|e| house.targetable(e)).collect();
                    let Some(device) = here.iter().find(|e| &e.domain == domain) else {
                        continue;
                    };
                    // The bystander must be unambiguous: a different domain
                    // *and* a name that does not read as the requested one,
                    // or the "must not move" assertion is unfair. Any of its
                    // names reading that way disqualifies it — the model sees
                    // the aliases too.
                    let bystander = here.iter().find(|e| {
                        &e.domain != domain
                            && SWITCHABLE.contains(&e.domain.as_str())
                            && !e.every_name().any(|n| name_suggests_domain(n, domain))
                    });
                    if let Some(b) = bystander {
                        return Ok(Target::single(
                            (*device).clone(),
                            Some(area.clone()),
                            Some((*b).clone()),
                        ));
                    }
                }
                Err(Unsatisfied(format!(
                    "no room in this home has a {domain} alongside another switchable device"
                )))
            }
            Self::NamedDevice { name } => {
                let device = house
                    .find_by_name(name)
                    // A named device that exists but is excluded is a mistake
                    // in the fixture, not a house without the device, and
                    // saying so plainly saves a confusing skip.
                    .filter(|e| house.targetable(e))
                    .ok_or_else(|| {
                        Unsatisfied(format!(
                            "this home exposes no device matching `{name}` (try --show-house)"
                        ))
                    })?;
                Ok(Target::single(device.clone(), device.areas.first().cloned(), None))
            }
            Self::NamedArea { area, domain } => {
                // The area name is matched loosely too: a house may record
                // `bucătărie` where the fixture says `Kitchen`.
                let real = house
                    .areas()
                    .into_iter()
                    .find(|a| loose_eq(a, area))
                    .ok_or_else(|| Unsatisfied(format!("this home has no area `{area}`")))?;
                let group: Vec<Entity> = house
                    .in_area(&real)
                    .filter(|e| &e.domain == domain && house.targetable(e))
                    .cloned()
                    .collect();
                let device = group.first().cloned().ok_or_else(|| {
                    Unsatisfied(format!("`{real}` exposes no {domain} to the assistant"))
                })?;
                Ok(Target { device, group, area: Some(real), bystander: None })
            }
        }
    }
}

/// Why a requirement could not be met. Carried into the report as a skip so
/// the reason is visible rather than inferred from a missing row.
#[derive(Debug, Clone)]
pub struct Unsatisfied(pub String);

impl std::fmt::Display for Unsatisfied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "\
- names: Ceiling lamp
  domain: light
  state: 'off'
  brightness: 128
  areas: Office
- names: Office AC
  domain: climate
  state: 'off'
  areas: Office
- names: Front door
  domain: lock
  state: locked
  areas: Hallway
- names: Hall lamp
  domain: light
  state: 'on'
  areas: Hallway
- names: Garage door
  domain: cover
  state: closed
  areas: Hallway
";

    #[test]
    fn parses_entities_with_areas_and_state() {
        let h = House::parse(DUMP);
        assert_eq!(h.entities.len(), 5);
        let lamp = h.get("Ceiling lamp").unwrap();
        assert_eq!(lamp.domain, "light");
        assert_eq!(lamp.areas, vec!["Office"]);
        assert_eq!(lamp.state.as_deref(), Some("off"));
        assert!(lamp.is_dimmable());
        assert!(!h.get("Hall lamp").unwrap().is_dimmable());
    }

    /// A benchmark runs unattended, so it never picks the front door or the
    /// garage — regardless of the fact that the model is still offered both.
    #[test]
    fn locks_and_garages_are_never_targeted() {
        let h = House::parse(DUMP);
        assert!(!h.get("Front door").unwrap().safe_to_target());
        assert!(!h.get("Garage door").unwrap().safe_to_target());
        assert!(h.get("Ceiling lamp").unwrap().safe_to_target());
    }

    /// A motion sensor reports `on` and `off` exactly as a lamp does, and the
    /// restore pass reaches every entity rather than only the ones a fixture
    /// named. Without this it tried to switch a sensor back and the house
    /// refused, ending a clean run by warning the home might be left changed.
    #[test]
    fn a_reading_is_never_targeted() {
        let h = House::parse(
            "- names: Office PIR\n  domain: binary_sensor\n  state: 'on'\n- names: Hall temp\n  \
             domain: sensor\n  state: '20'\n",
        );
        assert!(h.get("Office PIR").unwrap().is_reading());
        assert!(!h.get("Office PIR").unwrap().safe_to_target());
        assert!(!h.get("Hall temp").unwrap().safe_to_target());
        assert!(!h.get("Ceiling lamp").is_some_and(Entity::is_reading));
    }

    /// A house where two entities answer to one word. The server refuses such
    /// a name outright rather than choosing, so the fixture must pick the
    /// other lamp — aiming at the twin measured the house, and took the run
    /// down at the staging step before the model was even asked.
    #[test]
    fn a_name_two_devices_answer_to_is_never_targeted() {
        let h = House::parse(
            "- names: Couch\n  domain: light\n  state: 'off'\n  brightness: 40\n  areas: \
             Living\n- names: Couch\n  domain: media_player\n  state: 'off'\n  areas: Living\n- \
             names: Reading lamp\n  domain: light\n  state: 'off'\n  brightness: 90\n  areas: \
             Living\n",
        );
        let couch = h.get("Couch").unwrap();
        assert!(couch.safe_to_target(), "a lamp is safe in itself");
        assert!(!h.addressable(couch), "but the server cannot be asked for it");
        assert!(!h.targetable(couch));

        // Every requirement routes around it, including the one that asks by
        // name and the one that stages a whole room.
        for req in [
            Requirement::Device { domain: "light".into() },
            Requirement::DimmableDevice { domain: "light".into() },
            Requirement::NamedArea { area: "Living".into(), domain: "light".into() },
        ] {
            let t = req.resolve(&h).unwrap();
            assert_eq!(t.device.name, "Reading lamp", "{req:?} picked an uncommandable device");
            assert!(!t.group_names().contains(&"Couch"), "{req:?} staged an uncommandable device");
        }
        let err = Requirement::NamedDevice { name: "Couch".into() }.resolve(&h).unwrap_err();
        assert!(err.0.contains("Couch"));
    }

    /// A device the house listed but then refused to act on is dropped from
    /// consideration, aliases and all, so the same fixture re-resolves onto a
    /// device that works rather than being lost.
    #[test]
    fn a_refused_device_is_taken_out_of_consideration() {
        let h = House::parse(
            "- names: Couch, Canapea\n  domain: light\n  state: 'off'\n  brightness: 40\n  areas: \
             Living\n- names: Reading lamp\n  domain: light\n  state: 'off'\n  brightness: 90\n  \
             areas: Living\n",
        );
        let req = Requirement::Device { domain: "light".into() };
        assert_eq!(req.resolve(&h).unwrap().device.name, "Couch");

        let refused = BTreeSet::from(["couch".to_string()]);
        let usable = h.without(&refused);
        assert_eq!(usable.entities.len(), 1, "the alias row is the same device, also out");
        assert_eq!(req.resolve(&usable).unwrap().device.name, "Reading lamp");
        assert_eq!(h.without(&BTreeSet::new()).entities.len(), 2);
    }

    /// The bystander is found without anyone naming it: the office has a lamp
    /// and an air conditioner, which is exactly the domain-less room command.
    #[test]
    fn finds_a_room_with_a_bystander() {
        let h = House::parse(DUMP);
        let t = Requirement::AreaWithBystander { domain: "light".into() }.resolve(&h).unwrap();
        assert_eq!(t.area.as_deref(), Some("Office"));
        assert_eq!(t.device.name, "Ceiling lamp");
        assert_eq!(t.bystander.unwrap().name, "Office AC");
    }

    /// The hallway has a lamp and a cover, but the cover is a garage door and
    /// is excluded, so the hallway must not be chosen as the bystander room.
    #[test]
    fn an_excluded_device_cannot_serve_as_the_bystander() {
        let dump = "\
- names: Hall lamp
  domain: light
  areas: Hallway
- names: Garage door
  domain: cover
  areas: Hallway
";
        let h = House::parse(dump);
        assert!(Requirement::AreaWithBystander { domain: "light".into() }.resolve(&h).is_err());
    }

    /// Two runs must choose the same device or nothing is comparable.
    #[test]
    fn selection_is_deterministic() {
        let h = House::parse(DUMP);
        let req = Requirement::Device { domain: "light".into() };
        let a = req.resolve(&h).unwrap();
        let b = House::parse(DUMP);
        let b = req.resolve(&b).unwrap();
        assert_eq!(a.device.name, b.device.name);
    }

    /// Straight from a real house, and the reason `name_suggests_domain`
    /// exists. "Basement lights" is a `switch`, so by domain alone it looked
    /// like a fine bystander for "turn on the lights in the Basement" — but
    /// asserting it must **not** move would fail a model for doing what the
    /// user plainly meant. The room must be rejected instead.
    #[test]
    fn a_switch_named_lights_is_not_a_bystander_for_a_light_command() {
        let dump = "\
- names: Basement lights
  domain: switch
  areas: Basement
- names: Dark room lights
  domain: light
  areas: Basement
";
        let h = House::parse(dump);
        assert!(Requirement::AreaWithBystander { domain: "light".into() }.resolve(&h).is_err());
    }

    /// The exclusion must not be so eager that ordinary rooms stop resolving:
    /// an air conditioner is a perfectly fair bystander for a light command.
    #[test]
    fn an_unrelated_device_is_still_a_valid_bystander() {
        let h = House::parse(DUMP);
        let t = Requirement::AreaWithBystander { domain: "light".into() }.resolve(&h).unwrap();
        assert_eq!(t.bystander.unwrap().name, "Office AC");
    }

    /// `ac` has to be matched as a word. As a substring it hits "Terrace"
    /// and "Rack", which would quietly delete valid bystanders.
    #[test]
    fn domain_words_match_whole_words_only() {
        assert!(name_suggests_domain("Office AC", "climate"));
        assert!(name_suggests_domain("Air conditioner", "climate"));
        assert!(!name_suggests_domain("Terrace socket", "climate"));
        assert!(!name_suggests_domain("Rack fan power", "climate"));
        assert!(name_suggests_domain("Basement lights", "light"));
        assert!(!name_suggests_domain("Delight sensor", "light"));
    }

    /// A house without the thing a fixture needs says so, rather than failing.
    #[test]
    fn an_unsatisfiable_requirement_explains_itself() {
        let h = House::parse("- names: Hall lamp\n  domain: light\n  areas: Hallway\n");
        let err = Requirement::Device { domain: "media_player".into() }.resolve(&h).unwrap_err();
        assert!(err.to_string().contains("media_player"));
    }

    /// A block with no domain line is dropped rather than guessed at.
    #[test]
    fn a_block_without_a_domain_is_dropped() {
        let h = House::parse("- names: Mystery\n  areas: Yard\n");
        assert!(h.entities.is_empty());
    }

    /// Aliases share a line and must be split, or a Romanian room name in an
    /// English house is never found.
    #[test]
    fn area_aliases_are_split() {
        let h = House::parse("- names: Lamp\n  domain: light\n  areas: Kitchen, bucătărie\n");
        assert_eq!(h.areas(), vec!["Kitchen", "bucătărie"]);
    }

    /// The same convention on `names:`, which was read as one long name.
    ///
    /// A real speaker in the user's house is recorded as `Office display, Boxa
    /// birou` — one device carrying an English alias and a Romanian one. Taking
    /// the line verbatim made `name` a string Home Assistant refuses outright,
    /// so a fixture naming the Romanian alias staged and scored fine but could
    /// not be put back afterwards.
    #[test]
    fn device_aliases_are_split_and_only_the_first_is_sent() {
        let h = House::parse(
            "- names: Office display, Boxa birou\n  domain: media_player\n  state: 'off'\n",
        );
        let e = h.entities.first().expect("one entity, not two");
        assert_eq!(h.entities.len(), 1, "an alias list is one device");
        assert_eq!(e.name, "Office display", "the sendable name leads");
        assert_eq!(e.aliases, vec!["Boxa birou"]);
    }

    /// Either name finds the device, and what comes back can be commanded.
    #[test]
    fn a_device_is_found_by_any_of_its_names() {
        let h = House::parse(
            "- names: Office display, Boxa birou\n  domain: media_player\n  state: 'off'\n",
        );
        for asked in ["Office display", "Boxa birou", "boxa  birou", "birou boxa"] {
            let e = h.find_by_name(asked).unwrap_or_else(|| panic!("`{asked}` should resolve"));
            assert_eq!(e.name, "Office display", "`{asked}` must come back sendable");
        }
        // And the exact lookup the restore pass uses agrees, or a case that
        // named the alias is re-found as nothing and silently left alone.
        assert_eq!(h.get("Boxa birou").map(|e| e.name.as_str()), Some("Office display"));
    }

    /// A safety check reads every name, not just the first: a door recorded
    /// with a second-language alias is still a door.
    #[test]
    fn a_garage_named_in_an_alias_is_still_never_targeted() {
        let h = House::parse("- names: Big door, Poarta garaj\n  domain: cover\n  state: closed\n");
        assert!(!h.entities[0].safe_to_target());
    }

    /// The trap that would have left a house subtly wrong after every run.
    ///
    /// The dump reports brightness raw, 0–255, while `HassLightSet` takes a
    /// percentage — so reading `128` and writing it back unconverted would
    /// leave a half-lit lamp at full, and `255` would be refused outright.
    /// Confirmed against a real lamp and the real tool schema, not assumed.
    #[test]
    fn brightness_is_read_as_a_percentage_not_a_raw_byte() {
        let h = House::parse("- names: Lamp\n  domain: light\n  state: 'on'\n  brightness: 128\n");
        assert_eq!(h.get("Lamp").unwrap().level(), Some(Level::BrightnessPct(50)));

        let full = House::parse("- names: L\n  domain: light\n  state: 'on'\n  brightness: 255\n");
        assert_eq!(full.get("L").unwrap().level(), Some(Level::BrightnessPct(100)));
    }

    /// Volume runs the other way: the dump reports a 0.0–1.0 fraction and the
    /// tool wants 0–100, so the same care is needed in the opposite direction.
    #[test]
    fn volume_is_read_as_a_percentage_not_a_fraction() {
        let h = House::parse(
            "- names: Speaker\n  domain: media_player\n  state: playing\n  volume_level: 0.35\n",
        );
        assert_eq!(h.get("Speaker").unwrap().level(), Some(Level::VolumePct(35)));
    }

    /// A cover already reports a percentage, and a thermostat reports degrees
    /// in its own units. Neither must be rescaled.
    #[test]
    fn position_and_temperature_pass_through_unscaled() {
        let h = House::parse(
            "- names: Blind\n  domain: cover\n  state: open\n  current_position: 40\n- names: AC\n  \
             domain: climate\n  state: 'on'\n  temperature: 21.5\n",
        );
        assert_eq!(h.get("Blind").unwrap().level(), Some(Level::PositionPct(40)));
        assert_eq!(h.get("AC").unwrap().level(), Some(Level::TargetTemperature(21.5)));
    }

    /// A lamp reports its brightness rounded, so a level written as 50% reads
    /// back as 127 rather than 128. Without a tolerance every run would end by
    /// nudging every device it looked at, forever, and never settle.
    #[test]
    fn a_rounding_difference_is_not_worth_restoring() {
        assert!(!Level::BrightnessPct(50).differs_from(Level::BrightnessPct(51)));
        assert!(Level::BrightnessPct(50).differs_from(Level::BrightnessPct(90)));
        assert!(!Level::TargetTemperature(21.5).differs_from(Level::TargetTemperature(21.52)));
        assert!(Level::TargetTemperature(21.5).differs_from(Level::TargetTemperature(23.0)));
    }

    /// A device that reports no level at all must not be mistaken for one at
    /// zero — restoring "brightness 0" would switch a lamp off.
    #[test]
    fn a_missing_level_is_absent_not_zero() {
        let h = House::parse("- names: Plain\n  domain: light\n  state: 'on'\n");
        assert_eq!(h.get("Plain").unwrap().level(), None);
    }
}
