use std::io;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) fn download_to_file(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "2",
            "--connect-timeout",
            "15",
            "--speed-time",
            "30",
            "--speed-limit",
            "1",
            "--max-time",
            "600",
            "--retry-max-time",
            "600",
            "-o",
        ])
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
    let mut result = json!({
        "skipped": false,
        "command": command,
        "args": args,
        "status": status,
    });
    for (key, value) in [
        ("stdoutTail", tail_text(&stdout, 2000)),
        ("stderrTail", tail_text(&stderr, 2000)),
    ] {
        if !value.is_empty() {
            result[key] = value.into();
        }
    }
    Ok(result)
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
            if !batch_script_exists(command, cwd) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Could not find {command}"),
                ));
            }
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

/// Whether cmd would find the script: a path as given (relative to `cwd`),
/// or a bare name on PATH, and in `cwd` unless
/// `NoDefaultCurrentDirectoryInExePath` is set. `cmd /C` itself always
/// starts, so a missing script would otherwise surface as a failed run.
#[cfg(windows)]
fn batch_script_exists(command: &str, cwd: &Path) -> bool {
    if command.contains(['/', '\\', ':']) {
        return cwd.join(command).is_file();
    }
    let search_cwd = std::env::var_os("NoDefaultCurrentDirectoryInExePath").is_none();
    let path = std::env::var_os("PATH").unwrap_or_default();
    search_cwd
        .then(|| cwd.to_path_buf())
        .into_iter()
        .chain(std::env::split_paths(&path))
        .any(|directory| directory.join(command).is_file())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_tool_is_reported_as_a_failed_launch() {
        let cwd = crate::tests::support::temp_dir("missing-tool");
        let error =
            run_checked_external_tool("wally install", "nowally-xyz-renium", &["install"], &cwd)
                .unwrap_err();
        assert_eq!(
            format!("{error}"),
            "Failed to launch wally install using command \"nowally-xyz-renium\""
        );
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_script_runs_where_cmd_finds_it() {
        let cwd = crate::tests::support::temp_dir("batch-tool");
        std::fs::write(cwd.join("faketool.cmd"), "@echo off\r\nexit /b 3\r\n").unwrap();
        let full = cwd.join("faketool.cmd").to_string_lossy().into_owned();
        let elsewhere = std::env::temp_dir();
        let ran = |tool: &str, cwd: &Path| {
            let error = format!(
                "{}",
                run_checked_external_tool("fake", tool, &[], cwd).unwrap_err()
            );
            assert!(
                error.starts_with("fake failed with exit code 3.")
                    || error == format!("Failed to launch fake using command {tool:?}"),
                "{error}"
            );
            error.starts_with("fake failed")
        };
        assert!(ran(&full, &elsewhere));
        assert!(ran(".\\faketool.cmd", &cwd));
        assert!(!ran("faketool-missing", &cwd));
        let cwd_searched = std::env::var_os("NoDefaultCurrentDirectoryInExePath").is_none();
        assert_eq!(ran("faketool", &cwd), cwd_searched);
        assert_eq!(ran("faketool.cmd", &cwd), cwd_searched);
        let _ = std::fs::remove_dir_all(cwd);
    }
}
