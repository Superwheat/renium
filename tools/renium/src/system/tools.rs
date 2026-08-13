use std::io;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) fn download_to_file(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "2", "--connect-timeout", "15", "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .context("Failed to run curl")?;
    if !status.success() {
        bail!("Downloading {url} failed with {status}");
    }
    Ok(())
}

pub(crate) fn run_git_checked(git_path: &str, args: &[String], cwd: &Path) -> Result<String> {
    let (_, output) = run_external_tool_output(git_path, args, cwd)
        .with_context(|| format!("Failed to launch git using command {git_path:?}"))?;
    if !output.status.success() {
        bail!(
            "git {} failed with exit code {}.\n{}",
            args.join(" "),
            output
                .status
                .code()
                .map_or_else(|| "unknown".to_string(), |code| code.to_string()),
            tail_text(&String::from_utf8_lossy(&output.stderr), 2000)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn run_checked_external_tool(
    label: &str,
    tool: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<Value> {
    let args = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    run_checked_external_tool_strings(label, tool, &args, cwd)
}

fn run_checked_external_tool_strings(
    label: &str,
    tool: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Value> {
    let (command, output) = run_external_tool_output(tool, args, cwd)
        .with_context(|| format!("Failed to launch {label} using command {tool:?}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let status = output.status.code();
    if !output.status.success() {
        bail!(
            "{label} failed with exit code {}.\nstdout:\n{}\nstderr:\n{}",
            status.map_or_else(|| "unknown".to_string(), |value| value.to_string()),
            tail_text(&stdout, 4000),
            tail_text(&stderr, 4000)
        );
    }
    Ok(json!({
        "skipped": false,
        "command": command,
        "args": args,
        "status": status,
        "stdoutTail": tail_text(&stdout, 2000),
        "stderrTail": tail_text(&stderr, 2000),
    }))
}

fn run_external_tool_output(
    tool: &str,
    args: &[String],
    cwd: &Path,
) -> io::Result<(String, Output)> {
    let mut last_error = None;
    for candidate in external_tool_candidates(tool) {
        match run_external_tool_candidate(&candidate, args, cwd) {
            Ok(output) => return Ok((candidate, output)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Could not find command {tool}"),
        )
    }))
}

fn external_tool_candidates(tool: &str) -> Vec<String> {
    #[cfg(not(windows))]
    {
        vec![tool.to_string()]
    }
    #[cfg(windows)]
    {
        let mut candidates = vec![tool.to_string()];
        let path = Path::new(tool);
        let has_separator = tool.contains('/') || tool.contains('\\');
        if !has_separator && path.extension().is_none() {
            for suffix in [".exe", ".cmd", ".bat"] {
                candidates.push(format!("{tool}{suffix}"));
            }
        }
        candidates
    }
}

fn run_external_tool_candidate(command: &str, args: &[String], cwd: &Path) -> io::Result<Output> {
    #[cfg(windows)]
    {
        let lower = command.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return Command::new("cmd")
                .arg("/C")
                .arg(command)
                .args(args)
                .current_dir(cwd)
                .output();
        }
    }
    Command::new(command).args(args).current_dir(cwd).output()
}

fn tail_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    trimmed
        .chars()
        .skip(count.saturating_sub(max_chars))
        .collect()
}
