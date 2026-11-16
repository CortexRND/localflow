//! Convert spoken symbol names in dictated text to the symbols themselves
//! (port of `localflow/symbols.py`, plus `discover_commands` from
//! `localflow/commands.py`).
//!
//! "config slash settings dash prod underscore v2" -> "config/settings-prod_v2".
//! With a registry of known command names, the words after a dictated slash
//! are snapped to a registered spelling: "slash skill part" -> "/skill_part".

#[path = "pycompat.rs"]
pub mod pycompat;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use pycompat::re::{self, Match, Node, Regex};
use pycompat::{chars, is_space, is_word, lower, parse_decimal, strip};

/// Glue both sides: "warm dash cache" -> "warm-cache". Order matters for the
/// alternation, exactly as the Python dict.
const JOINING: [(&str, &str); 3] = [("dash", "-"), ("hyphen", "-"), ("underscore", "_")];

fn joining(word: &str) -> Option<&'static str> {
    JOINING.iter().find(|(k, _)| *k == word).map(|(_, v)| *v)
}

fn joining_words() -> Node {
    re::alt(JOINING.iter().map(|(k, _)| re::lit_ci(k)).collect())
}

/// `\s*\b(dash|hyphen|underscore)\b[.,]?\s*` IGNORECASE
static JOIN_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::seq(vec![
            re::star(re::space()),
            re::boundary(),
            re::group(1, joining_words()),
            re::boundary(),
            re::opt(re::set(".,")),
            re::star(re::space()),
        ]),
        1,
    )
});

/// `(^|\s+)slash\b[.,]?\s*` IGNORECASE
static SLASH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::seq(vec![
            re::group(1, re::alt(vec![re::start(), re::plus(re::space())])),
            re::lit_ci("slash"),
            re::boundary(),
            re::opt(re::set(".,")),
            re::star(re::space()),
        ]),
        1,
    )
});

/// `\x00(\d+)\x00`
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| placeholder_pattern('\u{0}'));

/// `<mark>(\d+)<mark>` — shared with hot-phrase parking (`\x01`).
pub(crate) fn placeholder_pattern(mark: char) -> Regex {
    Regex::new(
        re::seq(vec![
            re::ch(mark),
            re::group(1, re::plus(re::digit())),
            re::ch(mark),
        ]),
        1,
    )
}

/// Replace `<mark>N<mark>` placeholders with `parked[N]`. Python raises
/// IndexError for an out-of-range N (only possible if the dictated text itself
/// contained the control character); here such a placeholder is left as-is.
pub(crate) fn restore_placeholders(pattern: &Regex, text: &str, parked: &[String]) -> String {
    pattern.sub(text, |m, t| {
        let (a, b) = m.span(1).unwrap_or((0, 0));
        match parse_decimal(&t[a..b]).and_then(|i| parked.get(i)) {
            Some(value) => value.clone(),
            None => m.text(t),
        }
    })
}

/// `(?:\s*[_-]\s*|\s+(?:dash|hyphen|underscore)\b[.,]?\s+|\s+)`
fn part_separator() -> Node {
    re::alt(vec![
        re::seq(vec![
            re::star(re::space()),
            re::set("_-"),
            re::star(re::space()),
        ]),
        re::seq(vec![
            re::plus(re::space()),
            joining_words(),
            re::boundary(),
            re::opt(re::set(".,")),
            re::plus(re::space()),
        ]),
        re::plus(re::space()),
    ])
}

/// Match "/" + a registered name. Longest names first; equal lengths are
/// ordered lexicographically (Python iterates a `set`, whose tie order is
/// hash-dependent).
fn command_pattern(names: &[String]) -> Option<Regex> {
    let unique: BTreeSet<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| !n.is_empty())
        .collect();
    let mut ordered: Vec<&str> = unique.into_iter().collect();
    ordered.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    if ordered.is_empty() {
        return None;
    }
    let alternatives = ordered
        .iter()
        .map(|name| {
            let mut parts = Vec::new();
            for (i, part) in name.split(['-', '_']).enumerate() {
                if i > 0 {
                    parts.push(part_separator());
                }
                parts.push(re::lit_ci(part));
            }
            re::seq(parts)
        })
        .collect();
    Some(Regex::new(
        re::seq(vec![
            re::not_after(|c| !is_space(c)),
            re::ch('/'),
            re::group(1, re::alt(alternatives)),
            re::not_ahead(re::pred(|c| is_word(c) || c == '-')),
            re::opt(re::group(
                2,
                re::seq(vec![
                    re::plus(re::set(".,!?")),
                    re::ahead(re::seq(vec![re::star(re::space()), re::dollar()])),
                ]),
            )),
        ]),
        2,
    ))
}

