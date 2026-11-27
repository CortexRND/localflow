//! The launcher's pre-launch secret check (design section 8, "Secrets").
//!
//! GPUI-free. Uses the same credential-like span classes LocalFlow already
//! masks in Ask's tool cards and provider prose
//! (`assistant::secret_patterns`): `name=value` pairs for credential-like
//! names, `Bearer …`, provider-style prefixes (`sk-…`, `ghp_…`, …) and
//! long mixed letter/digit tokens. A finding carries only its kind and its
//! line: the matched value is never copied out of the task, displayed or
//! logged. Redaction replaces each whole value with `[redacted]`.
//!
//! One deviation from the masking classes: a full commit id (exactly 40
//! hex digits) isn't a finding here, so a task naming a commit isn't
//! blocked, unless a credential word (`key`, `token`, `secret`, …) sits
//! just before it. Other all-hex tokens (32 or 64 digits are common API
//! key and private key lengths) are findings. Every one is still masked
//! wherever output is shown.

use std::ops::Range;

use crate::controllers::assistant::{
    assignment_value, mixed_token, prose_quoted_value, secret_patterns,
};

pub const REDACTED: &str = "[redacted]";

/// One credential-like value: what it looks like and where. Never the
/// value itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// "an API key", "a password", "a bearer token", …
    pub kind: &'static str,
    /// 1-based line of the task.
    pub line: usize,
    span: Range<usize>,
}

fn kind_for_name(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    if name.contains("pass") {
        "a password"
    } else if name.contains("token") {
        "an access token"
    } else if name.contains("private") {
        "a private key"
    } else if name.contains("secret") {
        "a client secret"
    } else {
        "an API key"
    }
}

/// Words that make a hex token beside them a credential, not a commit.
const CREDENTIAL_WORDS: [&str; 6] = ["key", "token", "secret", "pass", "private", "auth"];

/// A full commit id: exactly 40 hex digits, with no credential word in the
/// few characters before it on its line.
fn commit_id(text: &str, span: &Range<usize>) -> bool {
    let token = &text[span.clone()];
    if token.len() != 40 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let line_start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
    let mut from = span.start.saturating_sub(24).max(line_start);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let before = text[from..span.start].to_ascii_lowercase();
    !CREDENTIAL_WORDS.iter().any(|w| before.contains(w))
}

