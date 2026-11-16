//! Text formatting that reproduces what the Python `lf` prints.
//!
//! The Python CLI renders values with `str()`, `repr()` and `json.dumps()`;
//! these helpers produce the same text for the JSON values config and status
//! carry, so scripts reading either CLI see identical output.

use serde_json::Value;

/// Python `str.isspace()` (what `str.strip()` removes): Unicode White_Space
/// plus the ASCII information separators U+001C..U+001F.
pub fn py_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python `str.strip()` without arguments.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_space)
}

/// Python `repr(float)`: shortest round-trip digits, scientific notation when
/// the decimal exponent is below -4 or at least 16, `inf`/`nan` spelled out.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    // `{:e}` yields the shortest round-trip digits, e.g. "1.5e-5" or "-5e1".
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("`{:e}` always contains an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if negative { "-" } else { "" };
    if !(-4..16).contains(&exponent) {
        let mut text = String::from(&digits[..1]);
        if digits.len() > 1 {
            text.push('.');
            text.push_str(&digits[1..]);
        }
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        return format!("{sign}{text}e{exp_sign}{:02}", exponent.abs());
    }
    let point = exponent + 1; // digits before the decimal point
    let body = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
    } else {
        let (whole, fraction) = digits.split_at(point as usize);
        format!("{whole}.{fraction}")
    };
    format!("{sign}{body}")
}

/// Python `repr(str)`: single quotes unless the text contains a single quote
/// and no double quote; backslash, quote and control characters escaped.
pub fn str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if (0x80..0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn number_text(number: &serde_json::Number) -> String {
    if let Some(i) = number.as_i64() {
        i.to_string()
    } else if let Some(u) = number.as_u64() {
        u.to_string()
    } else {
        float_repr(number.as_f64().unwrap_or(f64::NAN))
    }
}

/// Python `repr()` of a JSON-shaped value (as inside a list or dict).
pub fn value_repr(value: &Value) -> String {
    match value {
        Value::String(text) => str_repr(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(value_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, item)| format!("{}: {}", str_repr(key), value_repr(item)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        other => value_str(other),
    }
}

/// Python `str()` of a JSON-shaped value.
pub fn value_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number_text(number),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => value_repr(value),
    }
}

fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_into(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            if number.is_f64() {
                let f = number.as_f64().unwrap_or(f64::NAN);
                if f.is_nan() {
                    out.push_str("NaN");
                } else if f.is_infinite() {
                    out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
                } else {
                    out.push_str(&float_repr(f));
                }
            } else {
                out.push_str(&number_text(number));
            }
        }
        Value::String(text) => json_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                json_into(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                json_string(key, out);
                out.push_str(": ");
                json_into(item, out);
            }
            out.push('}');
        }
    }
}

/// Python `json.dumps(value)` with its defaults (`ensure_ascii=True`,
/// `", "`/`": "` separators). Object keys keep their insertion order when
/// serde_json's `preserve_order` is enabled, sorted order otherwise.
pub fn json_dumps(value: &Value) -> String {
    let mut out = String::new();
    json_into(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (-50.0, "-50.0"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1.5e-5, "1.5e-05"),
            (0.0001, "0.0001"),
            (123456789012345.6, "123456789012345.6"),
            (1e15, "1000000000000000.0"),
            (-0.0, "-0.0"),
            (2.5e100, "2.5e+100"),
        ];
        for (value, expected) in cases {
            assert_eq!(float_repr(value), expected, "{value}");
        }
        assert_eq!(float_repr(f64::INFINITY), "inf");
        assert_eq!(float_repr(f64::NAN), "nan");
    }

    #[test]
    fn str_repr_matches_python_quoting() {
        assert_eq!(str_repr("abc"), "'abc'");
        assert_eq!(str_repr("it's"), "\"it's\"");
        assert_eq!(str_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(str_repr("tab\there\n"), "'tab\\there\\n'");
        assert_eq!(str_repr(""), "''");
        assert_eq!(str_repr("é"), "'é'");
    }

    #[test]
    fn value_str_matches_python_str() {
        assert_eq!(value_str(&json!(null)), "None");
        assert_eq!(value_str(&json!(true)), "True");
        assert_eq!(value_str(&json!(8756)), "8756");
        assert_eq!(value_str(&json!("x")), "x");
        assert_eq!(value_str(&json!(["a", 1])), "['a', 1]");
    }

    #[test]
    fn json_dumps_matches_python_defaults() {
        assert_eq!(json_dumps(&json!(["a", "b"])), "[\"a\", \"b\"]");
        assert_eq!(json_dumps(&json!([])), "[]");
        assert_eq!(json_dumps(&json!(true)), "true");
        assert_eq!(
            json_dumps(&json!("café 😀")),
            "\"caf\\u00e9 \\ud83d\\ude00\""
        );
        assert_eq!(
            json_dumps(&json!({"a": null, "b": 1.0})),
            "{\"a\": null, \"b\": 1.0}"
        );
    }
}
