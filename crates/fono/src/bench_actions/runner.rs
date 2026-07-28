// SPDX-License-Identifier: GPL-3.0-only
//! Run the suite: set the stage, say the thing, judge what happened, put it
//! back.
//!
//! The order of those four is the whole design. A benchmark against a real
//! home is worth more than one against a simulator — it has the real device
//! names, the real catalogue size, the real latencies, the real duplicate
//! names two rooms apart — but only if a run leaves no trace and a rerun
//! measures the same thing. Everything awkward in this file is paying for
//! that.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use fono_assistant::mcp_client::{self, McpEndpoint};

use super::fixture::{args_match, Case, CaseReport, Class, Manifest, Verdict};
use super::house::{Entity, House, Level, Target};
use super::turn::{TurnDriver, TurnObservation};

/// How long a setup or restore call may take.
///
/// Shorter than a model turn on purpose: these are direct device calls with
/// no thinking in them, and one that hangs should be seen as a hang.
const STAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for a device to actually reach the state it was told to.
///
/// A lamp reports back before the bulb has answered. Reading the world too
/// soon scores the delay rather than the model.
const SETTLE: Duration = Duration::from_millis(1200);

/// What a run needs to know that the fixtures do not say.
pub struct RunOptions {
    /// Which languages to say each utterance in.
    pub languages: Vec<String>,
    /// Run each case this many times. More than one is how a routing failure
    /// is told from a coin toss — nothing about a model at temperature zero
    /// guarantees the same tool call twice.
    pub repeats: u32,
    /// Resolve targets, print what would be said and to which device, and
    /// touch nothing. The last check before letting a script loose on a home.
    pub dry_run: bool,
    /// Skip cases that would make noise.
    pub quiet_hours: bool,
    /// Only run cases whose id contains this.
    pub only: Option<String>,
}

/// Everything one run produced.
pub struct RunOutcome {
    /// The shareable layer: verdicts and timings, keyed by case id. Contains
    /// no device name, no area, and nothing anybody said — so it can be
    /// committed, diffed and compared on a machine that has never seen the
    /// house, which is what makes regression tracking possible at all.
    pub safe: Vec<CaseReport>,
    /// The local layer: which device was chosen, the literal arguments, the
    /// reply text. This is what a failure is actually debugged from, and it
    /// stays in the run directory.
    pub detail: Vec<CaseDetail>,
}

/// The private half of one case's result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseDetail {
    pub id: String,
    pub language: String,
    pub said: String,
    pub device: String,
    /// Everything the command was expected to move. More than one for a room
    /// command, and worth recording separately: "two of three lamps came on"
    /// is a different diagnosis from "nothing happened".
    pub group: Vec<String>,
    pub area: Option<String>,
    pub bystander: Option<String>,
    pub reply: String,
    pub calls: Vec<DetailCall>,
    /// Why the verdict came out as it did, in words, so a failure does not
    /// have to be reverse-engineered from booleans.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DetailCall {
    pub name: String,
    pub arguments: String,
    pub outcome: Option<String>,
}

/// Run every case in a manifest.
pub async fn run(
    manifest: &Manifest,
    driver: &TurnDriver,
    ep: &McpEndpoint,
    opts: &RunOptions,
) -> Result<RunOutcome> {
    let house = House::read(ep).await.context("read the home's device list")?;
    if house.entities.is_empty() {
        anyhow::bail!(
            "the server described no devices — the suite would skip everything and report a \
             perfect score against an empty house"
        );
    }

    // The state of the home before this suite touched anything.
    //
    // Per-case restore is not enough on its own, and the gap is not
    // theoretical: a case staged `on` and restored to `on` leaves the lights
    // burning, because per-case restore faithfully puts back the state the
    // *staging* created rather than the state the home was in. That is exactly
    // how a run ended with a bedroom lit for hours.
    //
    // Photographed once, before the first `precondition` runs, and applied
    // again at the end. Belt and braces with the per-case restore, which is
    // still worth having: it keeps case two from starting inside case one's
    // mess, and only this final pass can undo the staging itself.
    let baseline = house.clone();

    let mut out = RunOutcome { safe: Vec::new(), detail: Vec::new() };
    let result = run_cases(manifest, driver, ep, opts, &house, &mut out).await;

    // Restore even when a case failed or the suite aborted: a half-finished
    // run is precisely when the home is most likely to be left switched on,
    // and the error is reported afterwards either way.
    if !opts.dry_run {
        match restore_baseline(ep, &baseline).await {
            Ok(0) => println!("\n  home left as it was found."),
            Ok(n) => println!("\n  put {n} device(s) back the way they were."),
            Err(e) => println!(
                "\n  WARNING: could not fully put the home back: {e}\n  Check the devices this \
                 suite touched before trusting the next run."
            ),
        }
    }
    result?;
    Ok(out)
}

