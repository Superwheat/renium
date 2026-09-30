use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};

use crate::project::config;
use crate::system::files::atomic_write_file;

const COMMAND_LIMIT: usize = 600;
const OUTPUT_LIMIT: usize = 1500;
const ENTRIES_PER_TRANSCRIPT: usize = 40;
const TRANSCRIPT_FILES: usize = 3;
const LOG_TAIL_LINES: usize = 400;
const CRASH_LIMIT: usize = 5;
const CRASH_BYTES: usize = 64 * 1024;

#[derive(Args)]
pub(crate) struct ReportArgs {
    #[arg(
        short,
        long,
        help = "What went wrong, in your words; goes at the top of the report"
    )]
    pub(crate) message: Option<String>,
    #[arg(
        long,
        default_value_t = 120,
        value_name = "MINUTES",
        help = "How far back to look for transcript entries, crash reports and log lines"
    )]
    pub(crate) since: u64,
    #[arg(
        long,
        help = "Leave out the agent transcript excerpt (the recent rbx commands and their output)"
    )]
    pub(crate) no_transcript: bool,
    #[arg(
        short,
        long,
        value_name = "DIR",
        help = "Folder to write the report into (default: the project's .renium/reports)"
    )]
    pub(crate) output: Option<PathBuf>,
}

struct TranscriptEntry {
    when: String,
    command: String,
    output: String,
}

struct TranscriptExcerpt {
    agent: &'static str,
    file: String,
    entries: Vec<TranscriptEntry>,
}

