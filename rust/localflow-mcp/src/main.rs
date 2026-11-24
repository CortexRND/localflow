//! Read-only LocalFlow MCP tools over newline-delimited JSON-RPC on stdio.
mod dictations;
mod prompts;
mod sources;

use std::{
    io::{self, BufRead, Write},
    sync::LazyLock,
};

use lf_core::{
    history::KINDS,
    symbols::pycompat::{float_repr, repr_str, truthy},
};
use serde_json::{json, Map, Value};

use sources::Sources;

const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const INSTRUCTIONS: &str =
    "Search the user's past LocalFlow dictations, spoken commands and meeting work prompts.\n\
     This is the user's private local history; quote it only as needed for the task.";
const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 50;
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({"type": "object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema["additionalProperties"] = json!(false);
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": {"readOnlyHint": true, "openWorldHint": false},
    })
}

static TOOLS: LazyLock<[Value; 4]> = LazyLock::new(|| {
    let limit = json!({
        "type": "integer",
        "description": format!("Maximum results (1-{MAX_LIMIT}, default {DEFAULT_LIMIT}).")
    });
    let query = json!({"type": "string", "description": "Words to find; omit to list recent."});
    let date_hint = "today, yesterday, 7d, 30d or YYYY-MM-DD";
    [
        tool(
            "search_dictations",
            "Search or list the user's past LocalFlow dictations, spoken commands and \
             transforms. Without a query, returns the newest first. Quote a phrase to \
             boost exact matches.",
            json!({
                "query": query,
                "since": {"type": "string", "description": format!("Start: {date_hint}.")},
                "until": {"type": "string", "description": format!("End: {date_hint} (a named day is inclusive).")},
                "app": {"type": "string", "description": "App name substring (case-insensitive) or exact bundle id."},
                "kind": {"type": "string", "enum": KINDS},
                "limit": limit,
            }),
            &[],
        ),
        tool(
            "get_dictation",
            "Full text of one dictation by id (from search_dictations).",
            json!({"id": {"type": "integer"}}),
            &["id"],
        ),
        tool(
            "search_prompts",
            "Search or list meeting work prompts (title, meeting and prompt body). \
             Without a query, returns the newest first.",
            json!({
                "query": query,
                "status": {"type": "string", "enum": prompts::STATUSES},
                "limit": limit,
            }),
            &[],
        ),
        tool(
            "get_prompt",
            "Full body and status of one work prompt by id (from search_prompts).",
            json!({"id": {"type": "string"}}),
            &["id"],
        ),
    ]
});

fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    if !args.is_object() && !args.is_null() {
        return Err("arguments must be an object".into());
    }
    let props = &schema["properties"];
    let required = schema["required"].as_array().map_or(&[][..], Vec::as_slice);
    if let Some(args) = args.as_object() {
        let mut unknown: Vec<_> = args
            .keys()
            .filter(|key| props.get(*key).is_none())
            .cloned()
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            return Err(format!("unknown argument(s): {}", unknown.join(", ")));
        }
    }
    let missing: Vec<_> = required
        .iter()
        .filter_map(Value::as_str)
        .filter(|key| args.get(*key).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing argument(s): {}", missing.join(", ")));
    }
    for (key, value) in args.as_object().into_iter().flatten() {
        if value.is_null() && !required.iter().any(|spec| spec == key.as_str()) {
            continue;
        }
        let spec = &props[key];
        let kind = spec["type"].as_str().unwrap_or("");
        if !(match kind {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            _ => false,
        }) {
            return Err(format!("{key} must be a {kind}"));
        }
        if let Some(allowed) = spec["enum"]
            .as_array()
            .filter(|choices| !choices.contains(value))
        {
            let choices: Vec<_> = allowed.iter().filter_map(Value::as_str).collect();
            return Err(format!("{key} must be one of {}", choices.join(", ")));
        }
    }
    Ok(())
}

fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(value) => if *value { "True" } else { "False" }.into(),
        Value::String(value) => repr_str(value),
        Value::Number(value) if value.is_f64() => float_repr(value.as_f64().unwrap()),
        Value::Number(value) => value.to_string(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn call_tool(src: &Sources, params: &Map<String, Value>) -> Result<Value, String> {
    let name = params.get("name").unwrap_or(&Value::Null);
    let Some(tool) = TOOLS.iter().find(|t| &t["name"] == name) else {
        return Err(format!("unknown tool: {}", repr(name)));
    };
    let args = params.get("arguments").unwrap_or(&Value::Null);
    validate(&tool["inputSchema"], args)?;
    let query = args["query"].as_str().unwrap_or("");
    let limit = args["limit"]
        .as_i64()
        .unwrap_or(if args["limit"].is_u64() {
            MAX_LIMIT
        } else {
            DEFAULT_LIMIT
        })
        .clamp(1, MAX_LIMIT) as usize;
    let value = match name.as_str().expect("registered tool name") {
        "search_dictations" => dictations::search(
            &src.history,
            query,
            args["since"].as_str(),
            args["until"].as_str(),
            args["app"].as_str(),
            args["kind"].as_str(),
            limit,
        ),
        "get_dictation" => {
            dictations::get(&src.history, args["id"].as_number().expect("validated id"))
        }
        "search_prompts" => prompts::search(
            &src.queue,
            src.prompts_cwd.as_deref(),
            query,
            args["status"].as_str(),
            limit,
        ),
        "get_prompt" => prompts::get(
            &src.queue,
            src.prompts_cwd.as_deref(),
            args["id"].as_str().expect("validated id"),
        ),
        _ => unreachable!("registered tool has no handler"),
    };
    Ok(match value {
        Ok(data) => json!({
            "content": [{"type": "text", "text": data.to_string()}],
            "structuredContent": data,
        }),
        Err(err) => json!({
            "content": [{"type": "text", "text": err.to_string()}],
            "isError": true,
        }),
    })
}

fn error(id: &Value, code: i64, message: impl AsRef<str>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.as_ref()}})
}

fn handle(src: &Sources, msg: &Value) -> Option<Value> {
    let id = &msg["id"];
    if msg.is_array() {
        return Some(error(
            id,
            INVALID_REQUEST,
            "batch requests are not supported",
        ));
    }
    let Some(method) = msg["method"].as_str() else {
        if msg.get("method").is_none() && msg.get("id").is_some() {
            return None;
        }
        return Some(error(id, INVALID_REQUEST, "invalid request"));
    };
    let id = msg.get("id")?;
    let empty = Map::new();
    let params = match &msg["params"] {
        Value::Object(params) => params,
        other if !truthy(other) => &empty,
        _ => return Some(error(id, INVALID_PARAMS, "params must be an object")),
    };
    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|requested| PROTOCOL_VERSIONS.contains(requested))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "localflow", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": *TOOLS}),
        "tools/call" => match call_tool(src, params) {
            Ok(result) => result,
            Err(message) => return Some(error(id, INVALID_PARAMS, message)),
        },
        _ => {
            return Some(error(
                id,
                METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            ))
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn serve(src: &Sources, mut stdin: impl BufRead, mut stdout: impl Write) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if stdin.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let reply = match serde_json::from_slice(&line) {
            Ok(msg) => handle(src, &msg),
            Err(_) => Some(error(&Value::Null, PARSE_ERROR, "parse error")),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
}

fn main() -> io::Result<()> {
    serve(&Sources::local(), io::stdin().lock(), io::stdout().lock())
}