/// The case loop, with the baseline restore lifted out so it always runs.
async fn run_cases(
    manifest: &Manifest,
    driver: &TurnDriver,
    ep: &McpEndpoint,
    opts: &RunOptions,
    house: &House,
    out: &mut RunOutcome,
) -> Result<()> {
    for case in &manifest.cases {
        if let Some(f) = &opts.only {
            if !case.id.contains(f.as_str()) {
                continue;
            }
        }
        // Resolve once per case, not once per language: the same device must
        // be used in every language or the cells are not comparable.
        let target = match case.requires.resolve(house) {
            Ok(t) => t,
            Err(why) => {
                // A dry run that silently omits a case is worse than useless:
                // it is read as "these are the cases that will run", and a
                // missing row looks like a fixture that was never written.
                if opts.dry_run {
                    println!("  {:<34} skipped — {why}", case.id);
                }
                for lang in &opts.languages {
                    out.safe.push(skipped(case, lang, &why.to_string()));
                }
                continue;
            }
        };
        for lang in &opts.languages {
            let Some(template) = case.utterances.get(lang) else {
                out.safe.push(skipped(case, lang, "no utterance written in this language"));
                continue;
            };
            let said =
                super::fixture::render(template, &target.device.name, target.area.as_deref());
            if opts.dry_run {
                let targets = if target.is_group() {
                    format!("{} devices: {}", target.group.len(), target.group_names().join(", "))
                } else {
                    format!("device: {}", target.device.name)
                };
                println!(
                    "  {:<34} {lang}  \"{said}\"\n      {targets}{}",
                    case.id,
                    target
                        .bystander
                        .as_ref()
                        .map(|b| format!("   must not move: {}", b.name))
                        .unwrap_or_default()
                );
                continue;
            }
            for _ in 0..opts.repeats.max(1) {
                let (report, detail) =
                    run_one(case, &target, &said, lang, driver, ep, opts).await?;
                out.safe.push(report);
                out.detail.push(detail);
            }
        }
    }
    Ok(())
}

/// One case, in one language, once.
async fn run_one(
    case: &Case,
    target: &Target,
    said: &str,
    lang: &str,
    driver: &TurnDriver,
    ep: &McpEndpoint,
    opts: &RunOptions,
) -> Result<(CaseReport, CaseDetail)> {
    let mut notes = Vec::new();

    // Stage. Without a known starting state, "turn off the lamp" against an
    // already-dark lamp is indistinguishable from a command that did nothing.
    if let Some(want) = &case.precondition {
        // Every member, not just the first: a room command staged on one of
        // three lamps starts from a half-lit room, and "turn them on" would
        // then be scored against a state it was already partly in.
        for e in &target.group {
            set_state(ep, e, want)
                .await
                .with_context(|| format!("could not stage {} before `{}`", e.name, case.id))?;
        }
        tokio::time::sleep(SETTLE).await;
    }

    // Photograph the world. Anything that moves between here and the second
    // photograph, other than what the case is about, is drift — a schedule, a
    // motion sensor, somebody else in the house — and must not be scored as
    // the model's fault.
    let before = House::read(ep).await.context("read the home before the command")?;

    let obs = driver.run(said).await?;

    tokio::time::sleep(SETTLE).await;
    let after = House::read(ep).await.context("read the home after the command")?;

    // Judge, then restore — the restore must not be able to change the score.
    let (verdict, report_bits) = judge(case, target, &obs, &before, &after, lang, &mut notes);

    if case.precondition.is_some() || case.expect_device.is_some() || case.expect_level.is_some() {
        if let Err(e) = restore(ep, target, &before).await {
            // Loud, and fatal: a half-restored home makes every later case
            // meaningless, and quietly carrying on would produce a full page
            // of numbers that mean nothing.
            anyhow::bail!(
                "could not put {} back the way it was after `{}`: {e}. Stopping — the rest of \
                 the suite would be measuring a home this run has already changed.",
                target.device.name,
                case.id
            );
        }
    }
    let _ = opts;

    let detail = CaseDetail {
        id: case.id.clone(),
        language: lang.to_string(),
        said: said.to_string(),
        device: target.device.name.clone(),
        group: target.group_names().into_iter().map(str::to_string).collect(),
        area: target.area.clone(),
        bystander: target.bystander.as_ref().map(|b| b.name.clone()),
        reply: obs.reply.clone(),
        calls: obs
            .calls
            .iter()
            .map(|c| DetailCall {
                name: c.name.clone(),
                arguments: c.arguments.clone(),
                outcome: c.outcome.clone(),
            })
            .collect(),
        notes,
    };
    let report = CaseReport {
        id: case.id.clone(),
        class: case.class,
        language: lang.to_string(),
        verdict,
        calls: obs.calls.len(),
        elapsed_ms: u64::try_from(obs.elapsed.as_millis()).unwrap_or(u64::MAX),
        skipped_because: None,
        ..report_bits
    };
    Ok((report, detail))
}

