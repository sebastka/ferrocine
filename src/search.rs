//! Searching items by their titles (shown, original, localized) and by the
//! names of their cast and crew.
//!
//! Each item stores a search text: those names folded (lower case, accents
//! removed, punctuation as spaces), one per line. A query is folded the same
//! way and matches when every one of its words appears, in any order:
//! "amelie" finds "Amélie", "keanu matrix" finds The Matrix.

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::media::{Meta, Person};

/// Lower case, without accents, punctuation as spaces, single-spaced.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.nfkd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)) {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The words of a query, folded.
pub fn terms(query: &str) -> Vec<String> {
    fold(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// What an item is found by: its titles and its people's names, folded.
pub fn index_text(meta: &Meta) -> String {
    let mut lines: Vec<String> = Vec::new();
    let titles = [
        Some(&meta.title),
        meta.original_title.as_ref(),
        meta.provider_title.as_ref(),
    ];
    for name in titles.into_iter().flatten().chain(meta.people.iter().map(|p| &p.name)) {
        let f = fold(name);
        if !f.is_empty() && !lines.contains(&f) {
            lines.push(f);
        }
    }
    lines.join("\n")
}

/// Why a search result matched, when not by its shown title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Matched {
    /// `original_title` (or the localized title), `cast` or `crew`.
    pub kind: &'static str,
    pub text: String,
    /// For cast and crew: `actor`, `director`, `writer`…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
}

/// The words of `terms` found in `text`.
fn hits(text: &str, terms: &[String]) -> usize {
    let f = fold(text);
    terms.iter().filter(|t| f.contains(t.as_str())).count()
}

/// Why an item matched `terms`: `None` when its shown title alone does;
/// else its other title, or the person, matching the most of the words the
/// shown title lacks (on a tie: the title, then an actor, then the first
/// billed).
pub fn why(title: &str, other_titles: &[Option<&str>], people: &[Person], terms: &[String]) -> Option<Matched> {
    // The words the shown title lacks: what the match must explain.
    let shown = fold(title);
    let missing: Vec<String> = terms.iter().filter(|t| !shown.contains(t.as_str())).cloned().collect();
    if missing.is_empty() {
        return None;
    }
    let terms = &missing;
    let best_title = other_titles
        .iter()
        .flatten()
        .filter(|t| fold(t) != fold(title))
        .map(|t| (hits(t, terms), *t))
        .max_by_key(|(n, _)| *n);
    let best_person = people
        .iter()
        .enumerate()
        .map(|(i, p)| (hits(&p.name, terms), std::cmp::Reverse(i), p))
        // A director who also stars is found as the star.
        .max_by_key(|(n, i, p)| (*n, p.role == "actor", *i));
    match (best_title, best_person) {
        (Some((t, text)), Some((p, _, _))) if t >= p && t > 0 => Some(Matched {
            kind: "original_title",
            text: text.to_owned(),
            role: None,
            character: None,
        }),
        (_, Some((p, _, person))) if p > 0 => Some(Matched {
            kind: if person.role == "actor" { "cast" } else { "crew" },
            text: person.name.clone(),
            role: Some(person.role.clone()),
            character: person.character.clone().filter(|c| !c.is_empty()),
        }),
        (Some((t, text)), None) if t > 0 => Some(Matched {
            kind: "original_title",
            text: text.to_owned(),
            role: None,
            character: None,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, role: &str, character: Option<&str>) -> Person {
        Person {
            name: name.into(),
            role: role.into(),
            character: character.map(String::from),
        }
    }

    #[test]
    fn folds_case_accents_and_punctuation() {
        assert_eq!(
            fold("Le Fabuleux Destin d'Amélie Poulain"),
            "le fabuleux destin d amelie poulain"
        );
        assert_eq!(fold("  Spider-Man: No Way Home! "), "spider man no way home");
        assert_eq!(fold("千と千尋の神隠し"), "千と千尋の神隠し");
        assert_eq!(fold("Ÿ ß Ø"), "y ß ø");
        assert_eq!(terms("Keanu  REEVES"), ["keanu", "reeves"]);
        assert!(terms(" - ").is_empty());
    }

    #[test]
    fn indexes_titles_and_people_once() {
        let meta = Meta {
            title: "Le Voyage de Chihiro".into(),
            original_title: Some("千と千尋の神隠し".into()),
            provider_title: Some("Le Voyage de Chihiro".into()),
            people: vec![
                person("Hayao Miyazaki", "director", None),
                person("Rumi Hîragi", "actor", Some("Chihiro")),
            ],
            ..Default::default()
        };
        assert_eq!(
            index_text(&meta),
            "le voyage de chihiro\n千と千尋の神隠し\nhayao miyazaki\nrumi hiragi"
        );
    }

    #[test]
    fn says_why_an_item_matched() {
        let people = [
            person("Lana Wachowski", "director", None),
            person("Keanu Reeves", "actor", Some("Neo")),
            person("Carrie-Anne Moss", "actor", Some("Trinity")),
        ];
        let original = [Some("Matrix, The")];
        assert_eq!(why("The Matrix", &original, &people, &terms("matrix")), None);
        let m = why("The Matrix", &original, &people, &terms("keanu")).unwrap();
        assert_eq!(
            (m.kind, m.text.as_str(), m.character.as_deref()),
            ("cast", "Keanu Reeves", Some("Neo"))
        );
        let m = why("The Matrix", &original, &people, &terms("wachowski")).unwrap();
        assert_eq!((m.kind, m.role.as_deref()), ("crew", Some("director")));
        let m = why(
            "Spirited Away",
            &[Some("Sen to Chihiro no Kamikakushi")],
            &[],
            &terms("chihiro"),
        )
        .unwrap();
        assert_eq!(
            (m.kind, m.text.as_str()),
            ("original_title", "Sen to Chihiro no Kamikakushi")
        );
        // Directing and starring: found as the star.
        let both = [
            person("Sylvester Stallone", "writer", None),
            person("Sylvester Stallone", "actor", Some("John Rambo")),
        ];
        let m = why("Rambo", &[], &both, &terms("stallone")).unwrap();
        assert_eq!((m.kind, m.character.as_deref()), ("cast", Some("John Rambo")));
        // "keanu matrix": the title has one word, the actor the other.
        let m = why("The Matrix", &original, &people, &terms("keanu matrix")).unwrap();
        assert_eq!(m.text, "Keanu Reeves");
    }
}