pub(crate) fn run(args: ReportArgs, global_project: Option<&Path>) -> Result<()> {
    let current = std::env::current_dir().context("Failed to read the current directory")?;
    let project = config::try_load_project(global_project, Some(&current))
        .ok()
        .flatten();
    let root = project
        .as_ref()
        .map_or_else(|| current.clone(), |project| project.root.clone());
    let home = home_directory();
    let since = SystemTime::now() - Duration::from_secs(args.since.max(1) * 60);
    let id = report_id()?;
    let output_root = args.output.unwrap_or_else(|| {
        project.as_ref().map_or_else(
            || app_data_directory().join("reports"),
            |project| project.root.join(".renium").join("reports"),
        )
    });
    let directory = output_root.join(&id);
    fs::create_dir_all(&directory)
        .with_context(|| format!("Failed to create {}", directory.display()))?;
    let mut sections: Vec<String> = Vec::new();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let redact_text = |text: &str| redact(text, home.as_deref());

    let now = SystemTime::now();
    let environment = json!({
        "id": id,
        "generated": utc_stamp(now),
        "version": crate::app::build::VERSION,
        "gitHash": crate::app::build::GIT_HASH,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "executable": std::env::current_exe().ok().map(|path| redact_text(&path.display().to_string())),
        "projectRoot": redact_text(&root.display().to_string()),
        "sinceMinutes": args.since,
    });

    let (doctor, project_path) =
        match crate::project::workflows::doctor_result(&root, global_project) {
            Ok(value) => value,
            Err(error) => (json!({ "error": format!("{error:#}") }), None),
        };
    files.push((
        "doctor.json".into(),
        redact_text(&(serde_json::to_string_pretty(&doctor)? + "\n")).into_bytes(),
    ));

    let studio = studio_status(project.as_ref().map(|project| project.path.as_path()));
    files.push((
        "studio.json".into(),
        redact_text(&(serde_json::to_string_pretty(&studio)? + "\n")).into_bytes(),
    ));

    if let Some(path) = project_path
        .as_deref()
        .or(project.as_ref().map(|project| project.path.as_path()))
        && let Ok(text) = fs::read_to_string(path)
    {
        files.push((
            config::PROJECT_FILE_NAME.into(),
            redact_text(&text).into_bytes(),
        ));
    }

    let daemon_log = crate::app::output::daemon_log_path()
        .map(|path| tail_lines(&path, LOG_TAIL_LINES))
        .unwrap_or_default();
    if !daemon_log.is_empty() {
        files.push(("daemon.log".into(), redact_text(&daemon_log).into_bytes()));
    }

    let mut crashes = Vec::new();
    for directory in [
        root.join(".renium").join("diagnostics").join("crashes"),
        app_data_directory().join("diagnostics").join("crashes"),
    ] {
        crashes.extend(recent_files(&directory, since));
    }
    crashes.sort_by_key(|item| std::cmp::Reverse(item.1));
    crashes.truncate(CRASH_LIMIT);
    for (path, _) in &crashes {
        if let Ok(bytes) = fs::read(path) {
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(CRASH_BYTES)]).into_owned();
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "crash.json".into());
            files.push((format!("crashes/{name}"), redact_text(&text).into_bytes()));
        }
    }

    let transcripts = if args.no_transcript {
        Vec::new()
    } else {
        collect_transcripts(&root, home.as_deref(), since)
    };

    sections.push(format!("# Renium bug report {id}\n"));
    sections.push(format!(
        "Generated {} by Renium {} ({}) on {}/{}.\n",
        environment["generated"].as_str().unwrap_or(""),
        crate::app::build::VERSION,
        crate::app::build::GIT_HASH,
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    sections.push("## What happened\n".into());
    sections.push(match args.message.as_deref().map(str::trim) {
        Some(message) if !message.is_empty() => format!("{}\n", redact_text(message)),
        _ => "(no description given; add one with `rbx report -m \"...\"`)\n".into(),
    });
    sections.push("## Environment\n".into());
    sections.push(format!(
        "- Project root: `{}`\n- Executable: `{}`\n- Studio: {}\n",
        environment["projectRoot"].as_str().unwrap_or(""),
        environment["executable"].as_str().unwrap_or("unknown"),
        redact_text(&summarize_studio(&studio))
    ));
    sections.push("## Doctor\n".into());
    sections.push(redact_text(&summarize_doctor(&doctor)));
    sections.push(format!(
        "## Recent Renium commands from agent transcripts (last {} minutes)\n",
        args.since
    ));
    if args.no_transcript {
        sections.push("(left out with --no-transcript)\n".into());
    } else if transcripts.is_empty() {
        sections.push(
            "(no Codex or Claude Code transcript for this project changed in that window)\n".into(),
        );
    } else {
        let mut transcript_text = String::new();
        for excerpt in &transcripts {
            sections.push(format!(
                "### {} transcript `{}` ({} entries)\n",
                excerpt.agent,
                redact_text(&excerpt.file),
                excerpt.entries.len()
            ));
            for entry in &excerpt.entries {
                let block = format!(
                    "- {} `{}`\n\n  ```\n{}\n  ```\n",
                    entry.when,
                    redact_text(&entry.command).replace('`', "'"),
                    indent(&redact_text(&entry.output), "  ")
                );
                sections.push(block.clone());
                transcript_text.push_str(&block);
            }
        }
        files.push(("transcript.md".into(), transcript_text.into_bytes()));
    }
    sections.push(format!("## Daemon log (last {LOG_TAIL_LINES} lines)\n"));
    sections.push(if daemon_log.is_empty() {
        "(no daemon log; the daemon writes one from Renium 0.3.13 on)\n".into()
    } else {
        format!("```\n{}\n```\n", redact_text(&daemon_log))
    });
    sections.push("## Crash reports\n".into());
    sections.push(if crashes.is_empty() {
        "(none in the window)\n".into()
    } else {
        crashes
            .iter()
            .map(|(path, _)| {
                format!(
                    "- `crashes/{}`\n",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            })
            .collect()
    });
    let report_md = sections.join("\n");
    files.push(("report.md".into(), report_md.into_bytes()));
    files.push((
        "report.json".into(),
        (serde_json::to_string_pretty(&json!({
            "environment": environment,
            "files": files.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
            "transcripts": transcripts.iter().map(|excerpt| json!({
                "agent": excerpt.agent,
                "file": redact_text(&excerpt.file),
                "entries": excerpt.entries.len(),
            })).collect::<Vec<_>>(),
        }))? + "\n")
            .into_bytes(),
    ));

    for (name, bytes) in &files {
        let path = directory.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_file(&path, bytes)?;
    }
    let zip_path = output_root.join(format!("{id}.zip"));
    write_zip(&zip_path, &files)?;

    let issue_url = format!(
        "https://github.com/Superwheat/renium/issues/new?title={}",
        url_encode(&format!(
            "{id}: {}",
            args.message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .map_or("Renium bug report".to_string(), |message| message
                    .chars()
                    .take(80)
                    .collect())
        ))
    );
    crate::app::output::emit_global_output(
        &json!({
            "ok": true,
            "id": id,
            "directory": directory,
            "zip": zip_path,
            "issueUrl": issue_url,
            "files": files.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
        }),
        &format!(
            "Report {id} written to {}\nZip: {}\nReview it (project file, command outputs and log lines are inside; keys and home paths are masked), then open {} and attach the zip, or post it in the Discord server: https://discord.gg/wwTFHSSNn3",
            directory.display(),
            zip_path.display(),
            issue_url
        ),
    )
}

fn studio_status(project: Option<&Path>) -> Value {
    match crate::automation::commands::daemon_result(
        crate::automation::op::STUDIO_STATUS,
        project,
        json!({ "all": false, "bridgeWaitSeconds": 2.0, "bridgePorts": "8781,8782" }),
        false,
        None,
    ) {
        Ok(value) => value,
        Err(error) => json!({ "error": format!("{error:#}") }),
    }
}

fn summarize_studio(studio: &Value) -> String {
    if let Some(error) = studio.get("error").and_then(Value::as_str) {
        return format!("not reachable ({error})");
    }
    let clients = studio
        .get("clients")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let place = studio
        .get("clients")
        .and_then(Value::as_array)
        .and_then(|clients| clients.first())
        .and_then(|client| client.get("placeName"))
        .and_then(Value::as_str)
        .unwrap_or("none");
    format!("{clients} connected runtime(s), first place `{place}`; full status in studio.json")
}

fn summarize_doctor(doctor: &Value) -> String {
    let Some(checks) = doctor.get("checks").and_then(Value::as_array) else {
        return format!(
            "(doctor failed: {})\n",
            doctor
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    };
    let mut text = String::from("| Check | Status | Detail |\n|---|---|---|\n");
    for check in checks {
        text.push_str(&format!(
            "| {} | {} | {} |\n",
            check.get("name").and_then(Value::as_str).unwrap_or(""),
            check.get("status").and_then(Value::as_str).unwrap_or(""),
            check
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("")
                .replace('|', "\\|")
                .replace('\n', " ")
        ));
    }
    text
}

fn collect_transcripts(
    root: &Path,
    home: Option<&str>,
    since: SystemTime,
) -> Vec<TranscriptExcerpt> {
    let Some(home) = home else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let root_key = path_key(&root.display().to_string());
    let since_iso = utc_stamp(since);
    let mut candidates: Vec<(SystemTime, PathBuf, &'static str)> = Vec::new();
    for entry in walk(&home.join(".codex").join("sessions"), 4) {
        if let Some(modified) = modified_after(&entry, since)
            && codex_session_cwd(&entry).is_some_and(|cwd| path_key(&cwd).starts_with(&root_key))
        {
            candidates.push((modified, entry, "Codex"));
        }
    }
    for entry in walk(&home.join(".claude").join("projects"), 2) {
        if let Some(modified) = modified_after(&entry, since)
            && claude_session_cwd(&entry).is_some_and(|cwd| path_key(&cwd).starts_with(&root_key))
        {
            candidates.push((modified, entry, "Claude Code"));
        }
    }
    candidates.sort_by_key(|item| std::cmp::Reverse(item.0));
    candidates.truncate(TRANSCRIPT_FILES);
    candidates
        .into_iter()
        .filter_map(|(_, path, agent)| {
            let entries = if agent == "Codex" {
                codex_entries(&path, &since_iso)
            } else {
                claude_entries(&path, &since_iso)
            };
            (!entries.is_empty()).then(|| TranscriptExcerpt {
                agent,
                file: path.display().to_string(),
                entries,
            })
        })
        .collect()
}

fn walk(directory: &Path, depth: usize) -> Vec<PathBuf> {
    let mut output = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return output;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                output.extend(walk(&path, depth - 1));
            }
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            output.push(path);
        }
    }
    output
}

fn modified_after(path: &Path, since: SystemTime) -> Option<SystemTime> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    (modified >= since).then_some(modified)
}