/// Did the model open correctly?
///
/// Scored on the **first** call only. The gap between this and the final
/// outcome is the measured worth of the retry ladder, and collapsing the two
/// into one number hides the thing the harness was built to see.
fn score_routing(case: &Case, obs: &TurnObservation, notes: &mut Vec<String>) -> bool {
    let Some(call) = obs.first_call() else { return case.expect_no_call };
    if case.expect_no_call {
        notes.push(format!("called `{}` when it should have asked or explained", call.name));
        return false;
    }

    let name_ok = case.expect_tool.as_ref().is_none_or(|w| &call.name == w);
    if !name_ok {
        notes.push(format!(
            "first call was `{}`, expected `{}`",
            call.name,
            case.expect_tool.as_deref().unwrap_or("?")
        ));
    }

    // Unparseable arguments are a finding in their own right, not a parse
    // error to swallow: a model that emits broken JSON has failed the case,
    // and `Null` matches nothing, which is the correct outcome.
    let parsed: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);

    let args_ok = case.expect_args.as_ref().is_none_or(|want| args_match(want, &parsed));
    if !args_ok {
        notes.push(format!("first call arguments did not match: {}", call.arguments));
    }

    let none_forbidden =
        case.forbid_args.iter().all(|k| parsed.get(k).is_none_or(serde_json::Value::is_null));
    if !none_forbidden {
        notes.push(format!("first call invented a value nobody asked for: {}", call.arguments));
    }

    name_ok && args_ok && none_forbidden
}

/// Did the world end up the way it was asked to?
///
/// The only question the user actually cares about, and it is answered by
/// looking at the house rather than by trusting either the model or the
/// server — Home Assistant returns a perfectly successful result for a
/// command that matched nothing and did nothing.
fn score_outcome(
    case: &Case,
    target: &Target,
    obs: &TurnObservation,
    after: &House,
    notes: &mut Vec<String>,
) -> bool {
    // For a case that must not act, the right outcome is that nothing
    // happened. Without this the model calls a tool, routing scores false,
    // the outcome scores true by default, and the case is reported as having
    // *recovered* — the exact opposite of what it did.
    if case.expect_no_call {
        return obs.calls.is_empty();
    }

    // A level, when the case names one. Checked before the on/off test and
    // combined with it, because the two are independent assertions and a case
    // may reasonably make either or both.
    let level_ok = score_level(case, target, after, notes);

    let Some(want) = &case.expect_device else { return level_ok };

    // Every member has to arrive. A room with three lamps where one came on
    // is the most common way for a room command to be half-right, and it is
    // exactly what a single-device check would call a pass.
    //
    // `unavailable` members are excused rather than failed: a lamp that is
    // unplugged cannot be commanded by anyone, and scoring the model for it
    // would be measuring the house.
    let mut wrong = Vec::new();
    let mut reachable = 0;
    for e in &target.group {
        let observed = after.get(&e.name).and_then(|x| x.state.as_deref());
        if observed.is_none_or(|s| s.eq_ignore_ascii_case("unavailable")) {
            continue;
        }
        reachable += 1;
        if !observed.is_some_and(|s| reached(&e.domain, want, s)) {
            wrong.push(format!("{} is `{}`", e.name, observed.unwrap_or("unknown")));
        }
    }
    if reachable == 0 {
        notes.push("every device this case targets is unavailable".to_string());
        return false;
    }
    if !wrong.is_empty() {
        notes.push(format!(
            "expected {} of {reachable} to end up `{want}`, but {}",
            if target.is_group() { "all" } else { "the device" },
            wrong.join(", ")
        ));
    }
    wrong.is_empty() && level_ok
}

/// Did the device end up at the level the case asked for?
///
/// The assertion a number-carrying command needs, and the one whose absence let
/// a refused `HassSetVolume` score as a pass: the tool was right, the argument
/// was right, and nothing checked that the speaker had heard.
///
/// Judged against the first group member that reports a level at all. A device
/// with no level to read is not a failure — the case simply has nothing to say
/// about it, exactly as an `unavailable` member is excused above.
fn score_level(case: &Case, target: &Target, after: &House, notes: &mut Vec<String>) -> bool {
    let Some(want) = case.expect_level else { return true };
    for e in &target.group {
        let Some(observed) = after.get(&e.name).and_then(Entity::level) else { continue };
        // Compared in the observed device's own unit, so the tolerance that
        // stops a rounded reading from looking like a change applies here too.
        let wanted = match observed {
            Level::BrightnessPct(_) => Level::BrightnessPct(want),
            Level::VolumePct(_) => Level::VolumePct(want),
            Level::PositionPct(_) => Level::PositionPct(want),
            // A temperature is not a percentage and must not be compared to one.
            Level::TargetTemperature(_) => continue,
        };
        if wanted.differs_from(observed) {
            notes.push(format!("expected {} to end up at {want}%, but it is {observed:?}", e.name));
            return false;
        }
        return true;
    }
    true
}