/// `(\s*[_-]\s*|\s+)`
static TOKEN_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::group(
            1,
            re::alt(vec![
                re::seq(vec![
                    re::star(re::space()),
                    re::set("_-"),
                    re::star(re::space()),
                ]),
                re::plus(re::space()),
            ]),
        ),
        1,
    )
});

/// (key, separators) of a token; see Python `_shape`.
fn shape(token: &str, spoken: bool) -> (String, Vec<Option<String>>) {
    let lowered = chars(&lower(token));
    let mut words: Vec<String> = Vec::new();
    let mut seps: Vec<Option<String>> = Vec::new();
    let mut last = 0;
    for found in TOKEN_SPLIT.find_iter(&lowered) {
        let piece: String = lowered[last..found.start].iter().collect();
        words.push(piece.trim_end_matches(['.', ',']).to_string());
        let sep = found.text(&lowered);
        let stripped = strip(&sep);
        seps.push(if stripped.is_empty() {
            None
        } else {
            Some(stripped.to_string())
        });
        last = found.end;
    }
    let tail: String = lowered[last..].iter().collect();
    words.push(tail.trim_end_matches(['.', ',']).to_string());
    if spoken {
        let mut i = 1;
        while i + 1 < words.len() {
            if let Some(sep) = joining(&words[i]) {
                words.remove(i);
                // seps[i-1:i+1] = [sep]
                let end = (i + 1).min(seps.len());
                seps.splice(i - 1..end, [Some(sep.to_string())]);
            } else {
                i += 1;
            }
        }
    }
    (words.join(" "), seps)
}

fn candidates(names: &[String]) -> HashMap<String, Vec<String>> {
    let mut lookup: HashMap<String, Vec<String>> = HashMap::new();
    for name in names {
        if !name.is_empty() {
            lookup
                .entry(shape(name, false).0)
                .or_default()
                .push(name.clone());
        }
    }
    lookup
}

fn resolve(token: &str, lookup: &HashMap<String, Vec<String>>) -> Option<String> {
    for spoken in [false, true] {
        let (key, seps) = shape(token, spoken);
        let found = lookup.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        if found.len() == 1 {
            return Some(found[0].clone());
        }
        let matches: Vec<&String> = found
            .iter()
            .filter(|name| {
                let (_, name_seps) = shape(name, false);
                seps.iter()
                    .zip(name_seps.iter())
                    .all(|(s, n)| s.is_none() || s == n)
            })
            .collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
        if !found.is_empty() {
            return None;
        }
    }
    None
}

fn to_owned_names<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    names.iter().map(|n| n.as_ref().to_string()).collect()
}

fn normalize(text: &str, names: &[String], mut render: impl FnMut(&str) -> String) -> String {
    let Some(pattern) = command_pattern(names) else {
        return text.to_string();
    };
    let exact: HashSet<&str> = names.iter().map(String::as_str).collect();
    let lookup = candidates(names);
    pattern.sub(text, |m: &Match, t| {
        let token = m.group(t, 1).unwrap_or_default();
        let canonical = if exact.contains(token.as_str()) {
            Some(token)
        } else {
            resolve(&token, &lookup)
        };
        match canonical {
            Some(name) => render(&name),
            None => m.text(t),
        }
    })
}

/// Snap "/" + spoken command words to registered names.
pub fn normalize_commands<S: AsRef<str>>(text: &str, names: &[S]) -> String {
    normalize(text, &to_owned_names(names), |name| format!("/{name}"))
}

/// Map a matched joining word to its symbol. Python indexes `_JOINING` with
/// `group.lower()`, which raises KeyError for IGNORECASE-only spellings such
/// as "daſh"; here those resolve to the alternative they matched instead.
fn joining_symbol(word: &str) -> &'static str {
    if let Some(sym) = joining(&lower(word)) {
        return sym;
    }
    let folded = chars(word);
    JOINING
        .iter()
        .find(|(k, _)| Regex::new(re::lit_ci(k), 0).fullmatch(&folded))
        .map(|(_, v)| *v)
        .unwrap_or("")
}

