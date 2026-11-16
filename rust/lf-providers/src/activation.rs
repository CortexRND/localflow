//! Port of `localflow/llm_activation.py`: activation snapshots, the LLM key
//! Ask and cleanup use, bound to one endpoint.
//!
//! Activating a provider never rewrites a shared key or an existing snapshot.
//! It stores a new secret `llm_activation_<provider>_<32 hex>` holding
//! `{"base_url": ..., "key": ...}` and then atomically saves a config whose
//! `[llm] api_key_ref` names it. The key is only ever used for the endpoint it
//! was activated for. `llm_activation_index` lists snapshot names (never
//! values) so removing a provider key revokes every snapshot taken from it.
//!
//! Pure: no keyring, no config. Secret values are never logged.

use crate::pyfmt::{py_json_str, py_strip};
use regex::Regex;
use std::sync::OnceLock;

pub const PREFIX: &str = "llm_activation_";
pub const INDEX_NAME: &str = "llm_activation_index";

fn ref_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\Allm_activation_([a-z][a-z0-9]{0,31})_([0-9a-f]{32})\z").unwrap()
    })
}

/// True for a well-formed snapshot name in our own namespace.
pub fn is_ref(value: &str) -> bool {
    ref_pattern().is_match(value)
}

/// The provider preset id a snapshot was taken from.
pub fn ref_provider(value: &str) -> Option<&str> {
    ref_pattern()
        .captures(value)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
}

pub fn new_ref(provider_id: &str) -> anyhow::Result<String> {
    let reference = format!("{PREFIX}{provider_id}_{}", uuid::Uuid::new_v4().simple());
    if !is_ref(&reference) {
        anyhow::bail!("Unsupported provider id for an activation");
    }
    Ok(reference)
}

/// `url.strip().rstrip("/")`.
pub fn endpoint(url: &str) -> String {
    py_strip(url).trim_end_matches('/').to_string()
}

/// Byte-identical to Python `json.dumps({"base_url": endpoint(url), "key": key},
/// separators=(",", ":"))`.
pub fn encode(base_url: &str, key: &str) -> String {
    format!(
        "{{\"base_url\":{},\"key\":{}}}",
        py_json_str(&endpoint(base_url)),
        py_json_str(key)
    )
}

/// (endpoint, key) from a stored snapshot, or None when malformed/empty.
pub fn decode(value: &str) -> Option<(String, String)> {
    if value.is_empty() {
        return None;
    }
    let data: serde_json::Value = serde_json::from_str(value).ok()?;
    let object = data.as_object()?;
    let url = object.get("base_url")?.as_str()?;
    let key = object.get("key")?.as_str()?;
    if url.is_empty() || key.is_empty() {
        return None;
    }
    Some((url.to_string(), key.to_string()))
}

/// The snapshot key named by `reference`, only for the endpoint it is bound
/// to. Anything else (bad name, missing or malformed snapshot, another
/// endpoint) yields "" — never a fallback to some other key.
pub fn resolve(reference: &str, base_url: &str, get_secret: impl Fn(&str) -> String) -> String {
    if !is_ref(reference) {
        log::warn!("ignoring an invalid llm.api_key_ref; activate the provider again");
        return String::new();
    }
    let Some((bound_url, key)) = decode(&get_secret(reference)) else {
        log::warn!("the activated provider key is missing; activate the provider again");
        return String::new();
    };
    if bound_url != endpoint(base_url) {
        log::warn!(
            "the activated provider key belongs to another endpoint; activate the provider again"
        );
        return String::new();
    }
    key
}

fn dedupe_refs<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if is_ref(name) && !out.iter().any(|seen| seen == name) {
            out.push(name.to_string());
        }
    }
    out
}

pub fn read_index(get_secret: impl Fn(&str) -> String) -> Vec<String> {
    let raw = get_secret(INDEX_NAME);
    if raw.is_empty() {
        return Vec::new();
    }
    let names: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => {
            log::warn!("ignoring a malformed provider activation index");
            return Vec::new();
        }
    };
    match names.as_array() {
        Some(items) => dedupe_refs(items.iter().filter_map(|item| item.as_str())),
        None => Vec::new(),
    }
}