/// Is a device's reported state the one the fixture asked for?
///
/// Exact for almost everything, and deliberately not for `climate`. A
/// thermostat or an air conditioner does not report `on` — it reports the mode
/// it is running in (`cool`, `heat`, `dry`, `fan_only`, `auto`), and only `off`
/// is spelled the way a fixture would spell it. Comparing literally would score
/// a working air conditioner as a failure for coming on in `cool`, and the
/// fixture's alternative — pinning one mode — would be a fixture about one
/// house's idea of what "on" means.
///
/// Anything that is not `off` is therefore `on` for a climate device, which is
/// also how a person would read it.
fn reached(domain: &str, want: &str, observed: &str) -> bool {
    if domain == "climate" && want.eq_ignore_ascii_case("on") {
        return !observed.eq_ignore_ascii_case("off");
    }
    observed.eq_ignore_ascii_case(want)
}

/// Everything scored about a turn, before the bookkeeping fields are added.
fn judge(
    case: &Case,
    target: &Target,
    obs: &TurnObservation,
    before: &House,
    after: &House,
    lang: &str,
    notes: &mut Vec<String>,
) -> (Verdict, CaseReport) {
    let routed_first_try = score_routing(case, obs, notes);
    let outcome_correct = score_outcome(case, target, obs, after, notes);

    // Did anything else move? The room-command failure is invisible in the
    // tool call — asking for a whole room is a well-formed request that also
    // starts the air conditioning — so it can only be caught here.
    let bystander_held = case.expect_bystander_unchanged.then(|| {
        target.bystander.as_ref().is_none_or(|b| {
            let held = !changed(before, after, &b.name);
            if !held {
                notes
                    .push("something else in the room moved that nobody asked to move".to_string());
            }
            held
        })
    });

    // Did anything move that this case has nothing to do with? A real home
    // has other actors in it, and calling their work a failure is how a
    // real-house suite turns into noise nobody trusts.
    let ignore: Vec<&str> = target
        .group_names()
        .into_iter()
        .chain(target.bystander.as_ref().map(|b| b.name.as_str()))
        .collect();
    let drifted = drift(before, after, &ignore);
    if let Some(what) = &drifted {
        notes.push(format!("{what} changed on its own during this case"));
    }

    // Did the reply describe what actually happened?
    //
    // Judged against the world, not against the fixture. A reply that says it
    // switched one lamp when it switched one lamp is honest even though the
    // case wanted three — the case failed, but calling that a lie would be a
    // second, false accusation. Only a claim of success over a home where
    // nothing at all moved is dishonest, and that is the failure worth naming:
    // a confident report over a dark room.
    let reply_truthful = if obs.reply.trim().is_empty() {
        None
    } else {
        let nothing_moved = !target.group.iter().any(|e| changed(before, after, &e.name));
        let truthful = !claims_success(&obs.reply) || !nothing_moved;
        if !truthful {
            notes.push("the reply claimed success but nothing in the home moved".to_string());
        }
        Some(truthful)
    };

    // Was the reply in the language it was asked in? A model can pick the
    // right device, move it, and then describe the result in the wrong
    // language — invisible to every other check here, and the most obvious
    // defect in the turn to the person listening.
    //
    // Not judged on a reply that was cut off part way through: half a sentence
    // is not evidence of the wrong language, and scoring it as such would
    // charge the model for an interruption it did not cause.
    let reply_language_matched = if obs.aborted {
        notes.push("the reply was cut off, so its language was not judged".to_string());
        None
    } else {
        super::language::matches(&obs.reply, lang)
    };
    if reply_language_matched == Some(false) {
        notes.push(format!("replied in the wrong language (wanted {lang})"));
    }

    let non_idempotent_retry = !case.retry_allowed && obs.retried();
    if non_idempotent_retry {
        notes.push(
            "repeated a command that must never be repeated — asking twice for two degrees \
             warmer is four degrees"
                .to_string(),
        );
    }

    let all_good = outcome_correct
        && bystander_held.unwrap_or(true)
        && reply_truthful.unwrap_or(true)
        && reply_language_matched.unwrap_or(true)
        && !non_idempotent_retry;

    let verdict = if drifted.is_some() && !all_good {
        Verdict::Drifted
    } else if all_good && routed_first_try {
        Verdict::Passed
    } else if all_good {
        Verdict::Recovered
    } else {
        Verdict::Failed
    };

    (
        verdict,
        CaseReport {
            id: String::new(),
            class: Class::PlainCommand,
            language: String::new(),
            verdict,
            routed_first_try,
            outcome_correct,
            bystander_held,
            reply_truthful,
            reply_language_matched,
            calls: 0,
            elapsed_ms: 0,
            skipped_because: None,
        },
    )
}