fn codex_session_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    let value: Value = serde_json::from_str(&first).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    value
        .get("payload")
        .and_then(|payload| payload.get("cwd"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn claude_session_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(200).map_while(Result::ok) {
        if let Some(index) = line.find("\"cwd\":\"")
            && let Ok(value) = serde_json::from_str::<Value>(&line)
            && let Some(cwd) = value.get("cwd").and_then(Value::as_str)
        {
            let _ = index;
            return Some(cwd.to_owned());
        }
    }
    None
}

fn codex_entries(path: &Path, since_iso: &str) -> Vec<TranscriptEntry> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut pending: HashMap<String, (String, String)> = HashMap::new();
    let mut ordered: VecDeque<(String, String)> = VecDeque::new();
    let mut entries: VecDeque<TranscriptEntry> = VecDeque::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let when = value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if when.as_str() < since_iso {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let call_id = payload
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match kind {
            "custom_tool_call" | "function_call" => {
                let text = payload
                    .get("input")
                    .or_else(|| payload.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if mentions_renium(text) {
                    let command = truncate(text, COMMAND_LIMIT);
                    if call_id.is_empty() {
                        ordered.push_back((when, command));
                    } else {
                        pending.insert(call_id, (when, command));
                    }
                }
            }
            "custom_tool_call_output" | "function_call_output" => {
                let output = output_text(payload.get("output"));
                let call = if call_id.is_empty() {
                    ordered.pop_front()
                } else {
                    pending.remove(&call_id)
                };
                if let Some((when, command)) = call {
                    push_entry(&mut entries, when, command, output);
                }
            }
            _ => {}
        }
    }
    entries.into()
}

fn claude_entries(path: &Path, since_iso: &str) -> Vec<TranscriptEntry> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut pending: HashMap<String, (String, String)> = HashMap::new();
    let mut entries: VecDeque<TranscriptEntry> = VecDeque::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("\"tool_use\"") && !line.contains("\"tool_result\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let when = value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if when.as_str() < since_iso {
            continue;
        }
        let Some(content) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let text = input
                        .get("command")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| input.to_string());
                    if mentions_renium(&text)
                        && let Some(id) = block.get("id").and_then(Value::as_str)
                    {
                        pending.insert(
                            id.to_owned(),
                            (when.clone(), truncate(&text, COMMAND_LIMIT)),
                        );
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                        && let Some((when, command)) = pending.remove(id)
                    {
                        push_entry(
                            &mut entries,
                            when,
                            command,
                            output_text(block.get("content")),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    entries.into()
}

fn push_entry(
    entries: &mut VecDeque<TranscriptEntry>,
    when: String,
    command: String,
    output: String,
) {
    if entries.len() >= ENTRIES_PER_TRANSCRIPT {
        entries.pop_front();
    }
    entries.push_back(TranscriptEntry {
        when: when.chars().take(19).collect::<String>().replace('T', " "),
        command,
        output: truncate(&output, OUTPUT_LIMIT),
    });
}

fn output_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| item.as_str().map(str::to_owned))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn mentions_renium(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["rbx ", "rbx.exe", "renium ", "renium.exe"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_owned();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!(
        "{kept}\n[... {} more characters]",
        trimmed.chars().count() - limit
    )
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_owned()
    }
}

fn recent_files(directory: &Path, since: SystemTime) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let modified = modified_after(&path, since)?;
            path.is_file().then_some((path, modified))
        })
        .collect()
}

