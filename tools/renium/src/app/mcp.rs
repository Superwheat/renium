//! `rbx mcp`: a Model Context Protocol server on stdio that exposes the
//! everyday Renium commands as typed tools, so an agent host calls them
//! in-process instead of through a shell. Each call runs this executable
//! with the matching arguments in the server's working directory and returns
//! the command's JSON line as the tool result.

use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

const PROTOCOL_VERSION: &str = "2024-11-05";

const VALUE_SHAPES: &str = "Property values are JSON: numbers, booleans, strings, [x,y,z] for Vector3 or Position, \
{\"_type\":\"Color3\",\"r\":1,\"g\":0.5,\"b\":0}, {\"_type\":\"EnumItem\",\"name\":\"Neon\"}, \
{\"_type\":\"UDim2\",\"xScale\":0,\"xOffset\":200,\"yScale\":0,\"yOffset\":50}, \
{\"_type\":\"CFrame\",\"position\":[x,y,z]}.";

pub(crate) fn mcp_server() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("Failed to read an MCP message")?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                write_message(
                    &mut stdout,
                    &error_response(Value::Null, -32700, &error.to_string()),
                )?;
                continue;
            }
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => Some(result_response(
                id.clone().unwrap_or(Value::Null),
                json!({
                    "protocolVersion": params
                        .get("protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or(PROTOCOL_VERSION),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "renium", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Tools act on the Renium project in the working directory and the Roblox Studio place it has open. Results are the command's JSON. Edits through add, set, move, rename and add_many write the project files first and push to Studio when push is true (the default).",
                }),
            )),
            "notifications/initialized" | "notifications/cancelled" => None,
            "ping" => Some(result_response(
                id.clone().unwrap_or(Value::Null),
                json!({}),
            )),
            "tools/list" => Some(result_response(
                id.clone().unwrap_or(Value::Null),
                json!({ "tools": tool_definitions() }),
            )),
            "tools/call" => Some(call_tool(id.clone().unwrap_or(Value::Null), &params)),
            _ if id.is_none() => None,
            _ => Some(error_response(
                id.clone().unwrap_or(Value::Null),
                -32601,
                &format!("Unknown method {method}"),
            )),
        };
        if let Some(response) = response {
            write_message(&mut stdout, &response)?;
        }
    }
    Ok(())
}