/// Did one named entity's state change between the two photographs?
fn changed(before: &House, after: &House, name: &str) -> bool {
    let b = before.get(name).and_then(|e| e.state.as_deref());
    let a = after.get(name).and_then(|e| e.state.as_deref());
    b != a
}

/// The first entity that moved on its own, ignoring the ones this case is
/// about. Sorted order makes the answer stable across runs.
fn drift(before: &House, after: &House, ignore: &[&str]) -> Option<String> {
    before
        .entities
        .iter()
        .filter(|e| !ignore.contains(&e.name.as_str()))
        // A reading that moves is not an action: a thermometer is supposed to
        // change. A lock or a garage door moving, on the other hand, is very
        // much drift — so this is the narrow reading test, not the broader
        // "may this be targeted" one.
        .filter(|e| !e.is_reading())
        .find(|e| changed(before, after, &e.name))
        .map(|e| e.domain.clone())
}

/// Does the reply assert that something was done?
///
/// Deliberately crude and deliberately biased towards *not* accusing: it only
/// reports a lie when the wording is unambiguous, because a false accusation
/// of dishonesty is worse than a missed one, and the state assertion already
/// catches the substance.
fn claims_success(reply: &str) -> bool {
    const DONE: &[&str] = &[
        "done",
        "turned on",
        "turned off",
        "switched on",
        "switched off",
        "i've",
        "i have",
        "am aprins",
        "am stins",
        "gata",
        "allumé",
        "éteint",
        "c'est fait",
        "encendido",
        "apagado",
        "hecho",
        "listo",
    ];
    const HEDGE: &[&str] =
        &["couldn't", "could not", "unable", "failed", "n-am putut", "nu am putut", "no pude"];
    let lower = reply.to_lowercase();
    if HEDGE.iter().any(|h| lower.contains(h)) {
        return false;
    }
    DONE.iter().any(|d| lower.contains(d))
}

/// Drive one entity into a known state, bypassing the assistant entirely.
///
/// Bypassing is the point: staging must not depend on the thing being
/// measured, or a model that cannot route also cannot be tested.
async fn set_state(ep: &McpEndpoint, entity: &Entity, want: &str) -> Result<()> {
    let tool = match (entity.domain.as_str(), want) {
        (_, "on") => "HassTurnOn",
        (_, "off") => "HassTurnOff",
        _ => anyhow::bail!("do not know how to put a {} into `{want}`", entity.domain),
    };
    call(ep, tool, &serde_json::json!({ "name": entity.name })).await
}

/// Put one entity's level back where it was.
///
/// Separate from [`set_state`] because the order matters: a lamp has to be on
/// before its brightness means anything, so the caller switches first and
/// levels second.
async fn set_level(ep: &McpEndpoint, entity: &Entity, level: Level) -> Result<()> {
    let (tool, args) = match level {
        Level::BrightnessPct(pct) => {
            ("HassLightSet", serde_json::json!({ "name": entity.name, "brightness": pct }))
        }
        Level::VolumePct(pct) => {
            ("HassSetVolume", serde_json::json!({ "name": entity.name, "volume_level": pct }))
        }
        Level::TargetTemperature(t) => (
            "HassClimateSetTemperature",
            serde_json::json!({ "name": entity.name, "temperature": t }),
        ),
        Level::PositionPct(pct) => {
            ("HassSetPosition", serde_json::json!({ "name": entity.name, "position": pct }))
        }
    };
    call(ep, tool, &args).await
}

/// Make one device call and insist it worked.
async fn call(ep: &McpEndpoint, tool: &str, args: &serde_json::Value) -> Result<()> {
    let ep = McpEndpoint { timeout: STAGE_TIMEOUT, ..ep.clone() };
    let res =
        mcp_client::call_tool(&ep, tool, args).await.with_context(|| format!("{tool} {args}"))?;
    if res.is_error {
        anyhow::bail!("{tool} was refused: {}", res.text);
    }
    Ok(())
}

/// Put back everything this case moved.
///
/// Restoring from the photograph rather than from the fixture means a case
/// that failed in an unexpected way is still cleaned up.
///
/// Levels are put back as well as on/off, and after them: a speaker turned
/// down to a tenth is as much a changed house as a lamp left burning, and the
/// final baseline pass alone would leave every later case in this run starting
/// from the level this one set.
async fn restore(ep: &McpEndpoint, target: &Target, before: &House) -> Result<()> {
    let names: Vec<&str> = target
        .group_names()
        .into_iter()
        .chain(target.bystander.as_ref().map(|b| b.name.as_str()))
        .collect();
    // One read for the whole restore rather than one per device: this runs
    // after every case, and the levels are only compared to avoid writing a
    // value that is already there.
    let now = House::read(ep).await.ok();
    for name in names {
        let Some(entity) = before.get(name) else { continue };
        if let Some(was) = entity.state.as_deref().filter(|s| matches!(*s, "on" | "off")) {
            set_state(ep, entity, was).await?;
        }
        // After the switch, so a lamp is on before it is dimmed, and skipped
        // for a device that was off — where setting a level would switch it
        // back on.
        if entity.state.as_deref() != Some("off") {
            if let Some(was) = entity.level() {
                let is = now.as_ref().and_then(|h| h.get(name)).and_then(Entity::level);
                if is.is_none_or(|is| was.differs_from(is)) {
                    set_level(ep, entity, was).await?;
                }
            }
        }
    }
    Ok(())
}