/// Convert spoken "slash", "dash", "hyphen", "underscore" to symbols, snapping
/// registered command names after a slash.
pub fn apply_spoken_symbols<S: AsRef<str>>(text: &str, commands: &[S]) -> String {
    let mut text = SLASH_PATTERN.sub(text, |m, t| {
        if m.group(t, 1).unwrap_or_default().is_empty() {
            "/".to_string()
        } else {
            " /".to_string()
        }
    });
    let mut resolved: Vec<String> = Vec::new();
    if !commands.is_empty() {
        text = normalize(&text, &to_owned_names(commands), |name| {
            resolved.push(format!("/{name}"));
            format!("\u{0}{}\u{0}", resolved.len() - 1)
        });
    }
    text = JOIN_PATTERN.sub(&text, |m, t| {
        joining_symbol(&m.group(t, 1).unwrap_or_default()).to_string()
    });
    if !resolved.is_empty() {
        text = restore_placeholders(&PLACEHOLDER, &text, &resolved);
    }
    text
}

/// `^[A-Za-z0-9]+(?:[-_][A-Za-z0-9]+)*$`
fn valid_command_name(name: &str) -> bool {
    // Python `$` also accepts one trailing "\n".
    let name = name.strip_suffix('\n').unwrap_or(name);
    !name.is_empty()
        && name
            .split(['-', '_'])
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Port of `commands.discover_commands`: the sorted, de-duplicated set of
/// valid configured names plus valid, non-hidden subdirectory names of each
/// skills dir. Skills dirs are expanded like Python `Path.expanduser()`;
/// where Python raises for an unresolvable `~`/`~user`, that entry is
/// skipped (it cannot name an existing directory).
pub fn discover_commands<S: AsRef<str>, D: AsRef<str>>(
    names: &[S],
    skills_dirs: &[D],
) -> Vec<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for name in names {
        if valid_command_name(name.as_ref()) {
            found.insert(name.as_ref().to_string());
        }
    }
    for entry in skills_dirs {
        let Ok(root) = crate::home::expand_user(Path::new(entry.as_ref())) else {
            continue;
        };
        if !root.is_dir() {
            continue;
        }
        let Ok(children) = std::fs::read_dir(&root) else {
            continue;
        };
        for child in children.flatten() {
            let name = child.file_name().to_string_lossy().into_owned();
            if child.path().is_dir() && !name.starts_with('.') && valid_command_name(&name) {
                found.insert(name);
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: [&str; 2] = ["skill_part", "candidate-solutioning"];

    #[test]
    fn known_commands() {
        let cases = [
            ("slash skill part", "/skill_part"),
            ("Slash skill part.", "/skill_part"),
            ("slash candidate solutioning", "/candidate-solutioning"),
            ("Slash candidate solutioning.", "/candidate-solutioning"),
            ("/candidate-solutioning", "/candidate-solutioning"),
            ("/skill_part", "/skill_part"),
            (
                "run slash skill part with these arguments",
                "run /skill_part with these arguments",
            ),
            ("slash unknown words", "/unknown words"),
            (
                "we discussed candidate solutioning today",
                "we discussed candidate solutioning today",
            ),
            ("slash skill underscore part", "/skill_part"),
            ("slash candidate dash solutioning", "/candidate-solutioning"),
            ("slash Skill Part, then commit", "/skill_part, then commit"),
            ("slash skill partial", "/skill partial"),
            ("slash skill part two", "/skill_part two"),
            ("config slash settings dash prod", "config /settings-prod"),
        ];
        for (spoken, expected) in cases {
            assert_eq!(
                apply_spoken_symbols(spoken, &REGISTRY),
                expected,
                "{spoken:?}"
            );
        }
    }

    #[test]
    fn no_registry_keeps_legacy_behaviour() {
        let none: [&str; 0] = [];
        assert_eq!(
            apply_spoken_symbols("slash skill part.", &none),
            "/skill part."
        );
        assert_eq!(apply_spoken_symbols("", &none), "");
        assert_eq!(apply_spoken_symbols("warm dash cache", &none), "warm-cache");
        assert_eq!(apply_spoken_symbols("a Hyphen, b", &none), "a-b");
        assert_eq!(apply_spoken_symbols("v underscore 2", &none), "v_2");
        assert_eq!(
            apply_spoken_symbols("dashboard dashes", &none),
            "dashboard dashes"
        );
        assert_eq!(apply_spoken_symbols(" slash home", &none), " /home");
        assert_eq!(apply_spoken_symbols("xslash home", &none), "xslash home");
        // IGNORECASE long-s: Python would raise KeyError here; we map to "-".
        assert_eq!(apply_spoken_symbols("a daſh b", &none), "a-b");
        assert_eq!(apply_spoken_symbols("SLAſH x", &none), "/x");
    }

    #[test]
    fn overlapping_names_prefer_longest() {
        let names = ["skill_part", "skill_part_two"];
        assert_eq!(
            normalize_commands("/skill part two", &names),
            "/skill_part_two"
        );
        assert_eq!(normalize_commands("/skill part", &names), "/skill_part");
    }

    #[test]
    fn punctuation_only_stripped_at_end() {
        assert_eq!(
            normalize_commands("/skill part. Then more", &REGISTRY),
            "/skill_part. Then more"
        );
        assert_eq!(
            normalize_commands("say /skill part.", &REGISTRY),
            "say /skill_part"
        );
    }

    #[test]
    fn path_like_text_untouched() {
        assert_eq!(
            normalize_commands("/usr/skill part", &REGISTRY),
            "/usr/skill part"
        );
        assert_eq!(
            normalize_commands("/skill_part/child", &REGISTRY),
            "/skill_part/child"
        );
        let none: [&str; 0] = [];
        assert_eq!(normalize_commands("/skill part", &none), "/skill part");
    }

    #[test]
    fn lookahead_failure_backtracks_to_shorter_name() {
        let names = ["skill_part", "skill"];
        assert_eq!(normalize_commands("/skill part-x", &names), "/skill part-x");
        assert_eq!(normalize_commands("/skill part", &names), "/skill_part");
        assert_eq!(normalize_commands("/skill-x", &names), "/skill-x");
    }

    #[test]
    fn colliding_names_keep_exact_and_skip_ambiguous_speech() {
        let names = ["skill-part", "skill_part"];
        assert_eq!(apply_spoken_symbols("/skill-part", &names), "/skill-part");
        assert_eq!(apply_spoken_symbols("/skill_part", &names), "/skill_part");
        assert_eq!(
            apply_spoken_symbols("slash skill part", &names),
            "/skill part"
        );
        assert_eq!(
            apply_spoken_symbols("slash skill underscore part", &names),
            "/skill_part"
        );
        assert_eq!(
            apply_spoken_symbols("Slash Skill underscore Part.", &names),
            "/skill_part"
        );
        assert_eq!(
            apply_spoken_symbols("slash skill dash part", &names),
            "/skill-part"
        );
        assert_eq!(apply_spoken_symbols("/Skill-Part", &names), "/skill-part");
    }

    #[test]
    fn symbol_words_inside_registered_names() {
        let names = ["dash", "foo_dash_bar"];
        assert_eq!(apply_spoken_symbols("slash dash", &names), "/dash");
        assert_eq!(
            apply_spoken_symbols("slash foo dash bar.", &names),
            "/foo_dash_bar"
        );
        assert_eq!(
            apply_spoken_symbols("warm dash cache", &names),
            "warm-cache"
        );
    }

    #[test]
    fn discover_commands_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["candidate-solutioning", "skill_part", ".hidden", "bad name"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let missing = dir.path().join("missing").to_string_lossy().into_owned();
        let names = discover_commands(
            &["explicit_one", "not valid", "a__b", "-x"],
            &[root, missing],
        );
        assert_eq!(
            names,
            vec!["candidate-solutioning", "explicit_one", "skill_part"]
        );
        let empty: [&str; 0] = [];
        assert!(discover_commands(&empty, &empty).is_empty());
    }

    /// Configured command names are external input: a name with very many
    /// separators (and the matching dictation) must not overflow the stack.
    #[test]
    fn long_command_names_with_many_separators_do_not_overflow() {
        // The recursive matcher overflowed a 2 MB test thread well below this.
        let parts = 20_000;
        let name = vec!["ab"; parts].join("-");
        let spoken = format!("say slash {} now", vec!["ab"; parts].join(" dash "));
        let names = [name.clone()];
        assert_eq!(
            apply_spoken_symbols(&spoken, &names),
            format!("say /{name} now")
        );
        assert_eq!(
            normalize_commands(&format!("/{name}."), &names),
            format!("/{name}")
        );
        let near_miss = format!("/{}-x", name);
        assert_eq!(normalize_commands(&near_miss, &names), near_miss);
    }

    #[test]
    fn shape_reads_separators() {
        assert_eq!(
            shape("Skill_Part", false),
            ("skill part".into(), vec![Some("_".into())])
        );
        assert_eq!(
            shape("skill underscore part", true),
            ("skill part".into(), vec![Some("_".into())])
        );
        assert_eq!(
            shape("skill part.", false),
            ("skill part".into(), vec![None])
        );
    }
}
