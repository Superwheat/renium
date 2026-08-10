use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{Map, Value};

use crate::file_io::atomic_write_file;

use super::projection::target_segments;
use super::{
    AdapterSpec, LoadedProject, ScriptExtensionPolicy, load_nested_project, parse_jsonc_value,
};

const SUPPORTED_ADAPTER_FORMATS: &[&str] = &[
    "txt",
    "csv",
    "json",
    "jsonc",
    "toml",
    "yaml",
    "msgpack",
    "markdown",
    "model-json",
    "rbxm",
    "rbxmx",
    "nested-project",
];

pub(super) fn is_supported_adapter_format(format: &str) -> bool {
    SUPPORTED_ADAPTER_FORMATS.contains(&format)
}

pub(super) fn render_adapter(source: &Path, format: &str) -> Result<Vec<u8>> {
    let source_bytes =
        fs::read(source).with_context(|| format!("Failed to read {}", source.display()))?;
    let source_text = |bytes| {
        String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", source.display()))
    };
    let value = match format {
        "txt" => {
            let text = source_text(source_bytes)?;
            return Ok(format!("return {}\n", luau_string(&text)).into_bytes());
        }
        "markdown" => {
            let text = source_text(source_bytes)?;
            let rich_text = markdown_to_roblox_rich_text(&text);
            return Ok(format!("return {}\n", luau_string(&rich_text)).into_bytes());
        }
        "csv" => {
            let text = source_text(source_bytes)?;
            csv_to_value(&text)?
        }
        "json" => serde_json::from_slice(&source_bytes)
            .with_context(|| format!("Invalid JSON in {}", source.display()))?,
        "jsonc" | "model-json" => {
            let text = source_text(source_bytes)?;
            parse_jsonc_value(&text)
                .with_context(|| format!("Invalid JSONC in {}", source.display()))?
        }
        "toml" => {
            let text = source_text(source_bytes)?;
            let parsed: toml::Value = toml::from_str(&text)
                .with_context(|| format!("Invalid TOML in {}", source.display()))?;
            serde_json::to_value(parsed)?
        }
        "yaml" => serde_yaml::from_slice(&source_bytes)
            .with_context(|| format!("Invalid YAML in {}", source.display()))?,
        "msgpack" => rmp_serde::from_slice(&source_bytes)
            .with_context(|| format!("Invalid MessagePack in {}", source.display()))?,
        other => bail!("Unsupported adapter format '{other}'"),
    };
    if format == "model-json" {
        return Ok((serde_json::to_string_pretty(&value)? + "\n").into_bytes());
    }
    let rendered = value_to_luau(&value, 0)?;
    if value_contains_null(&value) {
        return Ok(format!("local null = table.freeze({{}})\nreturn {rendered}\n").into_bytes());
    }
    Ok(format!("return {rendered}\n").into_bytes())
}

