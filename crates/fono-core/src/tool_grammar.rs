// SPDX-License-Identifier: GPL-3.0-only
//! Turn the tool catalogue into a grammar the model must write within.
//!
//! ## Why this exists
//!
//! A model running on the user's own machine writes a command out as text, and
//! a small one gets it wrong in ways nothing downstream can repair: an area the
//! house does not have, a device name that was never exposed, a field the
//! server insists on left out. Asking more clearly in the prompt was tried
//! three times and failed three times. A grammar is a different kind of fix —
//! the wrong answer stops being *possible* rather than being discouraged.
//!
//! ## What is universal here, and what is not
//!
//! This module knows **no vendor and no language**. It reads two things:
//!
//! 1. Each tool's own published schema — which fields exist, what type each
//!    one is, which are required, and any list of values the server itself
//!    declared. A server gets back exactly the rules it stated about itself.
//!    A loose schema yields a loose grammar; a tool with no schema at all gets
//!    no branch and stays entirely unconstrained, which is today's behaviour.
//! 2. A caller-supplied [`SlotValues`] — "on *this* server, a field with
//!    *this* name may hold these values". Both halves of that key matter: the
//!    field names come from the vendor layer, never from this file, and the
//!    server they were learned from travels with them.
//!
//! That split is the whole safety argument. A server Fono has never seen
//! *cannot* receive a Home Assistant rule, because nobody supplied slot values
//! against it — so it falls through to schema-only constraints and behaves as
//! it does today. Nothing about it is a promise to be kept by hand; it is
//! structural.
//!
//! ## Why slot values are needed at all
//!
//! Because the interesting slots are unconstrained upstream. A real Home
//! Assistant `tools/list` publishes **no** list of values for `area`, `name`
//! or `domain`, and **no** required-field list on any tool — those are bare
//! strings. So a purely schema-derived grammar, while correct, constrains
//! almost nothing on the three slots that actually fail. The lists Fono
//! supplies come from the house itself, read once at connect: an area name that
//! is not in the house cannot be the right answer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::Value;

use crate::tool_catalog::ToolRow;

/// The values that exist for a named field on a named server, plus any field
/// the caller wants forced to be present.
///
/// Keyed by *server and* field name. The field name is the only thing this
/// module can match on without knowing a vendor; the server is what keeps one
/// home's answers off another server's tools. A field nobody supplies values
/// for keeps whatever its schema said.
///
/// Both halves are load-bearing, and the second one is easy to leave out. With
/// the field name alone, two connected servers merge into a house that is
/// neither: `name` is the commonest parameter name there is, so the second
/// server's `name` field gets narrowed to the first server's devices and the
/// correct call becomes *unwritable* — no legal value exists. That is worse
/// than the mistake the rails were built to stop, where at least the wrong
/// answer was sayable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotValues {
    per_server: BTreeMap<String, ServerSlots>,
}

/// One server's answers about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ServerSlots {
    values: BTreeMap<String, Vec<String>>,
    required: BTreeSet<String>,
}

impl SlotValues {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the complete set of values a field may take on one server.
    ///
    /// An empty list is ignored rather than stored: it would mean "this field
    /// can hold nothing", which no caller means and which would make every
    /// tool using the field unwritable. A house that has not finished waking
    /// up answers with nothing, and must leave the field alone.
    pub fn set(&mut self, source: &str, field: &str, values: Vec<String>) {
        let cleaned: Vec<String> = dedup_sorted(values);
        if !cleaned.is_empty() {
            self.per_server
                .entry(source.to_owned())
                .or_default()
                .values
                .insert(field.to_owned(), cleaned);
        }
    }

    /// Insist a field is present whenever a tool on that server declares it,
    /// even though the schema calls it optional.
    ///
    /// This *contradicts* the server's own schema, so it is only ever reached
    /// through the vendor layer.
    pub fn require(&mut self, source: &str, field: &str) {
        self.per_server.entry(source.to_owned()).or_default().required.insert(field.to_owned());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.per_server.values().all(|s| s.values.is_empty() && s.required.is_empty())
    }

    /// What one field of one server's tool may hold, if anything was supplied.
    fn get(&self, source: &str, field: &str) -> Option<&Vec<String>> {
        self.per_server.get(source)?.values.get(field)
    }

    fn is_required(&self, source: &str, field: &str) -> bool {
        self.per_server.get(source).is_some_and(|s| s.required.contains(field))
    }
}

