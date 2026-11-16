//! Shared blocking HTTP helpers with `requests`-like error text.

use reqwest::blocking::{Client, RequestBuilder, Response};
use std::time::Duration;

/// A blocking client whose connect and overall timeouts are `secs` (Python
/// passes the same value as the `requests` timeout).
pub(crate) fn client(secs: i64) -> Result<Client, String> {
    if secs <= 0 {
        return Err(format!(
            "Attempted to set timeout to {secs}, but the timeout cannot be set to a value \
             less than or equal to 0."
        ));
    }
    let duration = Duration::from_secs(secs as u64);
    let builder = Client::builder()
        .connect_timeout(duration)
        .timeout(duration);
    // Tests talk to a local mock server; never route them through a proxy
    // that might be configured in the environment.
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|e| error_text(&e))
}

/// Full error chain text (reqwest's top-level message alone hides the cause).
pub(crate) fn error_text(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// `requests.Response.raise_for_status()` text for 4xx/5xx.
pub(crate) fn raise_for_status(response: &Response) -> Result<(), String> {
    let status = response.status();
    let code = status.as_u16();
    let kind = match code {
        400..=499 => "Client",
        500..=599 => "Server",
        _ => return Ok(()),
    };
    let reason = status.canonical_reason().unwrap_or("");
    Err(format!(
        "{code} {kind} Error: {reason} for url: {}",
        response.url()
    ))
}

/// Send, check the status and decode a JSON body.
pub(crate) fn send_json(request: RequestBuilder) -> Result<serde_json::Value, String> {
    let response = request.send().map_err(|e| error_text(&e))?;
    raise_for_status(&response)?;
    let body = response.text().map_err(|e| error_text(&e))?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Send and check the status only (healthchecks).
pub(crate) fn send_ok(request: RequestBuilder) -> Result<(), String> {
    let response = request.send().map_err(|e| error_text(&e))?;
    raise_for_status(&response)
}

/// Python `KeyError` text for a missing mapping key.
pub(crate) fn key_error(key: &str) -> String {
    crate::pyfmt::py_repr(key)
}

/// Python type name of a decoded JSON value.
pub(crate) fn py_type(value: &serde_json::Value) -> &'static str {
    use serde_json::Value;
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `obj.get(key, default)` on a decoded JSON body; non-dicts fail like Python.
pub(crate) fn dict_get<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> Result<Option<&'a serde_json::Value>, String> {
    match value {
        serde_json::Value::Object(map) => Ok(map.get(key)),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            py_type(other)
        )),
    }
}

/// `obj[key]` on a decoded JSON value; non-dicts fail like Python.
pub(crate) fn dict_index<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> Result<&'a serde_json::Value, String> {
    use serde_json::Value;
    match value {
        Value::Object(map) => map.get(key).ok_or_else(|| key_error(key)),
        Value::Array(_) => Err("list indices must be integers or slices, not str".into()),
        Value::String(_) => Err("string indices must be integers, not 'str'".into()),
        other => Err(format!("'{}' object is not subscriptable", py_type(other))),
    }
}

/// Iterate a decoded JSON value the way a Python `for` loop would, for values
/// whose items are then subscripted by a string key. Iterating a non-empty
/// string or dict yields strings, which fail that subscript.
pub(crate) fn iter_items(value: &serde_json::Value) -> Result<Vec<&serde_json::Value>, String> {
    use serde_json::Value;
    match value {
        Value::Array(items) => Ok(items.iter().collect()),
        Value::String(s) if s.is_empty() => Ok(Vec::new()),
        Value::Object(map) if map.is_empty() => Ok(Vec::new()),
        Value::String(_) | Value::Object(_) => {
            Err("string indices must be integers, not 'str'".into())
        }
        other => Err(format!("'{}' object is not iterable", py_type(other))),
    }
}

/// A string field Python would then call a `str` method on.
pub(crate) fn as_str<'a>(value: &'a serde_json::Value, method: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("'{}' object has no attribute '{method}'", py_type(value)))
}