/// Put the whole home back the way the suite found it.
///
/// The last thing a run does, and the thing that makes running it against a
/// real home defensible. Reads the home again rather than trusting the
/// per-case restores, so anything a failed case left behind — or a restore
/// that silently did not take — is still caught.
///
/// Drift is deliberately included: a device someone else moved mid-run is put
/// back too. That is the wrong call for a home automation tool and the right
/// one for a benchmark, because the alternative is a suite that has to
/// distinguish its own mess from everyone else's and gets it wrong.
///
/// The one exception is anything [`Entity::safe_to_target`] refuses. Including
/// drift means this pass reaches devices no fixture ever named, so without
/// that check a benchmark could re-lock a door somebody deliberately opened
/// while it ran. Restoring a lamp nobody asked about is harmless; restoring a
/// lock is not, and "put it back exactly" is the wrong instinct for anything
/// with a person on the other side of it.
///
/// Errors are collected rather than returned on the first failure: one
/// unreachable lamp must not leave the other nine devices switched on.
async fn restore_baseline(ep: &McpEndpoint, baseline: &House) -> Result<usize> {
    let now = House::read(ep).await.context("read the home to put it back")?;
    let mut fixed = 0;
    let mut failures = Vec::new();

    for was in &baseline.entities {
        if !was.safe_to_target() {
            continue;
        }
        let Some(is) = now.get(&was.name) else { continue };
        // A device that has since gone unavailable cannot be commanded, and
        // one that *was* unavailable has no state worth restoring.
        if [was.state.as_deref(), is.state.as_deref()]
            .iter()
            .any(|s| s.is_none_or(|s| s.eq_ignore_ascii_case("unavailable")))
        {
            continue;
        }

        let mut touched = false;
        if was.state != is.state {
            // Only on/off is restorable; a cover reports `open`/`closed` and
            // is put back by position instead, below.
            if let Some(want) = was.state.as_deref().filter(|s| matches!(*s, "on" | "off")) {
                match set_state(ep, was, want).await {
                    Ok(()) => touched = true,
                    Err(e) => failures.push(format!("{}: {e}", was.name)),
                }
            }
        }

        // Levels after switching, so a lamp is on before it is dimmed. Skipped
        // for anything the home reports as off, where a level is meaningless
        // and setting one would switch the device back on.
        if was.state.as_deref() != Some("off") {
            if let (Some(w), Some(i)) = (was.level(), is.level()) {
                if w.differs_from(i) {
                    match set_level(ep, was, w).await {
                        Ok(()) => touched = true,
                        Err(e) => failures.push(format!("{}: {e}", was.name)),
                    }
                }
            }
        }
        if touched {
            fixed += 1;
        }
    }

    if !failures.is_empty() {
        anyhow::bail!("{} device(s) would not go back: {}", failures.len(), failures.join("; "));
    }
    Ok(fixed)
}

fn skipped(case: &Case, lang: &str, why: &str) -> CaseReport {
    CaseReport {
        id: case.id.clone(),
        class: case.class,
        language: lang.to_string(),
        verdict: Verdict::Skipped,
        routed_first_try: false,
        outcome_correct: false,
        bystander_held: None,
        reply_truthful: None,
        reply_language_matched: None,
        calls: 0,
        elapsed_ms: 0,
        skipped_because: Some(why.to_string()),
    }
}

/// Headline numbers, grouped so a weak model is distinguishable from a broken
/// rung: six failures across six classes is the former, six in one class the
/// latter.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Summary {
    pub n: usize,
    pub passed: usize,
    pub recovered: usize,
    pub failed: usize,
    pub drifted: usize,
    pub skipped: usize,
    /// Chose the right tool and arguments unaided, as a fraction of cases
    /// actually run.
    ///
    /// Deliberately **not** a success rate: a case can route perfectly and
    /// still fail because the house rejected the argument. Reported next to
    /// `final_rate` rather than above it, because reading them as a pair —
    /// "routed well, still failed" — is what points at the server rather than
    /// the model.
    pub routing_rate: f32,
    /// Right in the end, however many attempts it took. This is the rate the
    /// user would recognise, and the only one that counts a case as good.
    pub final_rate: f32,
    /// Right in the end **and** unaided, so the ladder never had to fire.
    ///
    /// `final_rate` minus this is what Fono's recovery machinery is worth.
    /// Both are fractions of the same denominator, so they nest: this can
    /// never exceed `final_rate`.
    pub first_try_rate: f32,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

