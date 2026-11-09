//! Registers `rbx mcp` as the `renium` MCP server with the agents installed
//! for this user. An entry is added or removed as text, so every other key,
//! comment and the file's layout stay as they were; the edited file must
//! parse back to the original plus or minus that entry before it is written,
//! and the original is kept beside it as `<file>.renium.bak`.
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::output::{ReportedFailure, print_json_output};
use crate::project::config::parse_jsonc_value;
use crate::system::files::{canonical_path, home_directory};

const NAME: &str = "renium";
/// Codex stops waiting for a tool after 60 seconds unless told otherwise;
/// a Play check can take minutes.
const CODEX_TOOL_TIMEOUT_SECONDS: i64 = 15 * 60;

#[derive(Clone, Copy)]
enum Kind {
    McpServers,
    OpenCode,
    Codex,
}

/// An agent counts as installed when its folder exists (or, for Claude
/// Code, its config file); nothing is written for agents that are not.
const TARGETS: [(&str, &str, Kind); 6] = [
    (".claude", ".claude.json", Kind::McpServers),
    (".codex", ".codex/config.toml", Kind::Codex),
    (".cursor", ".cursor/mcp.json", Kind::McpServers),
    (
        ".codeium/windsurf",
        ".codeium/windsurf/mcp_config.json",
        Kind::McpServers,
    ),
    (".gemini", ".gemini/settings.json", Kind::McpServers),
    (
        ".config/opencode",
        ".config/opencode/opencode.json",
        Kind::OpenCode,
    ),
];

#[derive(Default)]
pub(crate) struct Registration {
    pub(crate) changed: Vec<PathBuf>,
    pub(crate) unchanged: Vec<PathBuf>,
    pub(crate) failed: Vec<(PathBuf, String)>,
}

impl Registration {
    pub(crate) fn failures(&self) -> Value {
        Value::Array(
            self.failed
                .iter()
                .map(|(path, error)| json!({"path": path, "error": error}))
                .collect(),
        )
    }

