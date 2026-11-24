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
        "search_prompts" => prompts::search(&src.queue, query, args["status"].as_str(), limit),
        "get_prompt" => prompts::get(&src.queue, args["id"].as_str().expect("validated id")),
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use super::*;

    fn sources(dir: &Path) -> Sources {
        Sources {
            history: dir.join("history.sqlite"),
            queue: dir.join("queue.json"),
        }
    }

    fn tool_schema(name: &str) -> &'static Value {
        &TOOLS.iter().find(|t| t["name"] == name).unwrap()["inputSchema"]
    }

    fn call(src: &Sources, name: Value, arguments: Value) -> Result<Value, String> {
        let params = json!({"name": name, "arguments": arguments});
        call_tool(src, params.as_object().unwrap())
    }

    fn request(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params})
    }

    /// Every file under `dir` with its bytes; reads must leave this unchanged.
    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(snapshot(&path));
            } else {
                files.insert(path.display().to_string(), fs::read(&path).unwrap());
            }
        }
        files
    }

    #[test]
    fn tools_list_the_four_read_only_tools_in_python_order() {
        let names: Vec<_> = TOOLS.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "search_dictations",
                "get_dictation",
                "search_prompts",
                "get_prompt"
            ]
        );
        for tool in TOOLS.iter() {
            let keys: Vec<_> = tool.as_object().unwrap().keys().cloned().collect();
            assert_eq!(keys, ["name", "description", "inputSchema", "annotations"]);
            assert_eq!(
                tool["annotations"],
                json!({"readOnlyHint": true, "openWorldHint": false})
            );
            let schema = tool["inputSchema"].as_object().unwrap();
            let required = tool["name"].as_str().unwrap().starts_with("get_");
            let expected: &[&str] = if required {
                &["type", "properties", "required", "additionalProperties"]
            } else {
                &["type", "properties", "additionalProperties"]
            };
            assert_eq!(schema.keys().collect::<Vec<_>>(), expected);
            assert_eq!(schema["additionalProperties"], json!(false));
        }
        assert_eq!(tool_schema("get_dictation")["required"], json!(["id"]));
        assert_eq!(
            tool_schema("get_dictation")["properties"]["id"],
            json!({"type": "integer"})
        );
        assert_eq!(
            tool_schema("get_prompt")["properties"]["id"],
            json!({"type": "string"})
        );
        let dictations = &tool_schema("search_dictations")["properties"];
        assert_eq!(
            dictations["kind"]["enum"],
            json!(["dictate", "command", "transform"])
        );
        assert_eq!(
            dictations["limit"],
            json!({"type": "integer", "description": "Maximum results (1-50, default 10)."})
        );
        assert_eq!(
            tool_schema("search_prompts")["properties"]["status"]["enum"],
            json!(["pending", "approved", "rejected", "dispatched", "failed"])
        );
    }

    #[test]
    fn validation_reports_errors_in_python_order_and_wording() {
        let search = tool_schema("search_dictations");
        let get = tool_schema("get_dictation");
        for args in [json!([]), json!(false), json!(0), json!(""), json!("{}")] {
            assert_eq!(
                validate(search, &args),
                Err("arguments must be an object".into())
            );
        }
        assert_eq!(validate(search, &Value::Null), Ok(()));
        assert_eq!(validate(search, &json!({})), Ok(()));
        assert_eq!(
            validate(get, &Value::Null),
            Err("missing argument(s): id".into())
        );
        // Unknown names are sorted by code point and reported before missing ones.
        assert_eq!(
            validate(get, &json!({"z": 1, "a": 2, "é": 3})),
            Err("unknown argument(s): a, z, é".into())
        );
        // An optional null is dropped; a required null is a type error.
        assert_eq!(
            validate(search, &json!({"query": null, "kind": null, "limit": null})),
            Ok(())
        );
        assert_eq!(
            validate(get, &json!({"id": null})),
            Err("id must be a integer".into())
        );
        for id in [json!(true), json!(1.0), json!("1"), json!([1]), json!({})] {
            assert_eq!(
                validate(get, &json!({ "id": id })),
                Err("id must be a integer".into()),
                "{id}"
            );
        }
        for id in [json!(0), json!(-1), json!(i64::MIN), json!(u64::MAX)] {
            assert_eq!(validate(get, &json!({ "id": id })), Ok(()), "{id}");
        }
        assert_eq!(
            validate(tool_schema("get_prompt"), &json!({"id": 1})),
            Err("id must be a string".into())
        );
        assert_eq!(
            validate(search, &json!({"kind": "typo"})),
            Err("kind must be one of dictate, command, transform".into())
        );
        assert_eq!(
            validate(search, &json!({"kind": 3})),
            Err("kind must be a string".into())
        );
        // The first bad argument in the order the client sent them.
        assert_eq!(
            validate(search, &json!({"query": 1, "limit": "x"})),
            Err("query must be a string".into())
        );
        assert_eq!(
            validate(search, &json!({"limit": "x", "query": 1})),
            Err("limit must be a integer".into())
        );
    }

    #[test]
    fn unknown_tool_names_are_reported_with_python_repr() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let cases = [
            (Value::Null, "None"),
            (json!("no_such_tool"), "'no_such_tool'"),
            (json!(["no_such_tool"]), "['no_such_tool']"),
            (json!(12), "12"),
            (
                json!({"a": [1.5, null, true, 100.0], "b": "it's"}),
                "{'a': [1.5, None, True, 100.0], 'b': \"it's\"}",
            ),
        ];
        for (name, shown) in cases {
            assert_eq!(
                call(&src, name, json!({})),
                Err(format!("unknown tool: {shown}"))
            );
        }
        assert!(snapshot(dir.path()).is_empty());
    }

    #[test]
    fn limit_is_clamped_like_python_int_handling() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let queue: Vec<_> = (0..60)
            .map(|n| json!({"id": format!("p{n:02}"), "created": format!("2024-01-01T00:00:{n:02}")}))
            .collect();
        fs::write(&src.queue, serde_json::to_vec(&queue).unwrap()).unwrap();
        let count = |limit: Value| {
            let result = call(&src, json!("search_prompts"), json!({ "limit": limit })).unwrap();
            result["structuredContent"]["count"].as_u64().unwrap()
        };
        assert_eq!(count(Value::Null), 10);
        assert_eq!(count(json!(0)), 1);
        assert_eq!(count(json!(-5)), 1);
        assert_eq!(count(json!(i64::MIN)), 1);
        assert_eq!(count(json!(7)), 7);
        assert_eq!(count(json!(50)), 50);
        assert_eq!(count(json!(51)), 50);
        assert_eq!(count(json!(i64::MAX)), 50);
        assert_eq!(count(json!(i64::MAX as u64 + 1)), 50);
        assert_eq!(count(json!(u64::MAX)), 50);
        let newest = call(&src, json!("search_prompts"), json!({"limit": 1})).unwrap();
        assert_eq!(newest["structuredContent"]["results"][0]["id"], "p59");
    }

    #[test]
    fn prompt_tools_never_write_next_to_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let missing = call(&src, json!("search_prompts"), Value::Null).unwrap();
        assert_eq!(
            missing["structuredContent"],
            json!({"count": 0, "match": "recent", "results": []})
        );
        assert!(
            snapshot(dir.path()).is_empty(),
            "a missing queue stays missing"
        );

        let body = dir.path().join("brief.md");
        fs::write(
            &body,
            "---\r\ntitle: x\r\n---\r\n\r\nReview cucumber budget",
        )
        .unwrap();
        let queue = json!([
            {"id": "budget", "title": "Budget", "status": "pending", "created": "2024-06-20T13:04:00",
             "path": body.display().to_string()},
            {"id": 2},
            3,
        ]);
        fs::write(&src.queue, serde_json::to_vec(&queue).unwrap()).unwrap();
        let before = snapshot(dir.path());
        let calls = [
            ("search_prompts", json!({})),
            ("search_prompts", json!({"query": "cucumber"})),
            (
                "search_prompts",
                json!({"query": "cumb", "status": "pending"}),
            ),
            ("get_prompt", json!({"id": "budget"})),
            ("get_prompt", json!({"id": "doesn't exist"})),
        ];
        let replies: Vec<_> = calls
            .into_iter()
            .map(|(name, args)| call(&src, json!(name), args).unwrap())
            .collect();
        assert_eq!(snapshot(dir.path()), before);
        assert_eq!(replies[0]["structuredContent"]["count"], 1);
        assert_eq!(replies[1]["structuredContent"]["match"], "ranked");
        assert_eq!(replies[2]["structuredContent"]["match"], "substring");
        let ok = &replies[3];
        assert_eq!(ok["structuredContent"]["body"], "Review cucumber budget");
        assert_eq!(
            ok["content"][0]["text"].as_str().unwrap(),
            ok["structuredContent"].to_string()
        );
        assert_eq!(
            replies[4],
            json!({
                "content": [{"type": "text", "text": "no work prompt with id \"doesn't exist\""}],
                "isError": true,
            })
        );
    }

    #[test]
    fn rpc_envelopes_match_the_python_server() {
        let dir = tempfile::tempdir().unwrap();
        let src = sources(dir.path());
        let reply = |msg: Value| handle(&src, &msg);
        let err = |id: Value, code: i64, message: &str| {
            Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
        };
        let ok = |result: Value| Some(json!({"jsonrpc": "2.0", "id": 7, "result": result}));

        assert_eq!(
            reply(json!([request("ping", Value::Null)])),
            err(
                Value::Null,
                INVALID_REQUEST,
                "batch requests are not supported"
            )
        );
        assert_eq!(
            reply(json!(12)),
            err(Value::Null, INVALID_REQUEST, "invalid request")
        );
        assert_eq!(
            reply(json!({"id": 3, "method": 5})),
            err(json!(3), INVALID_REQUEST, "invalid request")
        );
        assert_eq!(
            reply(json!({"jsonrpc": "2.0"})),
            err(Value::Null, INVALID_REQUEST, "invalid request")
        );
        assert_eq!(
            reply(json!({"jsonrpc": "2.0", "id": 29, "result": {}})),
            None
        );
        assert_eq!(
            reply(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": [1]})),
            None
        );
        for falsy in [
            Value::Null,
            json!(0),
            json!(""),
            json!([]),
            json!(false),
            json!({}),
        ] {
            assert_eq!(reply(request("ping", falsy)), ok(json!({})));
        }
        for params in [json!([1]), json!("x"), json!(1), json!(true)] {
            assert_eq!(
                reply(request("ping", params)),
                err(json!(7), INVALID_PARAMS, "params must be an object")
            );
        }
        for (requested, chosen) in [
            (json!("2024-11-05"), "2024-11-05"),
            (json!("2025-03-26"), "2025-03-26"),
            (json!("2099-12-31"), "2025-06-18"),
            (json!(["2024-11-05"]), "2025-06-18"),
            (Value::Null, "2025-06-18"),
        ] {
            let init = reply(request(
                "initialize",
                json!({ "protocolVersion": requested }),
            ));
            let result = &init.unwrap()["result"];
            assert_eq!(result["protocolVersion"], chosen);
            assert_eq!(result["capabilities"], json!({"tools": {}}));
            assert_eq!(result["serverInfo"]["name"], "localflow");
            assert_eq!(result["instructions"], INSTRUCTIONS);
        }
        assert_eq!(
            reply(request("tools/list", Value::Null)),
            ok(json!({"tools": *TOOLS}))
        );
        assert_eq!(
            reply(request("resources/list", Value::Null)),
            err(
                json!(7),
                METHOD_NOT_FOUND,
                "method not found: resources/list"
            )
        );
        assert_eq!(
            reply(request("tools/call", json!({"name": "get_dictation"}))),
            err(json!(7), INVALID_PARAMS, "missing argument(s): id")
        );
        assert!(snapshot(dir.path()).is_empty());
    }

    #[test]
    fn serve_answers_each_request_line_and_skips_blanks_and_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let input = [
            "",
            " \t\r",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "{malformed",
            r#"{"jsonrpc":"2.0","id":"two","method":"ping"}"#,
        ]
        .join("\n");
        let mut out = Vec::new();
        serve(&sources(dir.path()), input.as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'));
        let replies: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            replies,
            [
                json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
                json!({"jsonrpc": "2.0", "id": null,
                       "error": {"code": PARSE_ERROR, "message": "parse error"}}),
                json!({"jsonrpc": "2.0", "id": "two", "result": {}}),
            ]
        );
        assert!(snapshot(dir.path()).is_empty());
    }
}