/// Summarise, split by whatever key the caller groups on.
#[must_use]
pub fn summarise(reports: &[CaseReport]) -> Summary {
    let run: Vec<&CaseReport> = reports.iter().filter(|r| r.verdict != Verdict::Skipped).collect();
    let n = run.len();
    let mut s = Summary {
        n,
        skipped: reports.len() - n,
        passed: run.iter().filter(|r| r.verdict == Verdict::Passed).count(),
        recovered: run.iter().filter(|r| r.verdict == Verdict::Recovered).count(),
        failed: run.iter().filter(|r| r.verdict == Verdict::Failed).count(),
        drifted: run.iter().filter(|r| r.verdict == Verdict::Drifted).count(),
        ..Summary::default()
    };
    if n > 0 {
        s.routing_rate = run.iter().filter(|r| r.routed_first_try).count() as f32 / n as f32;
        s.final_rate = (s.passed + s.recovered) as f32 / n as f32;
        // Passed already means "good outcome, first try" — see the verdict
        // ladder in `score`. Counting `routed_first_try` here instead would
        // let the first-try rate exceed the final rate, which is incoherent.
        s.first_try_rate = s.passed as f32 / n as f32;
        let mut ms: Vec<u64> = run.iter().map(|r| r.elapsed_ms).collect();
        ms.sort_unstable();
        s.p50_ms = pct(&ms, 50);
        s.p95_ms = pct(&ms, 95);
    }
    s
}

/// Group summaries by language, then by class. Both cuts matter: the first
/// shows whether a house named in English understands Romanian, the second
/// shows which rung is broken.
#[must_use]
pub fn group_by<'a>(
    reports: &'a [CaseReport],
    key: impl Fn(&'a CaseReport) -> String,
) -> BTreeMap<String, Summary> {
    let mut buckets: BTreeMap<String, Vec<CaseReport>> = BTreeMap::new();
    for r in reports {
        buckets.entry(key(r)).or_default().push(r.clone());
    }
    buckets.into_iter().map(|(k, v)| (k, summarise(&v))).collect()
}