fn tail_lines(path: &Path, count: usize) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

fn write_zip(path: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    let file =
        fs::File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        writer.start_file(name.replace('\\', "/"), options)?;
        writer.write_all(bytes)?;
    }
    writer.finish()?;
    Ok(())
}

fn report_id() -> Result<String> {
    let suffix: String = crate::automation::authorization::random_id()?
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(6)
        .collect::<String>()
        .to_ascii_uppercase();
    let day = utc_stamp(SystemTime::now())
        .chars()
        .take(10)
        .filter(|character| *character != '-')
        .collect::<String>();
    Ok(format!("RNM-{day}-{suffix}"))
}

fn home_directory() -> Option<String> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| home.to_string_lossy().into_owned())
        .filter(|home| !home.is_empty())
}

fn app_data_directory() -> PathBuf {
    crate::daemon::local_app_data_daemon_path()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
}

const SECRET_KEYWORDS: [&str; 8] = [
    "api_key",
    "api-key",
    "apikey",
    "authorization",
    "bearer",
    "password",
    "secret",
    "token",
];

pub(crate) fn redact(text: &str, home: Option<&str>) -> String {
    let masked = match home.filter(|home| home.len() > 3) {
        Some(home) => mask_home(text, home),
        None => text.to_owned(),
    };
    mask_secrets(&masked)
}

