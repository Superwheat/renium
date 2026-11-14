use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};

use crate::app::output::{ReportedFailure, print_json_output};
use crate::studio::automation::validate_luau_syntax;

#[derive(Args)]
pub(crate) struct CheckArgs {
    /// Script files, folders or scripts' unique names (HonkHandler) to parse (- reads stdin); with no paths, the Luau files git reports as changed in the project, or every Luau file of a project that is not in git
    #[arg(value_name = "PATH")]
    files: Vec<PathBuf>,
}

pub(crate) fn run(args: CheckArgs, project: Option<&Path>) -> Result<()> {
    let mut whole_project = false;
    let files = if args.files.is_empty() {
        let (files, in_git) = default_luau_files(project)?;
        if files.is_empty() {
            let note = if in_git {
                "git reports no changed .luau or .lua files; pass files or folders to check specific ones"
            } else {
                "the project has no .luau or .lua files"
            };
            return print_json_output(&json!({"ok": true, "checked": 0, "note": note}), false);
        }
        whole_project = !in_git;
        files
    } else {
        expand_folders(named_scripts(args.files, project)?)?
    };
    let result = check(&files, &mut io::stdin().lock())?;
    let mut printed = result.clone();
    if crate::app::output::compact_text_output() {
        compact_check_result(&mut printed);
    }
    if whole_project {
        printed["note"] = json!("no git repository here: checked every script of the project");
    }
    print_json_output(&printed, false)?;
    if result["ok"] == false {
        return Err(ReportedFailure.into());
    }
    Ok(())
}

/// A path that is no file or folder becomes the file of the one script that
/// goes by that name in the project (`HonkHandler` for
/// `src/ServerScriptService/HonkHandler.server.luau`). `-` and a name no
/// script goes by stay as they are.
fn named_scripts(files: Vec<PathBuf>, project: Option<&Path>) -> Result<Vec<PathBuf>> {
    files
        .into_iter()
        .map(|file| {
            if file == Path::new("-") || file.exists() {
                return Ok(file);
            }
            let named = crate::automation::tools::project_script_by_name(project, &file)?;
            Ok(named.unwrap_or(file))
        })
        .collect()
}

fn is_luau_source(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("luau") || extension.eq_ignore_ascii_case("lua")
        })
}

fn expand_folders(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        if !path.is_dir() {
            files.push(path);
            continue;
        }
        let mut found = walkdir::WalkDir::new(&path)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
            })
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file() && is_luau_source(entry.path()))
            .map(walkdir::DirEntry::into_path)
            .collect::<Vec<_>>();
        if found.is_empty() {
            bail!("No .luau or .lua files under {}", path.display());
        }
        found.sort();
        files.extend(found);
    }
    Ok(files)
}

/// The files `rbx ck` checks when it is given none, and whether git chose
/// them: the changed Luau files in a git repository, every script of the
/// project where git has nothing to say.
fn default_luau_files(project: Option<&Path>) -> Result<(Vec<PathBuf>, bool)> {
    let scope = match crate::project::config::try_resolve_project_path(project, None)? {
        Some(project) => project
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        None => std::env::current_dir().context("Failed to read the current directory")?,
    };
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&scope)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| output.stdout)
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        return Ok((project_luau_files(project, &scope)?, false));
    };
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let listing = git(&[
        "status",
        "--porcelain",
        "-z",
        "--untracked-files=all",
        "--",
        ".",
    ])
    .context("git status failed; pass the files or folders to check")?;
    Ok((changed_luau_paths(&listing, &top), true))
}