fn markdown_to_roblox_rich_text(markdown: &str) -> String {
    fn escape(output: &mut String, text: &str) {
        for character in text.chars() {
            match character {
                '&' => output.push_str("&amp;"),
                '<' => output.push_str("&lt;"),
                '>' => output.push_str("&gt;"),
                '"' => output.push_str("&quot;"),
                '\'' => output.push_str("&apos;"),
                _ => output.push(character),
            }
        }
    }

    fn block_break(output: &mut String, lines: usize) {
        let existing = output
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count();
        for _ in existing..lines {
            output.push('\n');
        }
    }

    fn heading_size(level: HeadingLevel) -> u8 {
        match level {
            HeadingLevel::H1 => 28,
            HeadingLevel::H2 => 24,
            HeadingLevel::H3 => 21,
            HeadingLevel::H4 => 18,
            HeadingLevel::H5 => 16,
            HeadingLevel::H6 => 14,
        }
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let mut output = String::new();
    let mut links = Vec::new();
    let mut lists = Vec::<Option<u64>>::new();
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::End(TagEnd::Paragraph | TagEnd::BlockQuote(_)) => {
                block_break(&mut output, 2);
            }
            Event::Start(Tag::Heading { level, .. }) => {
                block_break(&mut output, 1);
                let _ = write!(&mut output, "<font size=\"{}\"><b>", heading_size(level));
            }
            Event::End(TagEnd::Heading(_)) => {
                output.push_str("</b></font>");
                block_break(&mut output, 2);
            }
            Event::Start(Tag::Emphasis) => output.push_str("<i>"),
            Event::End(TagEnd::Emphasis) => output.push_str("</i>"),
            Event::Start(Tag::Strong) => output.push_str("<b>"),
            Event::End(TagEnd::Strong) => output.push_str("</b>"),
            Event::Start(Tag::Strikethrough) => output.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => output.push_str("</s>"),
            Event::Start(Tag::CodeBlock(kind)) => {
                block_break(&mut output, 1);
                output.push_str("<font face=\"RobotoMono\">");
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    output.push('[');
                    escape(&mut output, &language);
                    output.push_str("]\n");
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                output.push_str("</font>");
                block_break(&mut output, 2);
            }
            Event::Start(Tag::BlockQuote(_)) => {
                block_break(&mut output, 1);
                output.push_str("&gt; ");
            }
            Event::Start(Tag::List(start)) => {
                block_break(&mut output, 1);
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                block_break(&mut output, 1);
            }
            Event::Start(Tag::Item) => {
                block_break(&mut output, 1);
                for _ in 1..lists.len() {
                    output.push_str("  ");
                }
                if let Some(Some(next)) = lists.last_mut() {
                    let _ = write!(&mut output, "{next}. ");
                    *next += 1;
                } else {
                    output.push_str("• ");
                }
            }
            Event::Start(Tag::Paragraph | Tag::Table(_) | Tag::TableHead | Tag::TableRow)
            | Event::End(TagEnd::Item | TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow) => {
                block_break(&mut output, 1)
            }
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                links.push(dest_url.into_string());
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some(url) = links.pop() {
                    output.push_str(" (");
                    escape(&mut output, &url);
                    output.push(')');
                }
            }
            Event::Start(Tag::TableCell) if !output.ends_with('\n') && !output.is_empty() => {
                output.push_str(" | ");
            }
            Event::Text(text) => escape(&mut output, &text),
            Event::Code(text) => {
                output.push_str("<font face=\"RobotoMono\">");
                escape(&mut output, &text);
                output.push_str("</font>");
            }
            Event::Html(html) | Event::InlineHtml(html) => escape(&mut output, &html),
            Event::SoftBreak => output.push('\n'),
            Event::HardBreak => output.push_str("<br />"),
            Event::Rule => {
                block_break(&mut output, 1);
                output.push_str("────────");
                block_break(&mut output, 2);
            }
            Event::FootnoteReference(label) => {
                output.push('[');
                escape(&mut output, &label);
                output.push(']');
            }
            Event::TaskListMarker(checked) => {
                output.push_str(if checked { "☑ " } else { "☐ " });
            }
            Event::InlineMath(value) | Event::DisplayMath(value) => escape(&mut output, &value),
            _ => {}
        }
    }
    output.trim_end_matches('\n').to_string()
}

pub(super) fn validate_adapter_source(source: &Path, format: &str) -> Result<()> {
    if !source.is_file() {
        bail!("Adapter input does not exist: {}", source.display());
    }
    if matches!(format, "rbxm" | "rbxmx") {
        let bytes = fs::read(source)?;
        if !bytes.starts_with(b"<roblox") {
            let kind = if format == "rbxm" {
                "a recognized Roblox model"
            } else {
                "a Roblox XML model"
            };
            bail!("{} is not {kind}", source.display());
        }
    }
    if format == "nested-project" {
        load_nested_project(source)?;
    }
    Ok(())
}