/// Byte-identical to Python `json.dumps([...])` (", " separator).
pub fn encode_index<'a>(refs: impl IntoIterator<Item = &'a str>) -> String {
    let items: Vec<String> = dedupe_refs(refs).iter().map(|r| py_json_str(r)).collect();
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF: &str = "llm_activation_groq_0123456789abcdef0123456789abcdef";

    #[test]
    fn ref_names() {
        assert!(is_ref(REF));
        assert_eq!(ref_provider(REF), Some("groq"));
        for bad in [
            "",
            "llm_api_key",
            "llm_activation_invalid",
            "llm_activation_Groq_0123456789abcdef0123456789abcdef",
            "llm_activation_groq_0123456789ABCDEF0123456789abcdef",
            "llm_activation_groq_0123456789abcdef0123456789abcde",
            "llm_activation_1groq_0123456789abcdef0123456789abcdef",
            "xllm_activation_groq_0123456789abcdef0123456789abcdef",
            "llm_activation_groq_0123456789abcdef0123456789abcdef\n",
            "llm_activation_abcdefghijklmnopqrstuvwxyz0123456_0123456789abcdef0123456789abcdef",
        ] {
            assert!(!is_ref(bad), "{bad:?}");
            assert_eq!(ref_provider(bad), None);
        }
        assert!(is_ref(
            "llm_activation_abcdefghijklmnopqrstuvwxyz012345_0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn new_refs_are_unique_and_validated() {
        let a = new_ref("openai").unwrap();
        let b = new_ref("openai").unwrap();
        assert!(is_ref(&a) && is_ref(&b) && a != b);
        assert_eq!(ref_provider(&a), Some("openai"));
        assert_eq!(
            new_ref("Bad_id").unwrap_err().to_string(),
            "Unsupported provider id for an activation"
        );
        assert!(new_ref("").is_err());
    }

    #[test]
    fn encode_matches_python_bytes_and_round_trips() {
        assert_eq!(
            encode(" https://api.groq.com/openai/v1/ ", "gsk_x\"\u{e9}"),
            concat!(
                r#"{"base_url":"https://api.groq.com/openai/v1","key":"gsk_x\""#,
                "\\",
                "u00e9\"}"
            )
        );
        assert_eq!(
            decode(&encode("https://h/v1", "k")),
            Some(("https://h/v1".into(), "k".into()))
        );
        assert_eq!(endpoint("  http://x//  "), "http://x");
        assert_eq!(endpoint(""), "");
    }

    #[test]
    fn decode_rejects_malformed() {
        for raw in [
            "",
            "nope",
            "[]",
            "null",
            r#"{"base_url":"u"}"#,
            r#"{"key":"k"}"#,
            r#"{"base_url":"","key":"k"}"#,
            r#"{"base_url":"u","key":""}"#,
            r#"{"base_url":1,"key":"k"}"#,
        ] {
            assert_eq!(decode(raw), None, "{raw}");
        }
    }

    #[test]
    fn resolve_is_endpoint_bound_and_fails_closed() {
        let stored = encode("https://api.groq.com/openai/v1", "KEY");
        let get = |name: &str| {
            if name == REF {
                stored.clone()
            } else {
                "OTHER".to_string()
            }
        };
        assert_eq!(resolve(REF, "https://api.groq.com/openai/v1/", get), "KEY");
        assert_eq!(resolve(REF, " https://api.groq.com/openai/v1 ", get), "KEY");
        assert_eq!(resolve(REF, "https://api.openai.com/v1", get), "");
        assert_eq!(
            resolve("llm_api_key", "https://api.groq.com/openai/v1", get),
            ""
        );
        assert_eq!(
            resolve(
                "llm_activation_invalid",
                "https://api.groq.com/openai/v1",
                get
            ),
            ""
        );
        assert_eq!(
            resolve(REF, "https://api.groq.com/openai/v1", |_| String::new()),
            ""
        );
        assert_eq!(
            resolve(REF, "https://api.groq.com/openai/v1", |_| "{}".into()),
            ""
        );
    }

    #[test]
    fn index_read_and_encode() {
        let other = "llm_activation_openai_ffffffffffffffffffffffffffffffff";
        let raw = format!(r#"["{REF}", 5, null, "bad", "{REF}", "{other}"]"#);
        assert_eq!(
            read_index(|_| raw.clone()),
            vec![REF.to_string(), other.to_string()]
        );
        for raw in [
            "[null, {}, [], 5]",
            "null",
            "{}",
            "[]",
            "",
            "not json",
            "\"x\"",
        ] {
            assert!(read_index(|_| raw.to_string()).is_empty(), "{raw}");
        }
        assert_eq!(
            encode_index([REF, "bad", REF, other]),
            format!(r#"["{REF}", "{other}"]"#)
        );
        assert_eq!(encode_index(std::iter::empty::<&str>()), "[]");
    }
}