    /// One sentence for setup's text output, when anything happened.
    pub(crate) fn describe(&self, action: &str) -> Option<String> {
        let join = |paths: Vec<&PathBuf>| {
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !self.changed.is_empty() {
            parts.push(format!(
                "renium MCP server {action} {}",
                join(self.changed.iter().collect())
            ));
        }
        if !self.failed.is_empty() {
            parts.push(format!(
                "MCP registration failed for {}",
                join(self.failed.iter().map(|(path, _)| path).collect())
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// `rbx mcp --install` registers the executable that runs it.
pub(crate) fn command(uninstall: bool) -> Result<()> {
    let home = home_directory().context("Cannot find the home folder")?;
    let (registration, executable) = if uninstall {
        (remove_in(&home), None)
    } else {
        let executable = std::env::current_exe().context("Cannot locate the Renium executable")?;
        let executable = canonical_path(&executable).unwrap_or(executable);
        (install_in(&home, &executable), Some(executable))
    };
    let mut response = json!({
        "ok": registration.failed.is_empty(),
        "changed": registration.changed,
    });
    if let Some(executable) = executable {
        response["command"] = json!(executable);
        if !registration.unchanged.is_empty() {
            response["present"] = json!(registration.unchanged);
        }
    }
    if !registration.failed.is_empty() {
        response["failed"] = registration.failures();
    }
    print_json_output(&response, false)?;
    if registration.failed.is_empty() {
        Ok(())
    } else {
        Err(ReportedFailure.into())
    }
}

/// The installed core, which updates replace in place, when there is one:
/// setup may run from a staging folder that is gone afterwards.
pub(crate) fn setup_executable() -> Result<PathBuf> {
    let name = if cfg!(windows) {
        "renium.exe"
    } else {
        "renium"
    };
    let executable = match crate::app::update::installer_core_root()
        .map(|root| root.join(name))
        .filter(|path| path.is_file())
    {
        Some(installed) => installed,
        None => std::env::current_exe().context("Cannot locate the Renium executable")?,
    };
    Ok(canonical_path(&executable).unwrap_or(executable))
}

pub(crate) fn install(executable: &Path) -> Registration {
    home_directory()
        .map(|home| install_in(&home, executable))
        .unwrap_or_default()
}

pub(crate) fn remove() -> Registration {
    home_directory()
        .map(|home| remove_in(&home))
        .unwrap_or_default()
}

/// Adds the entry where an agent has none; an existing `renium` entry is
/// left alone.
pub(crate) fn install_in(home: &Path, executable: &Path) -> Registration {
    let command = executable.to_string_lossy();
    apply(home, |text, kind| match kind {
        Kind::Codex => toml_add(text, &command),
        Kind::McpServers => json_add(
            text,
            "mcpServers",
            Node::Object(vec![
                ("command", Node::Leaf(json!(command))),
                ("args", Node::Leaf(json!(["mcp"]))),
            ]),
        ),
        Kind::OpenCode => json_add(
            text,
            "mcp",
            Node::Object(vec![
                ("type", Node::Leaf(json!("local"))),
                ("command", Node::Leaf(json!([command, "mcp"]))),
                ("enabled", Node::Leaf(json!(true))),
            ]),
        ),
    })
}

pub(crate) fn remove_in(home: &Path) -> Registration {
    apply(home, |text, kind| match kind {
        Kind::Codex => toml_remove(text),
        Kind::McpServers => json_remove(text, "mcpServers"),
        Kind::OpenCode => json_remove(text, "mcp"),
    })
}

fn apply(home: &Path, edit: impl Fn(&str, Kind) -> Result<Option<String>>) -> Registration {
    let mut registration = Registration::default();
    for (marker, file, kind) in TARGETS {
        let path = under(home, file);
        if !under(home, marker).is_dir() && !path.is_file() {
            continue;
        }
        match edit_file(&path, |text| edit(text, kind)) {
            Ok(true) => registration.changed.push(path),
            Ok(false) => registration.unchanged.push(path),
            Err(error) => registration
                .failed
                .push((path, format!("{error:#}; edit it by hand"))),
        }
    }
    registration
}

fn under(home: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(home.to_path_buf(), |path, part| path.join(part))
}

fn edit_file(path: &Path, edit: impl Fn(&str) -> Result<Option<String>>) -> Result<bool> {
    let current = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    let Some(updated) = edit(current.as_deref().unwrap_or_default())
        .with_context(|| format!("Could not update {}", path.display()))?
    else {
        return Ok(false);
    };
    if current.is_some() {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".renium.bak");
        fs::copy(path, &backup).with_context(|| format!("Failed to back up {}", path.display()))?;
    }
    fs::write(path, updated).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(true)
}

/// A JSON value written with keys in the given order.
enum Node {
    Leaf(Value),
    Object(Vec<(&'static str, Node)>),
}

impl Node {
    fn value(&self) -> Value {
        match self {
            Self::Leaf(value) => value.clone(),
            Self::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(key, node)| ((*key).to_string(), node.value()))
                    .collect(),
            ),
        }
    }

    /// `indent` is the indentation of the line the value starts on; `None`
    /// writes it on one line.
    fn render(&self, indent: Option<&str>, layout: &Layout) -> String {
        match (self, indent) {
            (Self::Leaf(value), None) => value.to_string(),
            (Self::Leaf(value), Some(indent)) => {
                let mut bytes = Vec::new();
                let formatter =
                    serde_json::ser::PrettyFormatter::with_indent(layout.unit.as_bytes());
                let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
                value
                    .serialize(&mut serializer)
                    .expect("a JSON value serializes");
                String::from_utf8_lossy(&bytes)
                    .replace('\n', &format!("{}{indent}", layout.newline))
            }
            (Self::Object(members), None) => format!(
                "{{{}}}",
                members
                    .iter()
                    .map(|(key, node)| format!("{}: {}", json!(key), node.render(None, layout)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            (Self::Object(members), Some(indent)) => {
                let inner = format!("{indent}{}", layout.unit);
                let lines = members
                    .iter()
                    .map(|(key, node)| {
                        format!(
                            "{inner}{}: {}",
                            json!(key),
                            node.render(Some(&inner), layout)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(&format!(",{}", layout.newline));
                format!("{{{nl}{lines}{nl}{indent}}}", nl = layout.newline)
            }
        }
    }
}

struct Layout {
    unit: String,
    newline: &'static str,
    one_line: bool,
}

struct Member {
    key: String,
    start: usize,
    value: Range<usize>,
}

struct Object {
    open: usize,
    close: usize,
    members: Vec<Member>,
}

impl Object {
    fn member(&self, key: &str) -> Option<&Member> {
        self.members.iter().rev().find(|member| member.key == key)
    }
}

fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(body) => ("\u{feff}", body),
        None => ("", text),
    }
}

fn json_add(text: &str, container: &str, entry: Node) -> Result<Option<String>> {
    let (bom, body) = split_bom(text);
    let created = body.trim().is_empty();
    let body = if created { "{}\n" } else { body };
    let before = parse_jsonc_value(body).context("It is not valid JSON")?;
    match before.get(container) {
        Some(Value::Object(servers)) if servers.contains_key(NAME) => return Ok(None),
        Some(Value::Object(_)) | None => {}
        Some(_) => bail!("Its `{container}` is not an object"),
    }
    let root = root_object(body)?;
    let layout = Layout {
        unit: root
            .members
            .first()
            .and_then(|member| line_indent(body, member.start))
            .filter(|indent| !indent.is_empty())
            .unwrap_or("  ")
            .to_string(),
        newline: if body.contains("\r\n") { "\r\n" } else { "\n" },
        one_line: !created && !body.trim_end().contains('\n'),
    };
    let mut expected = before;
    expected[container][NAME] = entry.value();
    let updated = match root.member(container) {
        Some(member) => insert(
            body,
            &object_at(body, member.value.start)?,
            NAME,
            &entry,
            &layout,
        ),
        None => insert(
            body,
            &root,
            container,
            &Node::Object(vec![(NAME, entry)]),
            &layout,
        ),
    };
    ensure!(
        parse_jsonc_value(&updated).ok() == Some(expected),
        "Adding the entry would change other settings"
    );
    Ok(Some(format!("{bom}{updated}")))
}

fn json_remove(text: &str, container: &str) -> Result<Option<String>> {
    let (bom, body) = split_bom(text);
    if body.trim().is_empty() {
        return Ok(None);
    }
    let before = parse_jsonc_value(body).context("It is not valid JSON")?;
    if before
        .get(container)
        .and_then(|servers| servers.get(NAME))
        .is_none()
    {
        return Ok(None);
    }
    let root = root_object(body)?;
    let servers = object_at(
        body,
        root.member(container)
            .context("The entry was not found")?
            .value
            .start,
    )?;
    let index = servers
        .members
        .iter()
        .rposition(|member| member.key == NAME)
        .context("The entry was not found")?;
    let updated = without_member(body, &servers, index);
    let mut expected = before;
    if let Some(servers) = expected[container].as_object_mut() {
        servers.remove(NAME);
    }
    ensure!(
        parse_jsonc_value(&updated).ok() == Some(expected),
        "Removing the entry would change other settings"
    );
    Ok(Some(format!("{bom}{updated}")))
}

/// Inserts `"key": node` as the last member of `object`, in the layout of
/// its other members.
fn insert(text: &str, object: &Object, key: &str, node: &Node, layout: &Layout) -> String {
    let key = json!(key);
    match object.members.last() {
        Some(last) => {
            let at = last.value.end;
            let member = match line_indent(text, last.start) {
                Some(indent) => format!(
                    ",{}{indent}{key}: {}",
                    layout.newline,
                    node.render(Some(indent), layout)
                ),
                None => format!(", {key}: {}", node.render(None, layout)),
            };
            format!("{}{member}{}", &text[..at], &text[at..])
        }
        None if layout.one_line => format!(
            "{}{{{key}: {}}}{}",
            &text[..object.open],
            node.render(None, layout),
            &text[object.close + 1..]
        ),
        None => {
            let indent = leading_whitespace(text, object.open);
            let inner = format!("{indent}{}", layout.unit);
            format!(
                "{}{{{nl}{inner}{key}: {}{nl}{indent}}}{}",
                &text[..object.open],
                node.render(Some(&inner), layout),
                &text[object.close + 1..],
                nl = layout.newline
            )
        }
    }
}

fn without_member(text: &str, object: &Object, index: usize) -> String {
    let member = &object.members[index];
    let cut = if object.members.len() == 1 {
        object.open + 1..object.close
    } else if let Some(next) = object.members.get(index + 1) {
        member.start..next.start
    } else {
        object.members[index - 1].value.end..member.value.end
    };
    format!("{}{}", &text[..cut.start], &text[cut.end..])
}

/// The whitespace before `index` when nothing else precedes it on its line.
fn line_indent(text: &str, index: usize) -> Option<&str> {
    let start = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
    let prefix = &text[start..index];
    prefix
        .bytes()
        .all(|byte| byte == b' ' || byte == b'\t')
        .then_some(prefix)
}

fn leading_whitespace(text: &str, index: usize) -> &str {
    let start = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
    let line = &text[start..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn root_object(text: &str) -> Result<Object> {
    object_at(text, skip_trivia(text.as_bytes(), 0)?)
}

fn object_at(text: &str, open: usize) -> Result<Object> {
    let bytes = text.as_bytes();
    ensure!(bytes.get(open) == Some(&b'{'), "Expected an object");
    let mut members = Vec::new();
    let mut index = skip_trivia(bytes, open + 1)?;
    while bytes.get(index) != Some(&b'}') {
        ensure!(bytes.get(index) == Some(&b'"'), "Expected a key");
        let key_end = string_end(bytes, index)?;
        let key = serde_json::from_str(&text[index..key_end])?;
        let colon = skip_trivia(bytes, key_end)?;
        ensure!(bytes.get(colon) == Some(&b':'), "Expected ':'");
        let start = skip_trivia(bytes, colon + 1)?;
        let end = value_end(bytes, start)?;
        members.push(Member {
            key,
            start: index,
            value: start..end,
        });
        index = skip_trivia(bytes, end)?;
        if bytes.get(index) == Some(&b',') {
            index = skip_trivia(bytes, index + 1)?;
        } else {
            ensure!(bytes.get(index) == Some(&b'}'), "Expected ',' or '}}'");
        }
    }
    Ok(Object {
        open,
        close: index,
        members,
    })
}

fn skip_trivia(bytes: &[u8], mut index: usize) -> Result<usize> {
    loop {
        match (bytes.get(index), bytes.get(index + 1)) {
            (Some(b' ' | b'\t' | b'\r' | b'\n'), _) => index += 1,
            (Some(b'/'), Some(b'/')) => {
                index =
                    memchr::memchr(b'\n', &bytes[index..]).map_or(bytes.len(), |end| index + end);
            }
            (Some(b'/'), Some(b'*')) => {
                index = memchr::memmem::find(&bytes[index + 2..], b"*/")
                    .map(|end| index + 2 + end + 2)
                    .context("Unterminated comment")?;
            }
            _ => return Ok(index),
        }
    }
}

fn string_end(bytes: &[u8], start: usize) -> Result<usize> {
    let mut index = start + 1;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'\\' => index += 2,
            b'"' => return Ok(index + 1),
            _ => index += 1,
        }
    }
    bail!("Unterminated string")
}

fn value_end(bytes: &[u8], start: usize) -> Result<usize> {
    match bytes.get(start) {
        Some(b'"') => string_end(bytes, start),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut index = start;
            loop {
                index = skip_trivia(bytes, index)?;
                match bytes.get(index) {
                    Some(b'"') => index = string_end(bytes, index)?,
                    Some(b'{' | b'[') => {
                        depth += 1;
                        index += 1;
                    }
                    Some(b'}' | b']') => {
                        depth -= 1;
                        index += 1;
                        if depth == 0 {
                            return Ok(index);
                        }
                    }
                    Some(_) => index += 1,
                    None => bail!("Unterminated value"),
                }
            }
        }
        Some(_) => Ok(bytes[start..]
            .iter()
            .position(|byte| {
                matches!(
                    byte,
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n' | b'/'
                )
            })
            .map_or(bytes.len(), |end| start + end)),
        None => bail!("Missing value"),
    }
}

fn toml_add(text: &str, command: &str) -> Result<Option<String>> {
    let before: toml::Table = toml::from_str(text).context("It is not valid TOML")?;
    if before
        .get("mcp_servers")
        .and_then(|servers| servers.get(NAME))
        .is_some()
    {
        return Ok(None);
    }
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut entry = toml::Table::new();
    entry.insert("command".into(), toml::Value::String(command.to_string()));
    entry.insert(
        "args".into(),
        toml::Value::Array(vec![toml::Value::String("mcp".into())]),
    );
    entry.insert(
        "tool_timeout_sec".into(),
        toml::Value::Integer(CODEX_TOOL_TIMEOUT_SECONDS),
    );
    let block = [
        format!("[mcp_servers.{NAME}]"),
        format!("command = {}", entry["command"]),
        format!("args = {}", entry["args"]),
        format!("tool_timeout_sec = {}", entry["tool_timeout_sec"]),
    ]
    .join(newline);
    let mut updated = text.to_string();
    if !updated.trim().is_empty() {
        if !updated.ends_with('\n') {
            updated.push_str(newline);
        }
        updated.push_str(newline);
    }
    updated.push_str(&block);
    updated.push_str(newline);
    let mut expected = before;
    let servers = expected
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(servers) = servers {
        servers.insert(NAME.into(), toml::Value::Table(entry));
    }
    ensure!(
        toml::from_str::<toml::Table>(&updated).ok() == Some(expected),
        "Adding [mcp_servers.{NAME}] would change other settings"
    );
    Ok(Some(updated))
}

fn toml_remove(text: &str) -> Result<Option<String>> {
    let before: toml::Table = toml::from_str(text).context("It is not valid TOML")?;
    if before
        .get("mcp_servers")
        .and_then(|servers| servers.get(NAME))
        .is_none()
    {
        return Ok(None);
    }
    let mut updated = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        if let Some(path) = toml_header(line) {
            skipping = path.len() >= 2 && path[0] == "mcp_servers" && path[1] == NAME;
        }
        if !skipping {
            updated.push_str(line);
        }
    }
    if skipping {
        let kept = updated.trim_end().len();
        updated.truncate(kept);
        if !updated.is_empty() {
            updated.push_str(if text.contains("\r\n") { "\r\n" } else { "\n" });
        }
    }
    let mut expected = before;
    if let Some(toml::Value::Table(servers)) = expected.get_mut("mcp_servers") {
        servers.remove(NAME);
    }
    let without_empty_servers = |mut table: toml::Table| {
        if table
            .get("mcp_servers")
            .and_then(toml::Value::as_table)
            .is_some_and(toml::Table::is_empty)
        {
            table.remove("mcp_servers");
        }
        table
    };
    ensure!(
        toml::from_str::<toml::Table>(&updated)
            .ok()
            .map(without_empty_servers)
            == Some(without_empty_servers(expected)),
        "[mcp_servers.{NAME}] is not a plain table there"
    );
    Ok(Some(updated))
}

/// The key path of a `[table]` or `[[array]]` header line.
fn toml_header(line: &str) -> Option<Vec<String>> {
    let line = line.trim_start();
    if !line.starts_with('[') {
        return None;
    }
    let table: toml::Table = toml::from_str(line).ok()?;
    let mut path = Vec::new();
    let mut current = &table;
    loop {
        let mut entries = current.iter();
        let Some((key, value)) = entries.next() else {
            return (!path.is_empty()).then_some(path);
        };
        if entries.next().is_some() {
            return None;
        }
        path.push(key.clone());
        current = match value {
            toml::Value::Table(table) => table,
            toml::Value::Array(items) => match items.as_slice() {
                [toml::Value::Table(table)] => table,
                _ => return None,
            },
            _ => return None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = r"C:\Users\Ada\AppData\Local\Renium\bin\renium.exe";

    fn standard() -> Node {
        Node::Object(vec![
            ("command", Node::Leaf(json!(COMMAND))),
            ("args", Node::Leaf(json!(["mcp"]))),
        ])
    }

    #[test]
    fn a_json_entry_joins_the_existing_servers_in_their_layout_and_leaves_cleanly() {
        let original = "{\r\n    \"theme\": \"dark\", // keep\r\n    \"mcpServers\": {\r\n        \"other\": {\"command\": \"node\"}\r\n    }\r\n}\r\n";
        let added = json_add(original, "mcpServers", standard())
            .unwrap()
            .unwrap();
        assert_eq!(
            added,
            "{\r\n    \"theme\": \"dark\", // keep\r\n    \"mcpServers\": {\r\n        \"other\": {\"command\": \"node\"},\r\n        \"renium\": {\r\n            \"command\": \"C:\\\\Users\\\\Ada\\\\AppData\\\\Local\\\\Renium\\\\bin\\\\renium.exe\",\r\n            \"args\": [\r\n                \"mcp\"\r\n            ]\r\n        }\r\n    }\r\n}\r\n"
        );
        assert!(
            json_add(&added, "mcpServers", standard())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            json_remove(&added, "mcpServers").unwrap().unwrap(),
            original
        );
        assert!(json_remove(original, "mcpServers").unwrap().is_none());
    }

    #[test]
    fn a_json_entry_creates_its_container_or_file() {
        let added = json_add("", "mcpServers", standard()).unwrap().unwrap();
        assert_eq!(
            parse_jsonc_value(&added).unwrap(),
            json!({"mcpServers": {"renium": {"command": COMMAND, "args": ["mcp"]}}})
        );
        assert!(added.starts_with("{\n  \"mcpServers\": {\n    \"renium\": {\n      \"command\""));
        let compact = json_add("{\"model\":\"x\",\"mcp\":{}}", "mcp", standard())
            .unwrap()
            .unwrap();
        assert!(!compact.contains('\n'), "{compact}");
        assert_eq!(
            parse_jsonc_value(&compact).unwrap(),
            json!({"model": "x", "mcp": {"renium": {"command": COMMAND, "args": ["mcp"]}}})
        );
        assert_eq!(
            json_remove(&compact, "mcp").unwrap().unwrap(),
            "{\"model\":\"x\",\"mcp\":{}}"
        );
        assert!(json_add("[1]", "mcp", standard()).is_err());
        assert!(json_add("{\"mcp\": []}", "mcp", standard()).is_err());
    }

    #[test]
    fn a_codex_entry_is_appended_and_removed_without_touching_the_rest() {
        let original = "model = \"gpt-5\"\n# keep\n\n[mcp_servers.other]\ncommand = \"node\"\n";
        let added = toml_add(original, COMMAND).unwrap().unwrap();
        assert!(added.starts_with(original));
        let table: toml::Table = toml::from_str(&added).unwrap();
        assert_eq!(
            table["mcp_servers"][NAME]["command"].as_str(),
            Some(COMMAND)
        );
        assert_eq!(
            table["mcp_servers"][NAME]["tool_timeout_sec"].as_integer(),
            Some(900)
        );
        assert!(toml_add(&added, COMMAND).unwrap().is_none());
        assert_eq!(toml_remove(&added).unwrap().unwrap(), original);
        let followed = format!("{added}\n[projects.x]\ntrust_level = \"trusted\"\n");
        assert_eq!(
            toml_remove(&followed).unwrap().unwrap(),
            format!("{original}\n[projects.x]\ntrust_level = \"trusted\"\n")
        );
        assert!(toml_add("mcp_servers = { other = { command = \"x\" } }\n", COMMAND).is_err());
    }
}