pub(super) fn adapter_format(adapter: &AdapterSpec) -> Result<String> {
    let format = adapter
        .format
        .as_deref()
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            adapter
                .source
                .file_name()
                .and_then(OsStr::to_str)
                .map(|name| {
                    if name.ends_with(".project.json") || name.ends_with(".project.jsonc") {
                        "nested-project".to_string()
                    } else if name.ends_with(".model.json")
                        || name.ends_with(".model.jsonc")
                        || name.ends_with(".model.renium.jsonc")
                    {
                        "model-json".to_string()
                    } else {
                        adapter
                            .source
                            .extension()
                            .and_then(OsStr::to_str)
                            .unwrap_or("")
                            .to_ascii_lowercase()
                    }
                })
        })
        .unwrap_or_default();
    let normalized = match format.as_str() {
        "md" => "markdown",
        "yml" => "yaml",
        "mpk" | "mpack" => "msgpack",
        other => other,
    }
    .to_string();
    if !is_supported_adapter_format(&normalized) {
        bail!(
            "Could not infer a supported format for {}; set format explicitly",
            adapter.source.display()
        );
    }
    Ok(normalized)
}

pub(super) fn adapter_output_path(
    loaded: &LoadedProject,
    adapter: &AdapterSpec,
    format: &str,
) -> Result<Option<PathBuf>> {
    if let Some(output) = adapter.output.as_deref() {
        return Ok(Some(loaded.root.join(output)));
    }
    if !matches!(
        format,
        "json" | "jsonc" | "toml" | "yaml" | "msgpack" | "markdown"
    ) {
        return Ok(None);
    }
    let target = target_segments(&adapter.target)?;
    let extension = match loaded.project.script_extension {
        ScriptExtensionPolicy::Lua => "lua",
        ScriptExtensionPolicy::Preserve | ScriptExtensionPolicy::Luau => "luau",
    };
    let leaf = target
        .last()
        .context("Adapter target must include an instance name")?;
    let parent = target[..target.len().saturating_sub(1)].iter().fold(
        loaded.root.join(&loaded.project.source_root),
        |path, segment| path.join(segment),
    );
    Ok(Some(parent.join(format!(
        "{}{}.{}",
        leaf, loaded.project.export_naming.module_suffix, extension
    ))))
}

pub(super) fn compare_or_write(
    path: &Path,
    bytes: &[u8],
    check: bool,
    changed: &mut Vec<String>,
) -> Result<()> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    changed.push(path.display().to_string());
    if !check {
        atomic_write_file(path, bytes)?;
    }
    Ok(())
}

fn value_to_luau(value: &Value, depth: usize) -> Result<String> {
    if depth > 128 {
        bail!("Adapter data is nested too deeply");
    }
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => {
            const MAX_EXACT_INTEGER: u64 = 9_007_199_254_740_992;
            if value
                .as_u64()
                .is_some_and(|integer| integer > MAX_EXACT_INTEGER)
                || value
                    .as_i64()
                    .is_some_and(|integer| integer.unsigned_abs() > MAX_EXACT_INTEGER)
            {
                bail!("Adapter integer {value} cannot be represented exactly by Luau");
            }
            Ok(value.to_string())
        }
        Value::String(value) => Ok(luau_string(value)),
        Value::Array(values) => {
            let items = values
                .iter()
                .map(|value| value_to_luau(value, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("{{{}}}", items.join(", ")))
        }
        Value::Object(values) => {
            let items = values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "[{}] = {}",
                        luau_string(key),
                        value_to_luau(value, depth + 1)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("{{{}}}", items.join(", ")))
        }
    }
}

fn value_contains_null(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(values) => values.iter().any(value_contains_null),
        Value::Object(values) => values.values().any(value_contains_null),
        _ => false,
    }
}

fn luau_string(value: &str) -> String {
    let mut equals = String::new();
    while value.contains(&format!("]{equals}]")) {
        equals.push('=');
    }
    format!("[{equals}[{value}]{equals}]")
}