fn write_message(stdout: &mut impl Write, message: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stdout, message)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn integer(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

fn boolean(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

fn object(description: &str) -> Value {
    json!({ "type": "object", "description": description, "additionalProperties": true })
}

fn string_list(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

fn tool_definitions() -> Vec<Value> {
    let push = boolean(
        "Push the result to the open Studio place right away, scoped to the instance and without a review (default true)",
    );
    vec![
        tool(
            "luau",
            "Run Luau in the open place and return what it returns. context edit (default) runs in the Edit window as one undo step; server or client run in the play session. Use it for reads of unsaved Studio state and for Studio-only edits that need no project file.",
            json!({
                "code": string("The Luau chunk; return a value to read it back"),
                "context": { "type": "string", "enum": ["edit", "server", "client"], "description": "Where to run (default edit)" },
                "player": string("Play client by name or index, with context client"),
            }),
            &["code"],
        ),
        tool(
            "tree",
            "List the saved instance tree below a service path from the project files (no Studio needed).",
            json!({
                "service": string("Service, e.g. Workspace"),
                "path": string("Dotted path below the service, e.g. Lobby.Kiosk (omit for the service root)"),
                "depth": integer("Levels to show (default 2)"),
                "limit": integer("Maximum rows"),
            }),
            &["service"],
        ),
        tool(
            "inspect",
            "Read one saved instance's fields from the project files: class, properties, attributes, tags.",
            json!({
                "service": string("Service, e.g. Workspace"),
                "path": string("Dotted path below the service, e.g. Lobby.Door"),
                "fields": string("Comma-separated field groups: props,attrs,tags (default props,attrs)"),
            }),
            &["service", "path"],
        ),
        tool(
            "find",
            "Find saved instances in one service by exact name, class or text.",
            json!({
                "service": string("Service to search"),
                "name": string("Exact instance name"),
                "class": string("Class name"),
                "text": string("Text to search for"),
                "path": string("Limit the search to this dotted subtree"),
                "limit": integer("Maximum results (default 20)"),
            }),
            &["service"],
        ),
        tool(
            "add",
            &format!(
                "Add one instance to the project files under a parent and push it to Studio. {VALUE_SHAPES}"
            ),
            json!({
                "service": string("Service that owns the parent, e.g. Workspace"),
                "name": string("Name of the new instance"),
                "class": string("Class name, e.g. Part, Folder, Script, TextLabel"),
                "parent": string("Dotted path of the parent below the service, e.g. Lobby (omit for the service root)"),
                "props": object("Properties as name: JSON value"),
                "attrs": object("Attributes as name: JSON value"),
                "push": push,
            }),
            &["service", "name", "class"],
        ),
        tool(
            "add_many",
            &format!(
                "Add several instances at once and push them. Each op has parent (dotted path with service, e.g. Workspace.Grid, or #ref of an earlier op), name, class, optional props, attrs, source (for scripts) and ref. {VALUE_SHAPES}"
            ),
            json!({
                "ops": { "type": "array", "items": { "type": "object", "additionalProperties": true }, "description": "The instances to create, in order" },
                "push": push,
            }),
            &["ops"],
        ),
        tool(
            "set",
            &format!(
                "Set properties or attributes of one saved instance and push it. {VALUE_SHAPES} A part's Position is stored as its CFrame."
            ),
            json!({
                "service": string("Service, e.g. Workspace"),
                "target": string("Dotted path below the service, e.g. Lobby.Door"),
                "props": object("Properties as name: JSON value"),
                "attrs": object("Attributes as name: JSON value"),
                "push": push,
            }),
            &["service", "target"],
        ),
        tool(
            "move",
            "Move a saved instance under another parent in the same service, reparent it in Studio and push it.",
            json!({
                "service": string("Service, e.g. Workspace"),
                "target": string("Dotted path of the instance below the service, e.g. Lobby.Kiosk"),
                "parent": string("Dotted path of the new parent below the service, e.g. Shops"),
                "push": push,
            }),
            &["service", "target", "parent"],
        ),
        tool(
            "rename",
            "Rename a saved instance and push it.",
            json!({
                "service": string("Service, e.g. Workspace"),
                "target": string("Dotted path of the instance below the service"),
                "name": string("The new name"),
                "push": push,
            }),
            &["service", "target", "name"],
        ),
        tool(
            "remove",
            "Delete a saved instance from the project files; push the service store afterwards to remove it in Studio.",
            json!({
                "service": string("Service, e.g. Workspace"),
                "target": string("Dotted path of the instance below the service"),
            }),
            &["service", "target"],
        ),
        tool(
            "push",
            "Push project files to the open Studio place: script files you wrote or edited, or a store file. A push over the review threshold returns heldForReview with the change list unless yes is true.",
            json!({
                "files": string_list("Project-relative file paths, e.g. src/ServerScriptService/Main.server.luau"),
                "verify_sources": boolean("Compare pushed script sources with Studio afterwards"),
                "yes": boolean("Apply a push over the review threshold at once"),
            }),
            &["files"],
        ),
        tool(
            "pull",
            "Pull the open Studio place into the project files, optionally one service, so Studio-only edits land in the files.",
            json!({ "service": string("Only this service") }),
            &[],
        ),
        tool(
            "check",
            "Check Luau syntax of project files offline.",
            json!({ "files": string_list("Project-relative .luau or .lua paths") }),
            &["files"],
        ),
        tool(
            "play",
            "Start, stop or restart a play session. A start waits for the player, optionally until a server-side Luau condition holds, and can return the server console and stop in the same call.",
            json!({
                "action": { "type": "string", "enum": ["start", "stop", "restart"], "description": "What to do (default start)" },
                "until": string("Luau expression on the server to wait for after the start, e.g. workspace.DistributedGameTime > 10"),
                "console": integer("Return this many recent server console entries with the result"),
                "stop_after": boolean("Stop the session once the start, until and console are done"),
                "players": integer("Launch a local server with this many clients instead of ordinary Play"),
            }),
            &[],
        ),
        tool(
            "console",
            "Read the Studio console: the play server (default), a play client, or the Edit window.",
            json!({
                "context": { "type": "string", "enum": ["server", "client", "edit"], "description": "Which console (default server)" },
                "level": { "type": "string", "enum": ["error", "warn", "info", "output"], "description": "Only entries of this level" },
                "grep": string("Only entries matching this case-insensitive regex"),
                "limit": integer("Maximum entries (default 200)"),
            }),
            &[],
        ),
        tool(
            "rbx",
            "Run any other rbx command by its arguments, for what the typed tools do not cover (rbx --help lists commands; the RENIUM guides explain them).",
            json!({
                "args": string_list("Arguments after rbx, e.g. [\"status\"] or [\"bg\", \"Workspace\", \"Lobby.Door\", \"Anchored\"]"),
                "stdin": string("Text to pass on standard input"),
            }),
            &["args"],
        ),
    ]
}

fn call_tool(id: Value, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match build_invocations(name, &arguments) {
        Ok(invocations) => {
            let mut texts = Vec::new();
            let mut is_error = false;
            for (args, stdin) in invocations {
                match run_rbx(&args, stdin.as_deref()) {
                    Ok((text, ok)) => {
                        is_error |= !ok;
                        texts.push(text);
                        if !ok {
                            break;
                        }
                    }
                    Err(error) => {
                        is_error = true;
                        texts.push(format!("{error:#}"));
                        break;
                    }
                }
            }
            result_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": texts.join("\n") }],
                    "isError": is_error,
                }),
            )
        }
        Err(error) => result_response(
            id,
            json!({
                "content": [{ "type": "text", "text": format!("{error:#}") }],
                "isError": true,
            }),
        ),
    }
}