fn pct(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = (sorted.len() - 1) * p / 100;
    sorted[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(verdict: Verdict, first: bool, ms: u64) -> CaseReport {
        CaseReport {
            id: "x".into(),
            class: Class::PlainCommand,
            language: "en".into(),
            verdict,
            routed_first_try: first,
            outcome_correct: verdict != Verdict::Failed,
            bystander_held: None,
            reply_truthful: None,
            reply_language_matched: None,
            calls: 1,
            elapsed_ms: ms,
            skipped_because: None,
        }
    }

    /// The two rates must differ when the ladder did work — that difference
    /// is the number the whole harness exists to produce.
    #[test]
    fn recovery_shows_up_as_a_gap_between_the_two_rates() {
        let s = summarise(&[
            rep(Verdict::Passed, true, 10),
            rep(Verdict::Recovered, false, 20),
            rep(Verdict::Failed, false, 30),
        ]);
        assert!((s.first_try_rate - 1.0 / 3.0).abs() < 1e-6);
        assert!((s.final_rate - 2.0 / 3.0).abs() < 1e-6);
    }

    /// The first live run printed "right first time 100% / right in the end
    /// 0%" — a case that chose the right tool and still failed at the server.
    /// Routing is its own number; the two success rates must nest.
    #[test]
    fn a_well_routed_failure_does_not_beat_the_final_rate() {
        let s = summarise(&[rep(Verdict::Failed, true, 10)]);
        assert!((s.routing_rate - 1.0).abs() < 1e-6, "routing stands on its own");
        assert!((s.first_try_rate - 0.0).abs() < 1e-6);
        assert!((s.final_rate - 0.0).abs() < 1e-6);
        assert!(s.first_try_rate <= s.final_rate);
    }

    /// A house that lacks the device must not drag the score down.
    #[test]
    fn skips_are_excluded_from_every_rate() {
        let s = summarise(&[rep(Verdict::Passed, true, 10), rep(Verdict::Skipped, false, 0)]);
        assert_eq!(s.n, 1);
        assert_eq!(s.skipped, 1);
        assert!((s.final_rate - 1.0).abs() < 1e-6);
    }

    #[test]
    fn an_empty_run_does_not_divide_by_zero() {
        let s = summarise(&[]);
        assert_eq!(s.n, 0);
        assert!((s.final_rate - 0.0).abs() < 1e-6);
    }

    #[test]
    fn a_plain_confirmation_reads_as_a_claim_of_success() {
        assert!(claims_success("Done — the lamp is on."));
        assert!(claims_success("Am aprins lumina."));
    }

    /// An honest failure must never be scored as a lie.
    #[test]
    fn an_admission_of_failure_is_not_a_claim() {
        assert!(!claims_success("I couldn't turn that on."));
        assert!(!claims_success("N-am putut să aprind lumina."));
    }

    /// A reading that moves on its own is not drift; a switch is.
    #[test]
    fn sensors_are_not_treated_as_drift() {
        let before = House::parse(
            "- names: Temp\n  domain: sensor\n  state: '20'\n- names: Fan\n  domain: switch\n  \
             state: 'off'\n",
        );
        let after = House::parse(
            "- names: Temp\n  domain: sensor\n  state: '21'\n- names: Fan\n  domain: switch\n  \
             state: 'off'\n",
        );
        assert!(drift(&before, &after, &[]).is_none());
    }

    #[test]
    fn an_unrelated_switch_moving_is_drift() {
        let before = House::parse("- names: Fan\n  domain: switch\n  state: 'off'\n");
        let after = House::parse("- names: Fan\n  domain: switch\n  state: 'on'\n");
        assert_eq!(drift(&before, &after, &[]).as_deref(), Some("switch"));
        // …unless it is the device the case was about.
        assert!(drift(&before, &after, &["Fan"]).is_none());
    }

    /// An air conditioner that came on reports `cool`, not `on`. Comparing
    /// literally would have failed every climate case for succeeding — the
    /// office AC fixtures are the reason this exists.
    #[test]
    fn a_climate_device_is_on_whenever_it_is_not_off() {
        for mode in ["cool", "heat", "dry", "fan_only", "auto"] {
            assert!(reached("climate", "on", mode), "`{mode}` should read as on");
        }
        assert!(!reached("climate", "on", "off"));
        assert!(reached("climate", "off", "off"));
    }

    /// The looseness is for `climate` alone: a lamp reporting `unavailable`
    /// must never be read as having come on.
    #[test]
    fn other_domains_are_still_compared_exactly() {
        assert!(!reached("light", "on", "unavailable"));
        assert!(reached("light", "on", "on"));
        assert!(reached("light", "on", "ON"), "case is not the difference being measured");
    }

    fn volume_case(want: u8) -> Case {
        Case {
            id: "v".into(),
            class: Class::ToolChoice,
            requires: super::super::house::Requirement::Device { domain: "media_player".into() },
            utterances: Default::default(),
            precondition: None,
            expect_device: None,
            expect_level: Some(want),
            expect_bystander_unchanged: false,
            expect_tool: None,
            expect_args: None,
            forbid_args: Vec::new(),
            expect_no_call: false,
            retry_allowed: true,
        }
    }

    fn speaker_at(pct: u32) -> House {
        House::parse(&format!(
            "- names: Speaker\n  domain: media_player\n  state: playing\n  volume_level: 0.{pct:02}\n"
        ))
    }

    /// The speaker, as a target naming itself and nothing else.
    fn speaker_target() -> Target {
        let device = speaker_at(20).get("Speaker").unwrap().clone();
        Target { group: vec![device.clone()], device, area: None, bystander: None }
    }

    /// The failure that motivated `expect_level`: Home Assistant refused a
    /// `HassSetVolume` outright, the tool name and the argument were both what
    /// the fixture asked for, and the case scored as a clean pass because
    /// nothing looked at the speaker.
    #[test]
    fn a_volume_that_never_landed_is_not_a_pass() {
        let target = speaker_target();
        let mut notes = Vec::new();
        assert!(!score_level(&volume_case(70), &target, &speaker_at(20), &mut notes));
        assert!(notes[0].contains("70%"), "the note has to say what was expected: {notes:?}");
    }

    #[test]
    fn a_volume_that_landed_is_a_pass() {
        let target = speaker_target();
        let mut notes = Vec::new();
        assert!(score_level(&volume_case(70), &target, &speaker_at(70), &mut notes));
        assert!(notes.is_empty());
    }

    /// A device reports its level rounded, so an exact comparison would fail a
    /// command that worked.
    #[test]
    fn a_rounding_difference_in_a_level_is_not_a_failure() {
        let target = speaker_target();
        let mut notes = Vec::new();
        assert!(score_level(&volume_case(70), &target, &speaker_at(71), &mut notes));
    }

    /// A case that names no level asserts nothing, and a device with no level
    /// to read is not a failure — it simply has nothing to say.
    #[test]
    fn an_unasserted_or_unreadable_level_is_not_a_failure() {
        let target = speaker_target();
        let mut notes = Vec::new();
        let mut case = volume_case(70);
        case.expect_level = None;
        assert!(score_level(&case, &target, &speaker_at(20), &mut notes));

        let plain = House::parse("- names: Speaker\n  domain: media_player\n  state: playing\n");
        assert!(score_level(&volume_case(70), &target, &plain, &mut notes));
    }
}