fn dedup_sorted(values: Vec<String>) -> Vec<String> {
    let mut v: Vec<String> =
        values.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
    v.sort();
    v.dedup();
    v
}

/// The wrapper the local backends ask the model to put a command inside.
/// Mirrors `fono_assistant::local_tools`, which owns the prompt side.
const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// The value that means "every kind of thing here", offered alongside the real
/// kinds a caller supplies.
///
/// It exists because making a field compulsory would otherwise cost the user a
/// sentence they are entitled to: "turn everything off in here". No server
/// accepts this value, so whoever offers it is responsible for taking it back
/// out before the call goes anywhere — which is the point. Right now a command
/// that reaches a whole area and one that forgot to say which kind of thing it
/// meant look identical, and both open the blinds. With this, the record says
/// which of the two happened.
pub const ANY_KIND: &str = "__all__";

/// Where the grammar takes over.
///
/// Everything before an opener is ordinary talking and is left completely
/// alone — this is what keeps stories, jokes and explanations as free as they
/// were. The bracketed group in each pattern tells llama.cpp that the
/// constrained part starts at the `{`, so the grammar below describes the call
/// object and nothing else.
///
/// # Why there is more than one
///
/// There has to be one pattern for **every** spelling the reply parser is
/// willing to read as a command, and the parser reads three. Fono asks for the
/// tagged form, but it also accepts a fenced code block and a reply that is
/// nothing but the JSON object, because small models produce all three and
/// refusing two of them would turn a good command into prose read aloud.
///
/// While only the tagged form armed the rails, the other two were a way out of
/// them. That is not a theoretical gap — it is what a house full of traces
/// actually recorded, with the rails switched on: an area called `Kitchen
/// display`, a kind of device called `roller`, and an `area` written as a list
/// when the server requires a string. Every one of those is unwritable under
/// the grammar, so every one of them was written down a path the grammar was
/// never watching. The setting looked on and did nothing.
///
/// So the rule is: whatever the parser will honour, the rails must cover. The
/// two lists are stated next to each other for that reason, and
/// `every_accepted_opener_arms_the_rails` fails the build if they drift.
#[must_use]
pub fn trigger_patterns() -> Vec<String> {
    vec![
        // The form Fono asks for.
        format!("{OPEN}\\s*(\\{{)"),
        // A fenced block, with or without the language tag.
        "```(?:json)?\\s*(\\{)".to_string(),
        // A reply that is nothing but the call. `^` is start-of-buffer here —
        // the pattern is searched, not full-matched, because it does not also
        // end in `$`, so this fires on a leading brace and never on one that
        // turns up mid-sentence.
        "^\\s*(\\{)".to_string(),
    ]
}

/// Build a grammar covering every tool in `tools`, or `None` when there is
/// nothing to constrain.
///
/// `None` is the honest answer in more cases than it looks: no tools offered,
/// or not one of them describing a single field. Returning a grammar that
/// matches everything would cost the same and prove nothing.
#[must_use]
pub fn build(tools: &[ToolRow], slots: &SlotValues) -> Option<String> {
    let mut branches = Vec::new();
    let mut rules = String::new();
    let mut lists = Choices::default();
    for (i, tool) in tools.iter().enumerate() {
        let label = format!("c{i}");
        rules.push_str(&call_rule(&label, tool, slots, &mut lists));
        branches.push(label);
    }
    if branches.is_empty() {
        return None;
    }

    let mut g = String::new();
    // A call, or a call wrapped under the name the model was shown. The parser
    // unwraps that nesting, so leaving it out of the rails would be another
    // way past them — the same mistake as covering only one opener.
    //
    // The closer is optional so a model that stops at the object, or that ends
    // its turn instead, is not driven into a dead end it cannot leave. So is
    // the fence, for a model that opened one.
    let _ = writeln!(
        g,
        "root ::= (call | wrapped) ws {}? ws {}?",
        quoted_literal(CLOSE),
        quoted_literal("```")
    );
    let _ = writeln!(g, "call ::= {}", branches.join(" | "));
    let _ = writeln!(
        g,
        "wrapped ::= \"{{\" ws {} ws \":\" ws call ws \"}}\"",
        quoted_literal("\"tool_call\"")
    );
    g.push_str(&rules);
    g.push_str(&lists.text);
    g.push_str(SHARED_RULES);
    Some(g)
}

