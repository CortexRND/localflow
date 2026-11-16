use lf_server::backend::{BackendRequest, BackendResponse};
use serde_json::{json, Value};

pub(crate) type ResponseResult<T> = Result<T, BackendResponse>;
pub(crate) struct Body {
    value: Value,
    errors: Vec<Value>,
}
impl Body {
    pub fn parse(request: &BackendRequest) -> ResponseResult<Self> {
        let bytes = &request.body;
        if bytes.is_empty() {
            return Err(invalid(
                json!([{"type":"missing","loc":["body"],"msg":"Field required","input":null}]),
            ));
        }
        if let Some(content_type) = request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            let mime = content_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if mime != "application/json"
                && !(mime.starts_with("application/") && mime.ends_with("+json"))
            {
                return Err(invalid(
                    json!([{"type":"model_attributes_type","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","input":String::from_utf8_lossy(bytes)}]),
                ));
            }
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid(json!([{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error","input":{},"ctx":{"error":"Invalid JSON"}}])))?;
        if value.is_null() {
            return Err(invalid(
                json!([{"type":"missing","loc":["body"],"msg":"Field required","input":null}]),
            ));
        }
        if !value.is_object() {
            return Err(invalid(
                json!([{"type":"model_attributes_type","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","input":value}]),
            ));
        }
        Ok(Self {
            value,
            errors: vec![],
        })
    }
    fn error(&mut self, key: &str, kind: &str, message: &str, value: Value) {
        self.errors
            .push(json!({"type":kind,"loc":["body",key],"msg":message,"input":value}));
    }
    pub fn string(&mut self, key: &str, default: Option<&str>, maximum: Option<usize>) -> String {
        let Some(value) = self.value.get(key).cloned() else {
            if let Some(default) = default {
                return default.into();
            }
            self.error(key, "missing", "Field required", self.value.clone());
            return String::new();
        };
        let Some(text) = value.as_str() else {
            self.error(key, "string_type", "Input should be a valid string", value);
            return String::new();
        };
        if let Some(maximum) = maximum.filter(|&maximum| text.chars().count() > maximum) {
            self.errors.push(json!({"type":"string_too_long","loc":["body",key],"msg":format!("String should have at most {maximum} characters"),"input":value,"ctx":{"max_length":maximum}}));
        }
        text.into()
    }
    pub fn optional_string(&mut self, key: &str) -> Option<String> {
        if self.value.get(key).is_none_or(Value::is_null) {
            None
        } else {
            Some(self.string(key, None, None))
        }
    }
    pub fn boolean(&mut self, key: &str, default: Option<bool>) -> Option<bool> {
        let Some(value) = self.value.get(key).cloned() else {
            return default;
        };
        if value.is_null() && default.is_none() {
            return None;
        }
        let parsed = match &value {
            Value::Bool(value) => Some(*value),
            Value::Number(value) if value.as_f64() == Some(0.0) => Some(false),
            Value::Number(value) if value.as_f64() == Some(1.0) => Some(true),
            Value::String(text) => match text.to_lowercase().as_str() {
                "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
                "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
                _ => None,
            },
            _ => None,
        };
        if parsed.is_none() {
            let parsing = value.is_string() || value.is_number();
            self.error(
                key,
                if parsing { "bool_parsing" } else { "bool_type" },
                if parsing {
                    "Input should be a valid boolean, unable to interpret input"
                } else {
                    "Input should be a valid boolean"
                },
                value,
            );
        }
        parsed
    }
    pub fn integer(&mut self, key: &str, default: i64) -> i64 {
        let Some(value) = self.value.get(key).cloned() else {
            return default;
        };
        let number = match &value {
            Value::Bool(v) => Some(i64::from(*v)),
            Value::Number(v) => v
                .as_i64()
                .or_else(|| v.as_u64().map(|n| n.min(i64::MAX as u64) as i64))
                .or_else(|| v.as_f64().filter(|n| n.fract() == 0.0).map(|n| n as i64)),
            Value::String(v) => integer_string(v),
            _ => None,
        };
        if let Some(number) = number {
            return number;
        }
        let (kind, message) = if value.is_string() {
            (
                "int_parsing",
                "Input should be a valid integer, unable to parse string as an integer",
            )
        } else if value.is_number() {
            (
                "int_from_float",
                "Input should be a valid integer, got a number with a fractional part",
            )
        } else {
            ("int_type", "Input should be a valid integer")
        };
        self.error(key, kind, message, value);
        default
    }
    pub fn finish(self) -> ResponseResult<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(invalid(json!(self.errors)))
        }
    }
}
fn invalid(detail: Value) -> BackendResponse {
    BackendResponse {
        status: 422,
        body: json!({"detail":detail}),
    }
}
fn integer_string(text: &str) -> Option<i64> {
    let text = text.trim();
    let head = if let Some((head, fraction)) = text.split_once('.') {
        if fraction.is_empty() || !fraction.bytes().all(|c| c == b'0') {
            return None;
        }
        head
    } else {
        text
    };
    let digits = head.strip_prefix(['+', '-']).unwrap_or(head).as_bytes();
    if digits.is_empty()
        || digits.len() > 4300
        || !digits.iter().enumerate().all(|(i, c)| {
            c.is_ascii_digit()
                || *c == b'_'
                    && i > 0
                    && i + 1 < digits.len()
                    && digits[i - 1].is_ascii_digit()
                    && digits[i + 1].is_ascii_digit()
        })
    {
        return None;
    }
    Some(
        head.replace('_', "")
            .parse()
            .unwrap_or(if head.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            }),
    )
}