fn is_separator(byte: u8) -> bool {
    byte == b'\\' || byte == b'/'
}

fn mask_home(text: &str, home: &str) -> String {
    let components: Vec<Vec<u8>> = home
        .split(['\\', '/'])
        .filter(|component| !component.is_empty())
        .map(|component| component.to_ascii_lowercase().into_bytes())
        .collect();
    if components.is_empty() {
        return text.to_owned();
    }
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        if let Some(end) = home_match_end(bytes, cursor, &components) {
            output.push('~');
            cursor = end;
            continue;
        }
        let character = text[cursor..].chars().next().unwrap();
        output.push(character);
        cursor += character.len_utf8();
    }
    output
}

fn home_match_end(bytes: &[u8], start: usize, components: &[Vec<u8>]) -> Option<usize> {
    let mut position = start;
    for (index, component) in components.iter().enumerate() {
        if !bytes[position..].starts_with(component) {
            return None;
        }
        position += component.len();
        if index + 1 < components.len() {
            let separators = bytes[position..]
                .iter()
                .take_while(|byte| is_separator(**byte))
                .count();
            if separators == 0 {
                return None;
            }
            position += separators;
        }
    }
    Some(position)
}

fn mask_secrets(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut masked = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let mut matched = None;
        for keyword in SECRET_KEYWORDS {
            if lower[cursor..].starts_with(keyword) {
                matched = Some(keyword.len());
                break;
            }
        }
        let Some(keyword_length) = matched else {
            let character = text[cursor..].chars().next().unwrap();
            masked.push(character);
            cursor += character.len_utf8();
            continue;
        };
        masked.push_str(&text[cursor..cursor + keyword_length]);
        cursor += keyword_length;
        let mut separator_end = cursor;
        while separator_end < text.len()
            && matches!(
                bytes[separator_end],
                b' ' | b'\t' | b'"' | b'\'' | b':' | b'='
            )
        {
            separator_end += 1;
        }
        let mut value_end = separator_end;
        while value_end < text.len()
            && !matches!(
                bytes[value_end],
                b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b',' | b'}' | b']'
            )
        {
            value_end += 1;
        }
        let value = &text[separator_end..value_end];
        if separator_end > cursor && value.len() >= 8 && !value.contains('/') {
            masked.push_str(&text[cursor..separator_end]);
            masked.push_str("[redacted]");
            cursor = value_end;
        }
    }
    masked
}