fn csv_to_value(text: &str) -> Result<Value> {
    let rows = parse_csv(text)?;
    let Some(headers) = rows.first() else {
        return Ok(Value::Array(Vec::new()));
    };
    let mut output = Vec::new();
    for (row_index, row) in rows.iter().enumerate().skip(1) {
        if row.len() != headers.len() {
            bail!(
                "CSV row {} has {} columns; expected {}",
                row_index + 1,
                row.len(),
                headers.len()
            );
        }
        let object = headers
            .iter()
            .cloned()
            .zip(row.iter().cloned().map(Value::String))
            .collect::<Map<_, _>>();
        output.push(Value::Object(object));
    }
    Ok(Value::Array(output))
}

pub(super) fn localization_csv_to_json(text: &str) -> Result<String> {
    let rows = parse_csv(text)?;
    let Some(headers) = rows.first() else {
        return Ok("[]".to_string());
    };
    let mut entries = Vec::new();
    for row in rows.iter().skip(1) {
        let mut entry = Map::new();
        let mut values = Map::new();
        for (index, header) in headers.iter().enumerate() {
            let value = row.get(index).map_or("", String::as_str);
            if header.is_empty() || value.is_empty() {
                continue;
            }
            match header.as_str() {
                "Key" => {
                    entry.insert("key".to_string(), Value::String(value.to_string()));
                }
                "Source" => {
                    entry.insert("source".to_string(), Value::String(value.to_string()));
                }
                "Context" => {
                    entry.insert("context".to_string(), Value::String(value.to_string()));
                }
                "Example" | "Examples" => {
                    entry.insert("example".to_string(), Value::String(value.to_string()));
                }
                _ => {
                    values.insert(header.clone(), Value::String(value.to_string()));
                }
            }
        }
        if !entry.contains_key("key") && !entry.contains_key("source") {
            continue;
        }
        entry.insert("values".to_string(), Value::Object(values));
        entries.push(Value::Object(entry));
    }
    serde_json::to_string(&entries).context("Failed to encode LocalizationTable contents")
}

pub(super) fn localization_json_to_csv(contents: &str) -> Result<Vec<u8>> {
    let entries = serde_json::from_str::<Vec<Map<String, Value>>>(contents)
        .context("LocalizationTable Contents is not valid JSON")?;
    let mut languages = BTreeSet::new();
    for entry in &entries {
        if let Some(values) = entry.get("values").and_then(Value::as_object) {
            languages.extend(values.keys().cloned());
        }
    }
    let mut headers = vec![
        "Key".to_string(),
        "Source".to_string(),
        "Context".to_string(),
        "Example".to_string(),
    ];
    headers.extend(languages.iter().cloned());
    let mut output = String::new();
    write_csv_row(&mut output, headers.iter().map(String::as_str));
    for entry in entries {
        let values = entry.get("values").and_then(Value::as_object);
        let mut row = vec![
            entry.get("key").and_then(Value::as_str).unwrap_or(""),
            entry.get("source").and_then(Value::as_str).unwrap_or(""),
            entry.get("context").and_then(Value::as_str).unwrap_or(""),
            entry
                .get("example")
                .or_else(|| entry.get("examples"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        ];
        for language in &languages {
            row.push(
                values
                    .and_then(|values| values.get(language))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
        }
        write_csv_row(&mut output, row);
    }
    Ok(output.into_bytes())
}

fn write_csv_row<'a>(output: &mut String, values: impl IntoIterator<Item = &'a str>) {
    let mut first = true;
    for value in values {
        if !first {
            output.push(',');
        }
        first = false;
        if value.contains(',')
            || value.contains('"')
            || value.contains('\n')
            || value.contains('\r')
        {
            output.push('"');
            output.push_str(&value.replace('"', "\"\""));
            output.push('"');
        } else {
            output.push_str(value);
        }
    }
    output.push('\n');
}

fn parse_csv(text: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            other => field.push(other),
        }
    }
    if quoted {
        bail!("CSV ends inside a quoted field");
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}