/// Every closed list of values the grammar offers, each written once.
///
/// A list of choices is one fact — "a device in this home is one of these
/// seventy-seven" — and one fact belongs in one place. Written out afresh at
/// every field of every tool that has one, a stock Home Assistant catalogue
/// reached 117 KB of grammar, **69 % of it the same device list repeated
/// thirty-eight times**: once for each of the nineteen tools that can name a
/// device, and twice over because a field is written into both the
/// nothing-yet and the something-already position (see [`argument_rules`]).
/// The whole text is rebuilt and re-parsed every turn, on the path the user is
/// waiting on.
///
/// Size is the symptom. The defect is that the grammar had no *name* for "a
/// device in this home", so the answer had to be spelled out wherever it was
/// needed. Nothing can disagree today, because one pass writes every copy from
/// one list — but the moment that list becomes conditional, as it does when two
/// servers each have their own house, fifty inline copies are fifty chances to
/// spell out the wrong one. A rule with a name can only be pointed at.
///
/// Keyed by the list itself rather than by the field, so a list that turns up
/// twice collapses however it got here: the server's own `device_class` values
/// repeat across eleven tools of a stock house.
#[derive(Default)]
struct Choices {
    /// Rule name for a list of values, keyed by the values.
    named: BTreeMap<Vec<String>, String>,
    /// The rules themselves, in the order they were first needed.
    text: String,
}

impl Choices {
    /// The rule holding `values`, defining it the first time it is asked for.
    ///
    /// `field` only decorates the name so a person can read the grammar; what
    /// keeps two rules apart is the number in front, which is why an awkward
    /// field name is safe to cut about below.
    fn rule(&mut self, field: &str, values: &[String]) -> String {
        if let Some(name) = self.named.get(values) {
            return name.clone();
        }
        let name = format!("list{}-{}", self.named.len(), rule_label(field));
        let choices: Vec<String> = values.iter().map(|s| json_string_literal(s)).collect();
        let _ = writeln!(self.text, "{name} ::= {}", choices.join(" | "));
        self.named.insert(values.to_vec(), name.clone());
        name
    }
}

/// A field name cut down to what GBNF allows in a rule name.
///
/// Purely for reading: anything outside the allowed set is dropped, and a field
/// named entirely in characters GBNF will not take reads as `values`. Two fields
/// are free to reduce to the same label because the caller has already made the
/// name unique.
///
/// The allowed set is letters, digits and the hyphen — exactly what
/// `is_word_char` in llama.cpp's grammar parser takes, and **not** the
/// underscore. Letting one through cost the whole grammar rather than one rule:
/// the parser stops at the first character it does not recognise and throws away
/// every rule it had, so one field named `device_class` on a stock Home
/// Assistant meant every command in a run was written unconstrained, with
/// nothing but a warning line to say so. A label exists to be read, so it is the
/// cheapest possible thing to lose a character from.
fn rule_label(field: &str) -> String {
    let label: String =
        field.chars().take(24).filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if label.is_empty() {
        "values".to_string()
    } else {
        label
    }
}

/// One tool: its name is fixed, and its arguments are its own schema.
///
/// The key holding the arguments is written as `arguments`, which is what the
/// prompt asks for, or `parameters`, which is what several small models emit
/// regardless. The parser reads both, so the rails have to allow both: a
/// grammar that permitted only the asked-for spelling would force the model
/// off a path its reply was already going to be understood on.
fn call_rule(label: &str, tool: &ToolRow, slots: &SlotValues, lists: &mut Choices) -> String {
    let fields = fields_of(&tool.source, &tool.schema, slots, lists);
    let args = if fields.is_empty() {
        // Nothing declared, so nothing to say about the arguments beyond their
        // being a JSON object. The server told us no more than that.
        "obj".to_string()
    } else {
        format!("\"{{\" ws {label}-a0 ws \"}}\"")
    };
    let mut out = format!(
        "{label} ::= \"{{\" ws {} ws \":\" ws {} ws \",\" ws ({} | {}) ws \":\" ws {args} ws \"}}\"\n",
        quoted_literal("\"name\""),
        quoted_literal(&format!("\"{}\"", tool.name)),
        quoted_literal("\"arguments\""),
        quoted_literal("\"parameters\""),
    );
    out.push_str(&argument_rules(label, &fields));
    out
}

/// One field of a tool's arguments, reduced to what the grammar needs.
struct Field {
    name: String,
    value: String,
    required: bool,
}