fn url_encode(text: &str) -> String {
    let mut output = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            b' ' => output.push('+'),
            other => output.push_str(&format!("%{other:02X}")),
        }
    }
    output
}

pub(crate) fn utc_stamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        remainder % 3600 / 60,
        remainder % 60
    )
}

pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_render_utc_dates() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            utc_stamp(UNIX_EPOCH + Duration::from_secs(1_790_500_000)),
            "2026-09-27T09:06:40Z"
        );
    }

    #[test]
    fn redaction_masks_keys_and_home_paths() {
        let text = "ROBLOX_API_KEY=abcdefghijklmnop in C:\\Users\\someone\\proj and c:/users/someone/x plus C:\\\\Users\\\\someone\\\\y and C:\\\\\\\\Users\\\\\\\\SOMEONE\\\\\\\\z, token: \"0123456789abcdef\" short=key=abc";
        let masked = redact(text, Some("C:\\Users\\someone"));
        assert_eq!(
            masked,
            "ROBLOX_API_KEY=[redacted] in ~\\proj and ~/x plus ~\\\\y and ~\\\\\\\\z, token: \"[redacted]\" short=key=abc"
        );
        assert_eq!(
            redact("Users someone", Some("C:\\Users\\someone")),
            "Users someone"
        );
        assert_eq!(
            redact("path key=/usr/local/bin", None),
            "path key=/usr/local/bin"
        );
    }

    #[test]
    fn transcripts_keep_only_renium_commands_with_their_output() {
        let root = crate::tests::support::temp_dir("report-transcripts");
        let codex = root.join("codex.jsonl");
        fs::write(
            &codex,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"E:\\\\proj\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-09-27T05:00:00.000Z\",\"payload\":{\"type\":\"custom_tool_call\",\"call_id\":\"a\",\"input\":\"exec({cmd:\\\"rbx lst\\\"})\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-09-27T05:00:01.000Z\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"a\",\"output\":\"{\\\"ok\\\":true}\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-09-27T05:00:02.000Z\",\"payload\":{\"type\":\"custom_tool_call\",\"call_id\":\"b\",\"input\":\"exec({cmd:\\\"git status\\\"})\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-09-27T05:00:03.000Z\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"b\",\"output\":\"clean\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-09-27T04:00:00.000Z\",\"payload\":{\"type\":\"custom_tool_call\",\"call_id\":\"c\",\"input\":\"rbx old\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-09-27T04:00:01.000Z\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"c\",\"output\":\"too old\"}}\n"
            ),
        )
        .unwrap();
        let entries = codex_entries(&codex, "2026-09-27T04:30:00Z");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].command.contains("rbx lst"));
        assert_eq!(entries[0].output, "{\"ok\":true}");
        assert_eq!(entries[0].when, "2026-09-27 05:00:00");
        assert_eq!(codex_session_cwd(&codex).as_deref(), Some("E:\\proj"));

        let claude = root.join("claude.jsonl");
        fs::write(
            &claude,
            concat!(
                "{\"type\":\"assistant\",\"cwd\":\"E:\\\\proj\\\\sub\",\"timestamp\":\"2026-09-27T05:10:00.000Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{\"command\":\"rbx status\"}}]}}\n",
                "{\"type\":\"user\",\"cwd\":\"E:\\\\proj\\\\sub\",\"timestamp\":\"2026-09-27T05:10:01.000Z\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":[{\"type\":\"text\",\"text\":\"Error: No Studio\"}]}]}}\n"
            ),
        )
        .unwrap();
        let entries = claude_entries(&claude, "2026-09-27T04:30:00Z");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "rbx status");
        assert_eq!(entries[0].output, "Error: No Studio");
        assert!(path_key(&claude_session_cwd(&claude).unwrap()).starts_with(&path_key("E:/proj")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn report_ids_have_a_date_and_six_characters() {
        let id = report_id().unwrap();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts[0], "RNM");
        assert_eq!(parts[1].len(), 8);
        assert_eq!(parts[2].len(), 6);
        assert!(
            parts[2]
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );
    }
}