/// Without git nothing says which files changed, so the whole project is
/// checked: every Luau file under its source folder, the paths its tree
/// names and its mounts.
fn project_luau_files(project: Option<&Path>, scope: &Path) -> Result<Vec<PathBuf>> {
    use crate::project::config;
    let Some(loaded) = config::try_load_project(project, None)? else {
        bail!(
            "{} is not in a git repository and has no Renium project file; pass the files or folders to check",
            scope.display()
        );
    };
    let mut roots = vec![loaded.root.join(&loaded.project.source_root)];
    roots.extend(
        config::project_tree_nodes(&loaded.project.tree)
            .into_iter()
            .filter_map(|(_, node)| node.path.map(|path| loaded.root.join(path))),
    );
    roots.extend(
        loaded
            .project
            .mounts
            .iter()
            .map(|mount| loaded.root.join(&mount.source)),
    );
    let mut files = BTreeSet::new();
    for root in roots {
        if root.is_dir() {
            files.extend(
                walkdir::WalkDir::new(&root)
                    .into_iter()
                    .filter_entry(|entry| {
                        entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
                    })
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_file() && is_luau_source(entry.path()))
                    .map(walkdir::DirEntry::into_path),
            );
        } else if root.is_file() && is_luau_source(&root) {
            files.insert(root);
        }
    }
    Ok(files.into_iter().collect())
}

fn changed_luau_paths(listing: &[u8], top: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries = listing.split(|byte| *byte == 0);
    while let Some(entry) = entries.next() {
        let (Some(status), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        if status.iter().any(|code| matches!(code, b'R' | b'C')) {
            entries.next();
        }
        let path = top.join(String::from_utf8_lossy(path).as_ref());
        if !status.contains(&b'D') && status != b"!!" && is_luau_source(&path) {
            files.push(path);
        }
    }
    files
}

/// What the offline check finds wrong with a script.
pub(crate) struct SyntaxFailure {
    /// The message `rbx ck` prints.
    pub(crate) message: String,
    /// The line the compiler names, when it names one.
    pub(crate) line: Option<usize>,
    /// The compiler's text after that line, or the whole message without one.
    pub(crate) reason: String,
}

/// The syntax error of Luau source as `rbx ck` reports it, or None when the
/// source parses.
pub(crate) fn syntax_failure(source: &str) -> Option<SyntaxFailure> {
    let error = validate_luau_syntax(source).err()?;
    let message = format!("{error:#}");
    let located = error.chain().find_map(|cause| {
        let mlua::Error::SyntaxError { message, .. } = cause.downcast_ref::<mlua::Error>()? else {
            return None;
        };
        let (line, reason) = message.split_once(':')?;
        Some((line.trim().parse().ok()?, reason.trim().to_string()))
    });
    Some(match located {
        Some((line, reason)) => SyntaxFailure {
            message,
            line: Some(line),
            reason,
        },
        None => SyntaxFailure {
            reason: message.clone(),
            message,
            line: None,
        },
    })
}

/// The script files a push names are checked as `rbx ck` checks them, so a
/// script that does not parse stops the push before Studio is asked. A
/// leading byte order mark is not part of the text that is checked; the file
/// keeps it.
pub(crate) fn ensure_script_files_parse(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        if !is_luau_source(path) || !path.is_file() {
            continue;
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("Cannot read {} as UTF-8 text", path.display()))?;
        if let Some(failure) = syntax_failure(source.trim_start_matches('\u{feff}')) {
            bail!(
                "{}: {}; nothing was pushed (--no-check pushes a script that holds other text)",
                path.display(),
                failure.message
            );
        }
    }
    Ok(())
}

fn check(files: &[PathBuf], stdin: &mut impl Read) -> Result<Value> {
    if files
        .iter()
        .filter(|file| file.as_path() == Path::new("-"))
        .count()
        > 1
    {
        bail!("Use - only once to read source from stdin");
    }
    let results: Vec<_> = files
        .iter()
        .map(|file| {
            let source = if file == Path::new("-") {
                let mut source = String::new();
                stdin
                    .read_to_string(&mut source)
                    .map(|_| source.trim_start_matches('\u{feff}').to_string())
                    .map_err(anyhow::Error::from)
            } else {
                fs::read_to_string(file)
                    .with_context(|| format!("Cannot read {} as UTF-8 text", file.display()))
            };
            match source.and_then(|source| validate_luau_syntax(&source)) {
                Ok(()) => json!({"file": file, "ok": true}),
                Err(error) => json!({"file": file, "ok": false, "error": format!("{error:#}")}),
            }
        })
        .collect();
    Ok(json!({"ok": results.iter().all(|item| item["ok"] == true), "files": results}))
}