type Invocation = (Vec<String>, Option<String>);

fn text_arg(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    arguments.get(key).and_then(|value| match value {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

fn flag(arguments: &Map<String, Value>, key: &str, default: bool) -> bool {
    arguments
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn assignments(arguments: &Map<String, Value>, key: &str, option: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(values) = arguments.get(key).and_then(Value::as_object) {
        for (name, value) in values {
            out.push(option.to_string());
            out.push(format!("{name}={value}"));
        }
    }
    out
}

fn required(arguments: &Map<String, Value>, key: &str) -> Result<String> {
    text_arg(arguments, key).with_context(|| format!("{key} is required"))
}

fn build_invocations(name: &str, arguments: &Map<String, Value>) -> Result<Vec<Invocation>> {
    let push = |args: &mut Vec<String>| {
        if flag(arguments, "push", true) {
            args.push("--push".into());
        }
    };
    let single = |args: Vec<String>| Ok(vec![(args, None)]);
    match name {
        "luau" => single(luau_args(arguments)?),
        "tree" => single(tree_args(arguments)?),
        "inspect" => single(vec![
            "in".into(),
            required(arguments, "service")?,
            required(arguments, "path")?,
            "-F".into(),
            text_arg(arguments, "fields").unwrap_or_else(|| "props,attrs".into()),
        ]),
        "find" => single(find_args(arguments)?),
        "add" => {
            let mut args = vec![
                "ba".to_string(),
                required(arguments, "service")?,
                "-n".into(),
                required(arguments, "name")?,
                "-c".into(),
                required(arguments, "class")?,
            ];
            if let Some(parent) = text_arg(arguments, "parent") {
                args.extend(["-I".into(), parent]);
            }
            args.extend(assignments(arguments, "props", "-p"));
            args.extend(assignments(arguments, "attrs", "-a"));
            push(&mut args);
            single(args)
        }
        "add_many" => {
            let mut args = vec!["ba".to_string(), "--batch".into(), "-".into()];
            push(&mut args);
            let ops = arguments.get("ops").context("ops is required")?;
            Ok(vec![(args, Some(json!({ "ops": ops }).to_string()))])
        }
        "set" => set_invocations(arguments),
        "move" => {
            let mut args = vec![
                "mv".to_string(),
                required(arguments, "service")?,
                "-i".into(),
                required(arguments, "target")?,
                "-I".into(),
                required(arguments, "parent")?,
            ];
            push(&mut args);
            single(args)
        }
        "rename" => {
            let mut args = vec![
                "rn".to_string(),
                required(arguments, "service")?,
                "-i".into(),
                required(arguments, "target")?,
                required(arguments, "name")?,
            ];
            push(&mut args);
            single(args)
        }
        "remove" => single(vec![
            "br".into(),
            required(arguments, "service")?,
            required(arguments, "target")?,
        ]),
        "push" => {
            let mut args = vec!["ps".to_string()];
            args.extend(string_items(arguments, "files"));
            if flag(arguments, "verify_sources", false) {
                args.push("--verify-sources".into());
            }
            if flag(arguments, "yes", false) {
                args.push("--yes".into());
            }
            single(args)
        }
        "pull" => {
            let mut args = vec!["pl".to_string()];
            if let Some(service) = text_arg(arguments, "service") {
                args.extend(["-s".into(), service]);
            }
            single(args)
        }
        "check" => {
            let mut args = vec!["ck".to_string()];
            args.extend(string_items(arguments, "files"));
            single(args)
        }
        "play" => single(play_args(arguments)),
        "console" => single(console_args(arguments)),
        "rbx" => {
            let args = string_items(arguments, "args");
            if args.is_empty() {
                anyhow::bail!("args is required");
            }
            Ok(vec![(args, text_arg(arguments, "stdin"))])
        }
        other => anyhow::bail!("Unknown tool {other}"),
    }
}

fn luau_args(arguments: &Map<String, Value>) -> Result<Vec<String>> {
    let mut args = vec!["l".to_string()];
    match text_arg(arguments, "context").as_deref() {
        Some("server") => args.push("--server".into()),
        Some("client") => args.push("--client".into()),
        _ => args.push("--edit".into()),
    }
    if let Some(player) = text_arg(arguments, "player") {
        args.extend(["-p".into(), player]);
    }
    args.push(required(arguments, "code")?);
    Ok(args)
}

fn tree_args(arguments: &Map<String, Value>) -> Result<Vec<String>> {
    let mut args = vec!["tr".to_string(), required(arguments, "service")?];
    if let Some(path) = text_arg(arguments, "path") {
        args.push(path);
    }
    args.extend([
        "--depth".into(),
        text_arg(arguments, "depth").unwrap_or_else(|| "2".into()),
    ]);
    if let Some(limit) = text_arg(arguments, "limit") {
        args.extend(["--limit".into(), limit]);
    }
    Ok(args)
}

fn find_args(arguments: &Map<String, Value>) -> Result<Vec<String>> {
    let mut args = vec!["f".to_string(), required(arguments, "service")?];
    if let Some(text) = text_arg(arguments, "text") {
        args.push(text);
    }
    for (key, option) in [
        ("name", "-n"),
        ("class", "-c"),
        ("path", "--path"),
        ("limit", "--limit"),
    ] {
        if let Some(value) = text_arg(arguments, key) {
            args.extend([option.into(), value]);
        }
    }
    Ok(args)
}

/// Properties and attributes of one instance are two `bs` calls, since a
/// call takes one scope.
fn set_invocations(arguments: &Map<String, Value>) -> Result<Vec<Invocation>> {
    let service = required(arguments, "service")?;
    let target = required(arguments, "target")?;
    let props = assignments(arguments, "props", "-p");
    let attrs = assignments(arguments, "attrs", "-p");
    if props.is_empty() && attrs.is_empty() {
        anyhow::bail!("set needs props or attrs");
    }
    let mut invocations = Vec::new();
    for (values, scope) in [(props, None), (attrs, Some("attribute"))] {
        if values.is_empty() {
            continue;
        }
        let mut call = vec!["bs".to_string(), service.clone(), target.clone()];
        call.extend(values);
        if let Some(scope) = scope {
            call.extend(["-S".into(), scope.into()]);
        }
        if flag(arguments, "push", true) {
            call.push("--push".into());
        }
        invocations.push((call, None));
    }
    Ok(invocations)
}

fn play_args(arguments: &Map<String, Value>) -> Vec<String> {
    let mut args = vec!["play".to_string()];
    match text_arg(arguments, "action").as_deref() {
        Some("stop") => args.push("-x".into()),
        Some("restart") => args.push("-r".into()),
        _ => args.push("-s".into()),
    }
    if let Some(players) = text_arg(arguments, "players") {
        args.extend(["-p".into(), players]);
    }
    if let Some(until) = text_arg(arguments, "until") {
        args.extend(["--until".into(), until]);
    }
    if let Some(console) = text_arg(arguments, "console") {
        args.extend(["--console".into(), console]);
    }
    if flag(arguments, "stop_after", false) {
        args.push("--stop-after".into());
    }
    args
}

fn console_args(arguments: &Map<String, Value>) -> Vec<String> {
    let mut args = vec!["co".to_string()];
    match text_arg(arguments, "context").as_deref() {
        Some("client") => args.push("--client".into()),
        Some("edit") => {}
        _ => args.push("--server".into()),
    }
    for (key, option) in [("level", "--level"), ("grep", "--grep"), ("limit", "-n")] {
        if let Some(value) = text_arg(arguments, key) {
            args.extend([option.into(), value]);
        }
    }
    args
}

fn string_items(arguments: &Map<String, Value>, key: &str) -> Vec<String> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            Value::String(text) => Some(text.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })
        .collect()
}

fn run_rbx(args: &[String], stdin: Option<&str>) -> Result<(String, bool)> {
    let exe = std::env::current_exe().context("Failed to locate the rbx executable")?;
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("Failed to run rbx")?;
    if let Some(text) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        pipe.write_all(text.as_bytes())?;
    }
    let output = child.wait_with_output().context("Failed to wait for rbx")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let text = if stdout.is_empty() {
        stderr
    } else if stderr.is_empty() || output.status.success() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    };
    Ok((text, output.status.success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_become_rbx_arguments() {
        let args = serde_json::from_value::<Map<String, Value>>(json!({
            "service": "Workspace", "name": "Pad", "class": "Part", "parent": "Lobby",
            "props": { "Anchored": true, "Size": [8, 1, 8] }, "attrs": { "Health": 50 }
        }))
        .unwrap();
        let invocations = build_invocations("add", &args).unwrap();
        assert_eq!(
            invocations[0].0,
            [
                "ba",
                "Workspace",
                "-n",
                "Pad",
                "-c",
                "Part",
                "-I",
                "Lobby",
                "-p",
                "Anchored=true",
                "-p",
                "Size=[8,1,8]",
                "-a",
                "Health=50",
                "--push"
            ]
        );
        let args = serde_json::from_value::<Map<String, Value>>(json!({
            "service": "Workspace", "target": "Lobby.Pad", "props": { "Transparency": 0.5 }, "attrs": { "Owner": "x" }, "push": false
        }))
        .unwrap();
        let invocations = build_invocations("set", &args).unwrap();
        assert_eq!(invocations.len(), 2);
        assert_eq!(
            invocations[0].0,
            ["bs", "Workspace", "Lobby.Pad", "-p", "Transparency=0.5"]
        );
        assert_eq!(
            invocations[1].0,
            [
                "bs",
                "Workspace",
                "Lobby.Pad",
                "-p",
                "Owner=\"x\"",
                "-S",
                "attribute"
            ]
        );
        let args = serde_json::from_value::<Map<String, Value>>(
            json!({ "code": "return 1", "context": "server" }),
        )
        .unwrap();
        assert_eq!(
            build_invocations("luau", &args).unwrap()[0].0,
            ["l", "--server", "return 1"]
        );
        let args = serde_json::from_value::<Map<String, Value>>(
            json!({ "until": "x", "console": 40, "stop_after": true }),
        )
        .unwrap();
        assert_eq!(
            build_invocations("play", &args).unwrap()[0].0,
            [
                "play",
                "-s",
                "--until",
                "x",
                "--console",
                "40",
                "--stop-after"
            ]
        );
        let args = serde_json::from_value::<Map<String, Value>>(
            json!({ "ops": [{ "parent": "Workspace", "name": "A", "class": "Folder" }] }),
        )
        .unwrap();
        let (call, stdin) = build_invocations("add_many", &args).unwrap().remove(0);
        assert_eq!(call, ["ba", "--batch", "-", "--push"]);
        assert!(stdin.unwrap().contains("\"ops\""));
        assert!(build_invocations("nope", &Map::new()).is_err());
    }

    #[test]
    fn every_tool_has_a_schema_with_its_required_keys() {
        for tool in tool_definitions() {
            let schema = &tool["inputSchema"];
            let properties = schema["properties"].as_object().unwrap();
            for key in schema["required"].as_array().unwrap() {
                assert!(
                    properties.contains_key(key.as_str().unwrap()),
                    "{}",
                    tool["name"]
                );
            }
        }
    }
}
