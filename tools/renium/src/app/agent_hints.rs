//! A short Renium note in the global instruction files of installed agents,
//! so an agent that has never seen a Renium project learns that `rbx`
//! exists and how to start. Per-project guides take over after `rbx init`.
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

const START: &str = "<!-- renium:start -->";
const END: &str = "<!-- renium:end -->";

pub(crate) struct AgentHintTarget {
    pub(crate) agent: &'static str,
    pub(crate) path: PathBuf,
}

fn hint_block(version: &str) -> String {
    format!(
        "{START}\n\
Renium {version} is installed. `rbx` is a terminal tool for Roblox Studio: two-way sync between a project folder and the open place, live Luau, saved-data queries, playtests and captures. \
In a Roblox project folder run `rbx init` once, then read the RENIUM.md it creates and follow it. \
For an existing place file, run `rbx init` in its folder and then `rbx so FILE`. `rbx --help` lists commands. \
`rbx` reports \"No Renium project\" outside a project folder; it never creates one by itself.\n\
{END}\n"
    )
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
}

/// Instruction files of agents that are installed for this user. An agent is
/// installed when its home-level folder exists; the file is created when
/// missing because these agents read it whenever it is present.
pub(crate) fn targets_in(home: &Path) -> Vec<AgentHintTarget> {
    let candidates: [(&str, &str, &str); 5] = [
        ("Claude Code", ".claude", ".claude/CLAUDE.md"),
        ("Codex", ".codex", ".codex/AGENTS.md"),
        ("Gemini CLI", ".gemini", ".gemini/GEMINI.md"),
        ("OpenCode", ".config/opencode", ".config/opencode/AGENTS.md"),
        (
            "Windsurf",
            ".codeium/windsurf",
            ".codeium/windsurf/memories/global_rules.md",
        ),
    ];
    candidates
        .into_iter()
        .filter(|(_, marker, _)| home.join(marker).is_dir())
        .map(|(agent, _, file)| AgentHintTarget {
            agent,
            path: home.join(file),
        })
        .collect()
}

pub(crate) fn targets() -> Vec<AgentHintTarget> {
    home_directory()
        .map(|home| targets_in(&home))
        .unwrap_or_default()
}

fn strip_block(text: &str) -> String {
    let Some(start) = text.find(START) else {
        return text.to_string();
    };
    let Some(end) = text[start..].find(END) else {
        return text.to_string();
    };
    let after = text[start + end + END.len()..].trim_start_matches(['\r', '\n']);
    let mut before = text[..start].to_string();
    while before.ends_with('\n') || before.ends_with('\r') {
        before.pop();
    }
    if before.is_empty() {
        after.to_string()
    } else if after.trim().is_empty() {
        format!("{before}\n")
    } else {
        format!("{before}\n\n{after}")
    }
}

fn with_block(text: &str, version: &str) -> String {
    let base = strip_block(text);
    let block = hint_block(version);
    if base.trim().is_empty() {
        return block;
    }
    let mut result = base;
    while result.ends_with('\n') || result.ends_with('\r') {
        result.pop();
    }
    format!("{result}\n\n{block}")
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let crlf = fs::read_to_string(path).is_ok_and(|current| current.contains("\r\n"));
    let bytes = if crlf {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    };
    fs::write(path, bytes).with_context(|| format!("Failed to write {}", path.display()))
}

/// Writes or refreshes the note for every installed agent and returns the
/// files that changed.
pub(crate) fn install_in(home: &Path, version: &str) -> Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for target in targets_in(home) {
        let current = fs::read_to_string(&target.path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        let wanted = with_block(&current, version);
        if current != wanted {
            write_text(&target.path, &wanted)?;
            changed.push(target.path);
        }
    }
    Ok(changed)
}

pub(crate) fn install(version: &str) -> Result<Vec<PathBuf>> {
    match home_directory() {
        Some(home) => install_in(&home, version),
        None => Ok(Vec::new()),
    }
}

/// Removes the note; a file that held nothing else is deleted.
pub(crate) fn remove_in(home: &Path) -> Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for target in targets_in(home) {
        let Ok(current) = fs::read_to_string(&target.path) else {
            continue;
        };
        if !current.contains(START) {
            continue;
        }
        let stripped = strip_block(&current.replace("\r\n", "\n"));
        if stripped.trim().is_empty() {
            fs::remove_file(&target.path)
                .with_context(|| format!("Failed to remove {}", target.path.display()))?;
        } else {
            write_text(&target.path, &stripped)?;
        }
        changed.push(target.path);
    }
    Ok(changed)
}

pub(crate) fn remove() -> Result<Vec<PathBuf>> {
    match home_directory() {
        Some(home) => remove_in(&home),
        None => Ok(Vec::new()),
    }
}

pub(crate) fn status() -> Value {
    Value::Array(
        targets()
            .into_iter()
            .map(|target| {
                let present = fs::read_to_string(&target.path)
                    .is_ok_and(|text| text.contains(START) && text.contains(END));
                json!({"agent": target.agent, "path": target.path, "present": present})
            })
            .collect(),
    )
}

pub(crate) fn describe(paths: &[PathBuf]) -> String {
    match paths.len() {
        0 => "no agent instruction files needed a Renium note".to_string(),
        _ => format!(
            "Renium note written to {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchHome(PathBuf);

    impl ScratchHome {
        fn new() -> Result<Self> {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("renium-agent-hints-{}-{stamp}", std::process::id()));
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn notes_are_added_once_refreshed_on_change_and_removed_cleanly() -> Result<()> {
        let home = ScratchHome::new()?;
        fs::create_dir_all(home.path().join(".claude"))?;
        fs::create_dir_all(home.path().join(".codex"))?;
        fs::write(
            home.path().join(".codex/AGENTS.md"),
            "Be brief.\r\nUse the memory skill.\r\n",
        )?;
        let changed = install_in(home.path(), "1.0.0")?;
        assert_eq!(changed.len(), 2);
        let codex = fs::read_to_string(home.path().join(".codex/AGENTS.md"))?;
        assert!(
            codex
                .starts_with("Be brief.\r\nUse the memory skill.\r\n\r\n<!-- renium:start -->\r\n")
        );
        assert!(codex.contains("Renium 1.0.0 is installed"));
        assert!(fs::read_to_string(home.path().join(".claude/CLAUDE.md"))?.starts_with(START));
        assert!(!home.path().join(".gemini/GEMINI.md").exists());

        assert!(install_in(home.path(), "1.0.0")?.is_empty());
        let changed = install_in(home.path(), "1.1.0")?;
        assert_eq!(changed.len(), 2);
        let codex = fs::read_to_string(home.path().join(".codex/AGENTS.md"))?;
        assert_eq!(codex.matches(START).count(), 1);
        assert!(codex.contains("Renium 1.1.0 is installed"));

        let removed = remove_in(home.path())?;
        assert_eq!(removed.len(), 2);
        assert_eq!(
            fs::read_to_string(home.path().join(".codex/AGENTS.md"))?,
            "Be brief.\r\nUse the memory skill.\r\n"
        );
        assert!(!home.path().join(".claude/CLAUDE.md").exists());
        assert!(remove_in(home.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn a_note_in_the_middle_of_a_file_is_replaced_once() {
        let text = format!("top\n\n{}\n\nbottom\n", hint_block("0.1").trim_end());
        let updated = with_block(&text, "0.2");
        assert!(updated.contains("Renium 0.2 is installed"));
        assert!(!updated.contains("Renium 0.1 is installed"));
        assert_eq!(updated.matches(START).count(), 1);
        assert_eq!(strip_block(&text), "top\n\nbottom\n");
    }
}