/// Text mode counts the files that passed and lists only the failing ones.
fn compact_check_result(result: &mut Value) {
    let Some(files) = result.get("files").and_then(Value::as_array).cloned() else {
        return;
    };
    let failing = files
        .iter()
        .filter(|file| file["ok"] != true)
        .cloned()
        .collect::<Vec<_>>();
    result["checked"] = json!(files.len());
    if failing.is_empty() {
        result.as_object_mut().map(|map| map.remove("files"));
    } else {
        result["files"] = json!(failing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_is_checked_by_the_name_it_alone_goes_by() {
        let root = crate::tests::support::temp_dir("syntax-names");
        let project = root.join("renium.project.jsonc");
        fs::write(&project, "{\n  \"schemaVersion\": 1\n}\n").unwrap();
        for path in [
            "src/Server/Honk.server.luau",
            "src/A/Util.luau",
            "src/B/Util.luau",
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "return 1").unwrap();
        }
        let honk = root.join("src").join("Server").join("Honk.server.luau");
        let named = |name: &str| named_scripts(vec![name.into()], Some(&project));
        let honk_alone = std::slice::from_ref(&honk);
        assert_eq!(named("Honk").unwrap(), honk_alone);
        assert_eq!(named(&honk.to_string_lossy()).unwrap(), honk_alone);
        assert_eq!(named("-").unwrap(), [PathBuf::from("-")]);
        assert_eq!(named("Nope").unwrap(), [PathBuf::from("Nope")]);
        let shared = named("Util").unwrap_err().to_string();
        assert!(
            shared.starts_with("Util names 2 scripts: src/A/Util.luau, src/B/Util.luau"),
            "{shared}"
        );
        let result = check(&named("Honk").unwrap(), &mut io::empty()).unwrap();
        assert_eq!(result["ok"], true);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checks_folders_recursively_and_git_changes_without_paths() {
        let root = crate::tests::support::temp_dir("syntax-folders");
        for (path, source) in [
            ("src/a.luau", "return 1"),
            ("src/nested/b.lua", "local x ="),
            ("src/notes.txt", "not code"),
            ("src/.history/old.luau", "local broken ="),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        let files = expand_folders(vec![root.join("src"), "-".into()]).unwrap();
        assert_eq!(
            files,
            [
                root.join("src").join("a.luau"),
                root.join("src").join("nested").join("b.lua"),
                PathBuf::from("-"),
            ]
        );
        let result = check(&files, &mut "return 2".as_bytes()).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["files"][0]["ok"], true);
        assert_eq!(result["files"][1]["ok"], false);
        assert_eq!(result["files"][2]["ok"], true);
        fs::create_dir_all(root.join("empty")).unwrap();
        assert!(expand_folders(vec![root.join("empty")]).is_err());
        fs::remove_dir_all(root).unwrap();

        let top = Path::new("repo");
        let listing = b" M src/a.luau\0?? src/new.lua\0A  src/b.luau\0R  src/renamed.luau\0src/old.luau\0 D src/gone.luau\0D  src/staged-gone.luau\0M  README.md\0AM src/c.LUAU\0";
        assert_eq!(
            changed_luau_paths(listing, top),
            [
                "src/a.luau",
                "src/new.lua",
                "src/b.luau",
                "src/renamed.luau",
                "src/c.LUAU"
            ]
            .map(|path| top.join(path))
        );
        assert!(changed_luau_paths(b"", top).is_empty());
    }

    #[test]
    fn rejects_native_luau_statement_ambiguity_without_execution() {
        let invalid = "error('must not execute')\n(target :: any).Value = 1";
        let result = check(&["-".into()], &mut invalid.as_bytes()).unwrap();
        assert_eq!(result["ok"], false);
        assert!(
            result["files"][0]["error"]
                .as_str()
                .unwrap()
                .contains("Ambiguous syntax")
        );
        let valid = "error('must not execute');\n(target :: any).Value = 1";
        assert_eq!(
            check(&["-".into()], &mut valid.as_bytes()).unwrap()["ok"],
            true
        );
        assert!(crate::studio::automation::cooperative_luau(invalid).is_err());
        assert_eq!(
            crate::studio::automation::cooperative_luau(valid).unwrap(),
            valid
        );
    }

    #[test]
    fn nested_loop_instrumentation_does_not_overflow_a_request_thread() {
        let source = format!(
            "--!strict\nlocal function f() {}return 1 {} end; return f()",
            "for i = 1, 2 do ".repeat(12),
            "end ".repeat(12)
        );
        let result = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let output = crate::studio::automation::cooperative_luau(&source).unwrap();
                assert!(output.starts_with("--!strict\n"));
                assert_eq!(output.matches("__reniumCooperate();").count(), 12);
                validate_luau_syntax(&output).unwrap();
                let excessive = format!("{}return 1{}", "do ".repeat(2000), " end".repeat(2000));
                assert!(crate::studio::automation::cooperative_luau(&excessive).is_err());
            })
            .unwrap();
        result.join().unwrap();
    }

    #[test]
    fn instrumentation_preserves_loop_trivia_and_directives() {
        let source = "--!strict\nwhile false do-- preserve this comment\n break end\nrepeat-- repeat comment\n break until true";
        let output = crate::studio::automation::cooperative_luau(source).unwrap();
        assert!(output.starts_with("--!strict\n"));
        assert!(output.contains("__reniumCooperate();-- preserve this comment"));
        assert!(output.contains("__reniumCooperate();-- repeat comment"));
        validate_luau_syntax(&output).unwrap();
    }

    #[test]
    fn checks_luau_without_execution_and_reports_all_files() {
        let root = crate::tests::support::temp_dir("offline-syntax");
        let valid = root.join("valid.luau");
        let invalid = root.join("invalid.luau");
        fs::write(
            &valid,
            "local n: number = if true then 1 else 2\nerror(`must not execute {n}`)",
        )
        .unwrap();
        fs::write(&invalid, "local n =").unwrap();
        let missing = root.join("missing.luau");
        let result = check(
            &[valid.clone(), invalid, missing, "-".into()],
            &mut "return 1".as_bytes(),
        )
        .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["files"][0]["ok"], true);
        assert!(
            result["files"][1]["error"]
                .as_str()
                .unwrap()
                .contains("Invalid Luau syntax")
        );
        assert_eq!(result["files"][2]["ok"], false);
        assert_eq!(result["files"][3]["ok"], true);
        assert_eq!(check(&[valid], &mut io::empty()).unwrap()["ok"], true);
        assert!(check(&["-".into(), "-".into()], &mut io::empty()).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    #[test]
    fn text_mode_lists_only_failing_files() {
        let mut passing = json!({"ok": true, "files": [{"file": "a.luau", "ok": true}, {"file": "b.luau", "ok": true}]});
        compact_check_result(&mut passing);
        assert_eq!(passing, json!({"ok": true, "checked": 2}));
        let mut failing = json!({"ok": false, "files": [{"file": "a.luau", "ok": true}, {"file": "b.luau", "ok": false, "error": "x"}]});
        compact_check_result(&mut failing);
        assert_eq!(
            failing,
            json!({"ok": false, "checked": 2, "files": [{"file": "b.luau", "ok": false, "error": "x"}]})
        );
    }
}
