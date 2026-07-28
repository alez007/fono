// SPDX-License-Identifier: GPL-3.0-only
//! Did the assistant answer in the language it was spoken to in?
//!
//! Worth scoring separately because it fails independently of everything else:
//! a model can pick the right device, move it, and describe the result in
//! English to someone who spoke Romanian. Nothing else in the suite notices,
//! and to the user it is the most obvious defect in the turn.
//!
//! The hard part is that the replies are short — "Done.", "Gata." — and short
//! text is exactly where statistical language identification gives up. So this
//! answers in three tiers, and the important one is the third: **when it
//! cannot tell, it says so**, and the case is not scored on language at all.
//! A benchmark that guesses here would report a language failure rate made of
//! its own noise.

use whatlang::{Detector, Lang};

/// Whether `reply` reads as `want`, or `None` when there is not enough to
/// judge.
///
/// `want` is a language tag; a region suffix is ignored (`pt-BR` is judged as
/// Portuguese), because no model is being tested on its Brazilian.
#[must_use]
pub fn matches(reply: &str, want: &str) -> Option<bool> {
    let text = reply.trim();
    if text.is_empty() {
        return None;
    }
    let want = base(want);

    // Tier one: a marker word only one of the candidate languages uses.
    // Beats statistics outright on a two-word confirmation, which is most of
    // what a command reply is.
    if let Some(seen) = by_marker(text) {
        return Some(seen == want);
    }

    // Tier two: statistics, but only with enough text to be worth trusting
    // and only when the detector is confident.
    let long_enough = text.split_whitespace().count() >= 5 && text.chars().count() >= 24;
    if long_enough {
        if let Some(info) = Detector::new().detect(text) {
            if info.confidence() >= 0.65 {
                return Some(code_for(info.lang()) == want);
            }
        }
    }

    // Tier three: no opinion. Better a missing number than a wrong one.
    None
}

/// Strip a region suffix: `ro-RO` is Romanian.
fn base(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// Words that belong to exactly one of the languages the suite uses, and that
/// turn up in ordinary spoken replies.
///
/// Every entry has to be unique across the whole table — a word shared by two
/// of these languages would silently mislabel one of them — and diacritics are
/// kept, since dropping them is itself a bug worth catching elsewhere.
///
/// Each entry is padded with spaces and matched against a reply whose
/// punctuation has been flattened to spaces, so a marker matches a whole word
/// wherever it sits: `Gata.` and `Gata, e aprinsă` both carry `gata`, and
/// `the` never matches inside `theatre`.
const MARKERS: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            " the ",
            " i've ",
            " i have ",
            " turned ",
            " switched ",
            " is now ",
            " sorry ",
            " can't ",
            " cannot ",
            " done ",
        ],
    ),
    (
        "ro",
        &[
            " am ",
            " este ",
            " și ",
            " să ",
            " gata ",
            " lumina ",
            " aprins ",
            " aprinsă ",
            " stins ",
            " stinsă ",
            " oprit ",
            " pornit ",
            " acum ",
        ],
    ),
    (
        "fr",
        &[
            " j'ai ",
            " est ",
            " allumé ",
            " allumée ",
            " éteint ",
            " éteinte ",
            " lumière ",
            " c'est ",
            " fait ",
            " je ",
            " maintenant ",
            " désolé ",
        ],
    ),
    (
        "es",
        &[
            " he ",
            " está ",
            " encendido ",
            " encendida ",
            " apagado ",
            " apagada ",
            " luz ",
            " hecho ",
            " listo ",
            " ahora ",
            " siento ",
            " puedo ",
        ],
    ),
];

/// The single language whose markers appear, or `None` when none or several
/// do. Several means the reply is mixed, or a marker is not as unique as it
/// looked; in both cases silence is the honest answer.
fn by_marker(text: &str) -> Option<&'static str> {
    // Flatten anything that is not part of a word to a space, so punctuation
    // never hides a marker and a marker never matches mid-word. Apostrophes
    // survive because `j'ai` and `c'est` are two of the strongest signals.
    let flattened: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect();
    let hay = format!(" {} ", flattened.split_whitespace().collect::<Vec<_>>().join(" "));
    let mut found: Option<&'static str> = None;
    for (code, words) in MARKERS {
        if words.iter().any(|w| hay.contains(w)) {
            if found.is_some_and(|f| f != *code) {
                return None;
            }
            found = Some(code);
        }
    }
    found
}

/// The tag for a detected language, for the four the suite speaks.
fn code_for(lang: Lang) -> &'static str {
    match lang {
        Lang::Ron => "ro",
        Lang::Fra => "fr",
        Lang::Spa => "es",
        Lang::Eng => "en",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case that matters: a two-word confirmation, far too short for any
    /// statistical detector.
    #[test]
    fn judges_a_very_short_confirmation() {
        assert_eq!(matches("Gata.", "ro"), Some(true));
        assert_eq!(matches("Gata.", "en"), Some(false));
        assert_eq!(matches("Done.", "en"), Some(true));
    }

    /// The recorded failure: acted on correctly, answered in the wrong
    /// language.
    #[test]
    fn catches_an_english_reply_to_a_romanian_command() {
        assert_eq!(matches("I've turned the light on.", "ro"), Some(false));
    }

    #[test]
    fn recognises_french_and_spanish() {
        assert_eq!(matches("C'est fait.", "fr"), Some(true));
        assert_eq!(matches("Listo.", "es"), Some(true));
        assert_eq!(matches("Listo.", "fr"), Some(false));
    }

    /// A region suffix is not a different language.
    #[test]
    fn ignores_a_region_suffix() {
        assert_eq!(matches("Done.", "en-GB"), Some(true));
    }

    /// The important one. Anything unjudgeable must produce no score rather
    /// than a guess, or the language column fills up with the detector's own
    /// noise.
    #[test]
    fn says_nothing_when_it_cannot_tell() {
        assert_eq!(matches("", "en"), None);
        assert_eq!(matches("OK", "en"), None);
        assert_eq!(matches("42", "ro"), None);
    }

    /// A reply carrying markers from two languages is not evidence of either.
    #[test]
    fn a_mixed_reply_yields_no_verdict() {
        assert_eq!(matches("Gata, the light is on.", "ro"), None);
    }

    /// Longer text falls through to the detector, so unmarked sentences are
    /// still judged.
    #[test]
    fn falls_back_to_detection_on_longer_text() {
        let long = "Zonele de acoperire pentru dispozitivele conectate au fost configurate \
                    corespunzător de către administrator.";
        assert_eq!(matches(long, "ro"), Some(true));
    }
}