/// Read a tool's schema into an ordered field list.
///
/// The order is alphabetical by field name, because a JSON object parses into a
/// sorted map. That is worth relying on: the grammar text has to be identical
/// between runs, or a measured comparison could never attribute a difference to
/// anything.
fn fields_of(source: &str, schema: &Value, slots: &SlotValues, lists: &mut Choices) -> Vec<Field> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let declared: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    props
        .iter()
        .map(|(name, spec)| Field {
            value: value_rule(source, name, spec, slots, lists),
            required: declared.contains(name.as_str()) || slots.is_required(source, name),
            name: name.clone(),
        })
        .collect()
}

/// What may appear on the right of one field.
///
/// The server's own list of values wins whenever it published one: that is the
/// server being specific about itself, and overriding it would be Fono
/// guessing. Only a field the server left wide open is narrowed to the values
/// the caller supplied.
fn value_rule(
    source: &str,
    name: &str,
    spec: &Value,
    slots: &SlotValues,
    lists: &mut Choices,
) -> String {
    if let Some(list) = spec.get("enum").and_then(Value::as_array) {
        // The server's own order is kept: it is the server being specific about
        // itself, and sorting would be Fono talking over it.
        let choices: Vec<String> =
            list.iter().filter_map(Value::as_str).map(str::to_owned).collect();
        if !choices.is_empty() {
            return lists.rule(name, &choices);
        }
    }

    let ty = spec.get("type").and_then(Value::as_str).unwrap_or("");
    // An array narrows to a list of the supplied values, because that is how
    // these fields are actually shaped: `domain` is an array of strings. The
    // item rule appears twice here — one more reason it is a rule name and not
    // the list itself.
    if ty == "array" {
        let item = spec.get("items").unwrap_or(&Value::Null);
        let inner = value_rule(source, name, item, slots, lists);
        return format!("(\"[\" ws ({inner} (ws \",\" ws {inner})*)? ws \"]\")");
    }

    if let Some(values) = slots.get(source, name) {
        // Only ever applied where the schema left room for it. A field the
        // server typed as a number is not silently turned into a word.
        if ty.is_empty() || ty == "string" {
            return lists.rule(name, values);
        }
    }

    match ty {
        "string" => "str".to_string(),
        "integer" => "int".to_string(),
        "number" => "num".to_string(),
        "boolean" => "bool".to_string(),
        "object" => "obj".to_string(),
        _ => "val".to_string(),
    }
}

/// Rules that walk the fields in a fixed order, letting optional ones be
/// skipped while required ones cannot be.
///
/// Two rules per position, because whether a comma is needed depends on
/// whether anything has been written yet: the `a` rules are "nothing written
/// so far", the `b` rules are "something was". That is the whole trick, and it
/// is what lets a required field be genuinely unskippable instead of merely
/// mentioned in the prompt.
fn argument_rules(label: &str, fields: &[Field]) -> String {
    let mut out = String::new();
    let n = fields.len();
    for (j, f) in fields.iter().enumerate() {
        let pair =
            format!("{} ws \":\" ws {}", quoted_literal(&format!("\"{}\"", f.name)), f.value);
        let next_b = format!("{label}-b{}", j + 1);
        let next_a = format!("{label}-a{}", j + 1);
        if f.required {
            let _ = writeln!(out, "{label}-a{j} ::= {pair} {next_b}");
            let _ = writeln!(out, "{label}-b{j} ::= ws \",\" ws {pair} {next_b}");
        } else {
            let _ = writeln!(out, "{label}-a{j} ::= ({pair} {next_b}) | {next_a}");
            let _ = writeln!(out, "{label}-b{j} ::= (ws \",\" ws {pair} {next_b}) | {next_b}");
        }
    }
    // The tail: everything has been written, so there is nothing left to allow.
    let _ = writeln!(out, "{label}-a{n} ::= \"\"");
    let _ = writeln!(out, "{label}-b{n} ::= \"\"");
    out
}

/// A GBNF literal for text that must appear verbatim.
fn quoted_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A GBNF literal for a JSON string value — the quotes are part of what the
/// model has to write, so they are inside the literal.
fn json_string_literal(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    quoted_literal(&format!("\"{escaped}\""))
}