/// Every credential-like value in `text`, in order, non-overlapping.
pub fn scan(text: &str) -> Vec<Finding> {
    let [assignment, bearer, prefixed, long] = secret_patterns();
    let mut found: Vec<(Range<usize>, &'static str)> = Vec::new();
    for c in assignment.captures_iter(text) {
        if let Some(name) = c.get(1) {
            if prose_quoted_value(text, &c) {
                continue;
            }
            // A quoted value is one finding, spaces and all.
            let value = assignment_value(&c);
            found.push((value.range(), kind_for_name(name.as_str())));
        }
    }
    for c in bearer.captures_iter(text) {
        if let Some(value) = c.get(2) {
            found.push((value.range(), "a bearer token"));
        }
    }
    for m in prefixed.find_iter(text) {
        found.push((m.range(), "an API key"));
    }
    for m in long.find_iter(text) {
        if mixed_token(m.as_str()) && !commit_id(text, &m.range()) {
            found.push((m.range(), "a key or token"));
        }
    }
    // A value already redacted is not a finding. The unquoted pattern
    // stops before the marker's closing bracket; a longer quoted value
    // that merely starts with the marker is still a finding.
    found.retain(|(r, _)| {
        let value = &text[r.clone()];
        value != REDACTED
            && !(value == &REDACTED[..REDACTED.len() - 1]
                && text[r.end..].starts_with(']')
                && match text[r.end + 1..].chars().next() {
                    None => true,
                    Some(ch) => ch.is_whitespace() || matches!(ch, ',' | ';' | '}' | '"' | '\''),
                })
    });
    // Earliest first; on overlap the first (most specific class) wins and
    // the span grows to cover both, so redaction leaves no fragment.
    found.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
    let mut merged: Vec<(Range<usize>, &'static str)> = Vec::new();
    for (range, kind) in found {
        match merged.last_mut() {
            Some((last, _)) if range.start < last.end => last.end = last.end.max(range.end),
            _ => merged.push((range, kind)),
        }
    }
    merged
        .into_iter()
        .map(|(span, kind)| Finding {
            kind,
            line: text[..span.start].matches('\n').count() + 1,
            span,
        })
        .collect()
}

/// `text` with every finding replaced by [`REDACTED`].
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for finding in scan(text) {
        out.push_str(&text[at..finding.span.start]);
        out.push_str(REDACTED);
        at = finding.span.end;
    }
    out.push_str(&text[at..]);
    out
}

/// The blocking banner: `(title, body)` (design section 10, "Possible
/// secret"). `source` is "your task".
pub fn warning(findings: &[Finding], source: &str) -> Option<(String, String)> {
    let first = findings.first()?;
    let title = format!("Possible secret in {source}");
    let body = if findings.len() == 1 {
        format!(
            "One value on line {} looks like {}. It stays hidden here and is never written to logs or history.",
            first.line, first.kind
        )
    } else {
        format!(
            "{} values look like keys or passwords, the first on line {} ({}). They stay hidden here and are never written to logs or history.",
            findings.len(),
            first.line,
            first.kind.trim_start_matches("an ").trim_start_matches("a ")
        )
    };
    Some((title, body))
}

/// The "Redact and launch" row's supporting line.
pub fn redact_detail(provider: &str, count: usize) -> String {
    if count == 1 {
        format!("{provider} receives {REDACTED} in place of that value")
    } else {
        format!("{provider} receives {REDACTED} in place of those values")
    }
}

/// `(title, body, verb)` of "Send unchanged…"'s confirmation.
pub fn send_unchanged_confirm(
    findings: &[Finding],
    source: &str,
    provider: &str,
) -> (String, String, String) {
    let kind = findings.first().map(|f| f.kind).unwrap_or("a key");
    (
        "Send without redacting?".into(),
        format!(
            "The {} contains a value that looks like {kind}. {provider} will receive it as is.",
            source.trim_start_matches("your ")
        ),
        "Send unchanged".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-live-4f9QxZ2mT8vB7nR1kL0pY3wE";

    /// Words of the quoted fixture passphrase (never printed on failure).
    const PHRASE: [&str; 4] = ["correct", "horse", "battery", "staple"];

    /// A quoted value with spaces is one credential: Redact and launch
    /// leaves no word of it, in double or single quotes, `=` or `:`.
    #[test]
    fn a_quoted_value_with_spaces_is_one_finding() {
        for task in [
            "Log in with password=\"correct horse battery staple\" then deploy",
            "Use api_key: 'correct horse battery staple' for staging",
            "{\"secret\": \"correct horse battery staple\", \"user\": \"bob\"}",
            "Set PASSWORD = \"correct horse battery staple\"\nand continue",
        ] {
            let findings = scan(task);
            assert_eq!(findings.len(), 1, "one finding per quoted value");
            let redacted = redact(task);
            for word in PHRASE {
                assert!(!redacted.contains(word), "a word survived redaction");
            }
            assert!(redacted.contains(REDACTED));
            let masked = crate::controllers::assistant::mask_secrets(task);
            for word in &PHRASE[..3] {
                assert!(!masked.contains(word), "a word survived masking");
            }
            assert!(!masked.contains("staple"), "the last word survived masking");
        }
        // Quotes around the value stay; the rest of the line is untouched.
        // (`assert!`, not `assert_eq!`: a failure never prints the value.)
        assert!(
            redact("password=\"correct horse battery staple\" then deploy")
                == "password=\"[redacted]\" then deploy",
            "the quoted value is replaced whole, quotes kept"
        );
        assert!(
            redact("{\"secret\": \"correct horse battery staple\", \"user\": \"bob\"}")
                == "{\"secret\": \"[redacted]\", \"user\": \"bob\"}",
            "JSON-ish text keeps its shape"
        );
    }

    /// Unquoted and unclosed values still end at the first space.
    #[test]
    fn unquoted_and_unclosed_values_keep_their_bounds() {
        assert!(
            redact("password=hunter2hunter2 for staging") == "password=[redacted] for staging",
            "an unquoted value ends at the space"
        );
        assert!(
            redact("password=\"hunter2hunter2 for staging") == "password=\"[redacted] for staging",
            "an unclosed quote is an unquoted value"
        );
        assert!(scan("Rename the password field to passphrase").is_empty());
    }

    /// The marker only suppresses a value that is exactly the marker, not
    /// a longer quoted value that happens to start with it.
    #[test]
    fn the_redaction_marker_does_not_hide_a_longer_value() {
        let task = "Use secret=\"[redacted] password=abc horse\" here";
        assert_eq!(scan(task).len(), 1, "the longer quoted value is a finding");
        let redacted = redact(task);
        assert!(
            !redacted.contains("abc"),
            "the nested assignment is redacted"
        );
        assert!(
            !redacted.contains("horse"),
            "the whole quoted value is redacted"
        );
        assert!(scan(&redacted).is_empty(), "the redacted task passes");
        let glued = "Use password=[redacted]abc123456789abcdef here";
        assert!(
            !scan(glued).is_empty(),
            "a suffix glued to the marker still scans"
        );
    }

    /// Quoted prose such as `'password=' exactly as 'password equals'` is
    /// not a credential value, so keeping a draft does not cut the words
    /// between the quotes. Properly quoted JSON/YAML keys still scan.
    #[test]
    fn quoted_prose_is_not_a_quoted_value() {
        let prose = "Write 'password=' exactly as 'password equals' in the docs";
        assert!(scan(prose).is_empty(), "quoted prose is not a finding");
        assert!(redact(prose) == prose, "the draft text is unchanged");
        assert!(
            crate::controllers::assistant::mask_secrets(prose) == prose,
            "display masking is unchanged"
        );
        let jsonish = "{\"password\": \"correct horse battery staple\"}";
        assert_eq!(scan(jsonish).len(), 1, "a properly quoted key still scans");
    }

        let quoted = "Use 'password=\"correct horse battery staple\"' exactly";
        assert_eq!(scan(quoted).len(), 1, "a quoted assignment still scans");
    }

    #[test]
    fn findings_give_kind_and_line_but_never_the_value() {
        let task = format!("Rotate the webhook key.\nUse api_key={KEY}\nthen redeploy.");
        let found = scan(&task);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].kind, found[0].line), ("an API key", 2));
        let shown = format!("{found:?}");
        assert!(!shown.contains("4f9QxZ2m"), "the shown finding is redacted");
        let (title, body) = warning(&found, "your task").unwrap();
        assert_eq!(title, "Possible secret in your task");
        assert_eq!(
            body,
            "One value on line 2 looks like an API key. It stays hidden here and is never written to logs or history."
        );
        assert!(!title.contains("4f9Q") && !body.contains("4f9Q"));
    }

    #[test]
    fn every_class_is_found_and_redacted_completely() {
        let task = format!(
            "password: hunter2hunter2\nAuthorization: Bearer abcdefghijklmnop1234\n\
             token ghp_abcdefghijklmnopqrstuvwxyz0123 and {KEY}\nlong Zx9Zx9Zx9Zx9Zx9Zx9Zx9Zx9Zx9"
        );
        let found = scan(&task);
        let kinds: Vec<&str> = found.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                "a password",
                "a bearer token",
                "an API key",
                "an API key",
                "a key or token"
            ]
        );
        let redacted = redact(&task);
        for secret in [
            "hunter2hunter2",
            "abcdefghijklmnop1234",
            "ghp_abc",
            "4f9QxZ2m",
            "Zx9Zx9",
        ] {
            assert!(
                !redacted.contains(secret),
                "a fixture secret survived redaction"
            );
        }
        assert!(redacted.starts_with("password: [redacted]\n"));
        assert!(scan(&redacted).is_empty(), "a redacted task passes");
    }

    #[test]
    fn ordinary_tasks_and_commit_ids_pass() {
        for task in [
            "Add retry backoff to the webhook sender and open a PR",
            "Revert 3f9a2c1b7e4d5f6a8b9c0d1e2f3a4b5c6d7e8f90 and rerun CI",
            "Rename the token field in the schema",
            "",
        ] {
            assert!(scan(task).is_empty(), "ordinary text has no finding");
            assert_eq!(redact(task), task);
        }
    }

    /// Review M6: only a 40-digit commit id passes, and not beside a
    /// credential word; 32- and 64-digit hex keys are findings.
    #[test]
    fn hex_keys_are_findings_but_a_commit_id_is_not() {
        let commit = "3f9a2c1b7e4d5f6a8b9c0d1e2f3a4b5c6d7e8f90";
        let hex32 = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";
        let hex64 = format!("{commit}0a1b2c3d4e5f607182930a1b");
        assert_eq!(hex64.len(), 64);
        for task in [
            format!("Use {hex32} to call the API"),
            format!("Sign with {hex64} please"),
            format!("The deploy key {commit} must rotate"),
            format!("token: {commit}"),
        ] {
            assert_eq!(scan(&task).len(), 1, "the token is one finding");
            let redacted = redact(&task);
            assert!(
                !redacted.contains(&hex32[..12]),
                "the hex token is redacted"
            );
            assert!(
                !redacted.contains(&commit[..12]),
                "the commit-shaped token is redacted"
            );
        }
        assert!(scan(&format!("Cherry-pick {commit} onto release")).is_empty());
    }

    #[test]
    fn copy_matches_the_design() {
        let found = scan(&format!("a {KEY}\nb password=abcdefgh123"));
        assert_eq!(
            warning(&found, "your task").unwrap().1,
            "2 values look like keys or passwords, the first on line 1 (API key). They stay hidden here and are never written to logs or history."
        );
        assert_eq!(
            redact_detail("Claude", 1),
            "Claude receives [redacted] in place of that value"
        );
        let (title, body, verb) = send_unchanged_confirm(&found[..1], "your task", "Claude");
        assert_eq!(title, "Send without redacting?");
        assert_eq!(
            body,
            "The task contains a value that looks like an API key. Claude will receive it as is."
        );
        assert_eq!(verb, "Send unchanged");
        assert!(warning(&[], "your task").is_none());
    }
}