/// The generic JSON shapes every branch shares.
///
/// Whitespace is permitted between tokens rather than fixed, so the grammar
/// never fights the model over spacing it was trained to produce.
const SHARED_RULES: &str = r#"
ws ::= [ \t\n]*
str ::= "\"" ([^"\\] | "\\" ["\\/bfnrt])* "\""
int ::= "-"? [0-9]+
num ::= "-"? [0-9]+ ("." [0-9]+)?
bool ::= "true" | "false"
arr ::= "[" ws (val (ws "," ws val)*)? ws "]"
obj ::= "{" ws (str ws ":" ws val (ws "," ws str ws ":" ws val)*)? ws "}"
val ::= str | num | bool | "null" | arr | obj
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_catalog::{Capability, VerifyClass};

    fn tool(name: &str, schema: Value) -> ToolRow {
        tool_on("ha", name, schema)
    }

    fn tool_on(source: &str, name: &str, schema: Value) -> ToolRow {
        ToolRow {
            source: source.into(),
            name: name.into(),
            description: String::new(),
            schema,
            schema_hash: String::new(),
            capability: Capability::Safe,
            verify_class: VerifyClass::PostCondition,
            readback_tool: None,
            available: true,
            enabled: true,
            user_touched: false,
            runs: 0,
            last_run: None,
        }
    }

    fn turn_on() -> ToolRow {
        tool(
            "HassTurnOn",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "area": { "type": "string" },
                    "name": { "type": "string" },
                    "domain": { "type": "array", "items": { "type": "string" } },
                },
            }),
        )
    }

    fn house() -> SlotValues {
        let mut s = SlotValues::new();
        s.set("ha", "area", vec!["Kitchen".into(), "Master bedroom".into()]);
        s.set("ha", "name", vec!["Hall lamp".into()]);
        s.set("ha", "domain", vec!["light".into(), "cover".into()]);
        s
    }

    /// The failure this whole module exists for: the model invented an area
    /// the house does not have. Every real area appears as a choice, and
    /// nothing else can be written in that position.
    #[test]
    fn only_areas_the_house_actually_has_can_be_written() {
        let g = build(&[turn_on()], &house()).expect("a grammar");
        assert!(g.contains(r#""\"Kitchen\"""#), "{g}");
        assert!(g.contains(r#""\"Master bedroom\"""#), "{g}");
        assert!(!g.contains("Master bathroom mirror"), "an invented area cannot be in the menu");
    }

    /// The tool name is fixed per branch, so a model cannot call something
    /// that was never offered or that the user switched off.
    #[test]
    fn only_the_offered_tools_can_be_named() {
        let g = build(&[turn_on()], &house()).expect("a grammar");
        assert!(g.contains(r#""\"HassTurnOn\"""#), "{g}");
        assert!(!g.contains("HassTurnOff"));
    }

    /// A server that published its own list of values is right about itself,
    /// and Fono must not talk over it.
    #[test]
    fn a_servers_own_list_of_values_wins() {
        let t = tool(
            "HassMediaPause",
            serde_json::json!({
                "properties": { "area": { "type": "string", "enum": ["Cinema"] } },
            }),
        );
        let g = build(&[t], &house()).expect("a grammar");
        assert!(g.contains(r#""\"Cinema\"""#), "{g}");
        assert!(!g.contains("Kitchen"), "the server said Cinema; our own list must not override");
    }

    /// The safety property that keeps unfamiliar servers working: a tool whose
    /// fields we know nothing about is constrained to valid JSON and no more.
    #[test]
    fn a_server_we_know_nothing_about_keeps_todays_freedom() {
        let t = tool(
            "queue_job",
            serde_json::json!({ "properties": { "payload": { "type": "string" } } }),
        );
        let g = build(&[t], &house()).expect("a grammar");
        // Its one field is a bare string, not our device list.
        assert!(g.contains("\"payload\\\"\" ws \":\" ws str"), "{g}");
        assert!(!g.contains("Hall lamp"), "no slot list may leak onto a field nobody claimed");
    }

    /// A tool with no schema at all gets no field rules — the server said
    /// nothing, so Fono says nothing.
    #[test]
    fn a_tool_without_a_schema_is_left_open() {
        let g = build(&[tool("mystery", serde_json::json!({}))], &house()).expect("a grammar");
        assert!(g.contains("ws obj ws"), "{g}");
        assert!(!g.contains("mystery-a0"), "{g}");
    }

    /// A field the server calls required cannot be skipped: the `a` rule for
    /// it has no alternative that steps over it.
    #[test]
    fn a_required_field_has_no_way_around_it() {
        let t = tool(
            "HassSetPosition",
            serde_json::json!({
                "properties": { "position": { "type": "integer" } },
                "required": ["position"],
            }),
        );
        let g = build(&[t], &house()).expect("a grammar");
        let line = g.lines().find(|l| l.starts_with("c0-a0 ::=")).expect("the first field rule");
        assert!(!line.contains(" | "), "a required field must not be skippable: {line}");
        assert!(line.contains("int"), "{line}");
    }

    /// An optional field must stay skippable, or a grammar would demand
    /// answers the user never gave.
    #[test]
    fn an_optional_field_can_be_left_out() {
        let g = build(&[turn_on()], &house()).expect("a grammar");
        let line = g.lines().find(|l| l.starts_with("c0-a0 ::=")).expect("the first field rule");
        assert!(line.contains(" | "), "an optional field needs a way past it: {line}");
    }

    /// The vendor layer can insist on a field the schema calls optional. This
    /// is the one rule that contradicts a server, and it only ever arrives
    /// from outside this module.
    #[test]
    fn the_caller_can_insist_on_a_field_the_schema_calls_optional() {
        let mut slots = house();
        slots.require("ha", "domain");
        let g = build(&[turn_on()], &slots).expect("a grammar");
        // Fields come out sorted, so of `area` / `domain` / `name` the required
        // one sits second. A required field's rule has no alternative that
        // jumps past it; an optional one does.
        let rule = |name: &str| {
            g.lines()
                .find(|l| l.starts_with(&format!("{name} ::=")))
                .unwrap_or_else(|| panic!("no {name} rule in:\n{g}"))
                .to_string()
        };
        assert!(rule("c0-a1").contains(r#""\"domain\"""#), "{}", rule("c0-a1"));
        assert!(
            !rule("c0-a1").contains("| c0-a2"),
            "a field the vendor requires must not be skippable: {}",
            rule("c0-a1")
        );
        assert!(
            rule("c0-a0").contains("| c0-a1"),
            "an untouched field stays optional: {}",
            rule("c0-a0")
        );
    }

    /// Nothing offered means no grammar, rather than a grammar that permits
    /// everything: constructing one would cost the same and prove nothing.
    #[test]
    fn no_tools_means_no_grammar() {
        assert!(build(&[], &house()).is_none());
    }

    /// A house that answers with nothing has not been emptied, it has not
    /// woken up. Storing that as "this field accepts no value" would make
    /// every tool using the field impossible to write.
    #[test]
    fn an_empty_list_of_values_is_ignored_rather_than_enforced() {
        let mut slots = SlotValues::new();
        slots.set("ha", "area", Vec::new());
        slots.set("ha", "name", vec!["  ".into()]);
        assert!(slots.is_empty(), "nothing usable was supplied, so nothing may be enforced");
        let g = build(&[turn_on()], &slots).expect("a grammar");
        assert!(g.contains("\"area\\\"\" ws \":\" ws str"), "the field stays a plain string: {g}");
    }

    /// The grammar text has to be identical between runs, or a measured
    /// comparison could never attribute a difference to anything.
    #[test]
    fn the_grammar_is_byte_stable() {
        let a = build(&[turn_on()], &house()).expect("a grammar");
        let b = build(&[turn_on()], &house()).expect("a grammar");
        assert_eq!(a, b);
    }

    /// One server's house must not be imposed on another server's tools.
    ///
    /// This is the case the module claims to be structurally safe against, and
    /// it used not to be: slot values were keyed by field name alone, so the
    /// house Fono had read narrowed *every* server's field of that name.
    /// `name` is the commonest parameter name there is, so a notes server, a
    /// music server, a shell — anything with a `name` — could only be called
    /// with the name of a lamp. Not over-constrained: **unwritable**, because no
    /// legal value existed at all.
    #[test]
    fn one_servers_house_does_not_narrow_another_servers_fields() {
        let notes = tool_on(
            "notes",
            "create_note",
            serde_json::json!({ "properties": { "name": { "type": "string" } } }),
        );
        let g = build(&[turn_on(), notes], &house()).expect("a grammar");

        // The house still applies where it came from.
        assert!(g.contains(r#""\"Hall lamp\"""#), "the home's own tools stay narrowed: {g}");

        // And the other server's field of the same name is left as its schema
        // wrote it, so a note can still be called anything.
        let rule = g.lines().find(|l| l.starts_with("c1-a0 ::=")).expect("the note's field");
        assert!(rule.contains("str"), "the other server keeps a plain string: {rule}");
        assert!(!rule.contains("list"), "and is not held to this house: {rule}");
    }

    /// The same field name on two servers, each with its own values, stays two
    /// separate answers.
    #[test]
    fn two_homes_keep_their_own_areas() {
        let mut slots = SlotValues::new();
        slots.set("here", "area", vec!["Kitchen".into()]);
        slots.set("there", "area", vec!["Cabin".into()]);
        let schema = serde_json::json!({ "properties": { "area": { "type": "string" } } });
        let g = build(
            &[
                tool_on("here", "HassTurnOn", schema.clone()),
                tool_on("there", "HassTurnOn", schema),
            ],
            &slots,
        )
        .expect("a grammar");

        let rule = |label: &str| {
            g.lines()
                .find(|l| l.starts_with(&format!("{label} ::=")))
                .unwrap_or_default()
                .to_string()
        };
        let list_of = |label: &str| {
            let r = rule(label);
            let name = r
                .split_whitespace()
                .find(|t| t.starts_with("list"))
                .expect("the field must point at a list")
                .to_string();
            rule(&name)
        };
        assert!(list_of("c0-a0").contains("Kitchen"), "{g}");
        assert!(!list_of("c0-a0").contains("Cabin"), "{g}");
        assert!(list_of("c1-a0").contains("Cabin"), "{g}");
        assert!(!list_of("c1-a0").contains("Kitchen"), "{g}");
    }

    /// Ordinary talking must be untouched. The grammar only starts at the
    /// opening brace of a command, which is what the triggers say.
    ///
    /// One pattern per opener the reply parser honours, each marking the brace
    /// as the point the rails take over. When this list was shorter than the
    /// parser's, the extra openers were a way straight past the constraint.
    #[test]
    fn the_grammar_starts_only_where_a_command_starts() {
        let p = trigger_patterns();
        assert_eq!(p.len(), 3, "one pattern per opener the parser accepts: {p:?}");
        assert!(p[0].starts_with("<tool_call>"), "{p:?}");
        assert!(p[1].starts_with("```"), "{p:?}");
        assert!(p[2].starts_with('^'), "a bare object is a call too: {p:?}");
        for pattern in &p {
            assert!(pattern.contains("(\\{)"), "the brace must be the captured start: {pattern}");
            assert!(!pattern.ends_with('$'), "these are searched, not full-matched: {pattern}");
        }
    }

    /// The two spellings of the arguments key the parser reads must both be
    /// writable, or the rails would push the model off a path its reply was
    /// already going to be understood on.
    #[test]
    fn both_spellings_of_the_arguments_key_are_allowed() {
        let g = build(&[turn_on()], &house()).expect("a grammar");
        assert!(g.contains(r#""\"arguments\"""#), "{g}");
        assert!(g.contains(r#""\"parameters\"""#), "{g}");
    }

    /// The parser unwraps a call nested under `tool_call`, so the rails have to
    /// allow that shape as well — an unreachable nesting is one more door.
    #[test]
    fn a_call_nested_under_its_own_name_is_still_covered() {
        let g = build(&[turn_on()], &house()).expect("a grammar");
        assert!(g.contains(r#"wrapped ::= "{" ws "\"tool_call\"""#), "{g}");
        assert!(g.lines().any(|l| l.starts_with("root ::=") && l.contains("wrapped")), "{g}");
    }

    /// A name containing a quote must not break out of its literal, or the
    /// grammar would fail to parse and take tool calling down with it.
    #[test]
    fn an_awkward_device_name_cannot_break_the_grammar() {
        let mut slots = SlotValues::new();
        slots.set("ha", "name", vec![r#"Nick's "big" lamp"#.into()]);
        let g = build(&[turn_on()], &slots).expect("a grammar");
        assert!(g.contains(r#"\"Nick's \\\"big\\\" lamp\""#), "{g}");
    }

    fn times(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    /// The claim: a list of values is written **once**, however many tools can
    /// take it.
    ///
    /// It used not to be. Each field carried its own copy of the whole list, and
    /// each field is written into two rules, so a stock Home Assistant house put
    /// its device list into the grammar thirty-eight times — 117 KB of text
    /// rebuilt and re-parsed every turn, 69 % of it that one repeat.
    #[test]
    fn a_list_of_values_is_written_once_however_many_tools_can_take_it() {
        let lamp = r#""\"Hall lamp\"""#;
        let one = build(&[turn_on()], &house()).expect("a grammar");
        assert_eq!(times(&one, lamp), 1, "the house belongs in one rule: {one}");

        let many: Vec<ToolRow> =
            (0..20).map(|i| tool(&format!("HassTurnOn{i}"), turn_on().schema)).collect();
        let lots = build(&many, &house()).expect("a grammar");
        assert_eq!(times(&lots, lamp), 1, "twenty tools must not mean twenty copies of the house");

        // And the field points at that one rule rather than restating it.
        let named = lots
            .lines()
            .find(|l| l.starts_with("c0-a2 ::="))
            .expect("the rule for the device field");
        assert!(named.contains("list"), "the field must reference the list: {named}");
        assert!(!named.contains("Hall lamp"), "and must not restate it: {named}");
    }

    /// A list the *server* published repeats too — `device_class` is identical
    /// across eleven tools of a stock house — so sharing is keyed on the values
    /// and not on which field or tool asked.
    #[test]
    fn the_same_list_on_two_tools_becomes_one_rule() {
        let kinds = serde_json::json!({
            "properties": { "device_class": { "type": "string", "enum": ["door", "window"] } },
        });
        let g = build(&[tool("HassTurnOn", kinds.clone()), tool("HassTurnOff", kinds)], &house())
            .expect("a grammar");
        assert_eq!(times(&g, r#""\"door\"""#), 1, "one list, one rule: {g}");
        assert_eq!(g.lines().filter(|l| l.contains("::=") && l.contains("door")).count(), 1);
    }

    /// A whole realistic catalogue has to stay small, because the text is
    /// rebuilt and handed to llama.cpp to parse on **every** turn.
    ///
    /// The shape here is a real house's: 22 tools, 14 areas, 77 devices averaging
    /// nineteen characters, 8 kinds. That measured 117,430 bytes when every list
    /// was inlined. The ceiling is deliberately far below it and far above what
    /// this produces — it is here to fail if a list ever goes back to being
    /// written out per field, not to police ordinary changes.
    #[test]
    fn a_whole_house_worth_of_tools_stays_small() {
        let mut slots = SlotValues::new();
        slots.set("ha", "area", (0..14).map(|i| format!("Area {i}")).collect());
        slots.set("ha", "name", (0..77).map(|i| format!("Some device number {i}")).collect());
        slots.set("ha", "domain", (0..8).map(|i| format!("kind{i}")).collect());
        let tools: Vec<ToolRow> =
            (0..22).map(|i| tool(&format!("HassDoThing{i}"), turn_on().schema)).collect();

        let g = build(&tools, &slots).expect("a grammar");
        assert!(g.len() < 32 * 1024, "the whole catalogue came to {} bytes", g.len());
        assert_eq!(times(&g, r#""\"Some device number 42\"""#), 1, "still one copy");
    }

    /// A field name GBNF would not accept inside a rule name must not produce a
    /// grammar llama.cpp then refuses — which would silently disarm the rails
    /// altogether, since a rejected grammar samples exactly like none at all.
    ///
    /// The underscore is in here deliberately, and it is the whole reason this
    /// test is worth reading twice. An earlier version of it allowed one, which
    /// is what the code did too, so the test agreed with the mistake and passed:
    /// llama.cpp's rule names take letters, digits and the hyphen and nothing
    /// else. `device_class` is not a contrived example — a stock Home Assistant
    /// publishes it, so every command in a real house was written unconstrained.
    #[test]
    fn an_awkward_field_name_still_makes_a_rule_name_gbnf_accepts() {
        let mut slots = SlotValues::new();
        slots.set("ha", "who's there?", vec!["Kitchen".into()]);
        slots.set("ha", "привет", vec!["Office".into()]);
        slots.set("ha", "device_class", vec!["door".into()]);
        let t = tool(
            "odd",
            serde_json::json!({
                "properties": {
                    "who's there?": { "type": "string" },
                    "привет": {},
                    "device_class": { "type": "string" },
                },
            }),
        );
        let g = build(&[t], &slots).expect("a grammar");
        for line in g.lines().filter(|l| l.contains("::=")) {
            let name = line.split(" ::=").next().expect("a rule name");
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "rule name {name} is not one GBNF will take, from:\n{g}"
            );
        }
        // All three are still narrowed — an awkward name costs nothing but its
        // label.
        assert!(g.contains(r#""\"Kitchen\"""#) && g.contains(r#""\"Office\"""#), "{g}");
        assert!(g.contains(r#""\"door\"""#), "{g}");
    }
}
