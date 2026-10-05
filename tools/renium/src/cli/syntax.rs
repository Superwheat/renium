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
    /// Script files or folders to parse (- reads stdin); with no paths, the Luau files git reports as changed in the project
    #[arg(value_name = "PATH")]
    files: Vec<PathBuf>,
}

pub(crate) fn run(args: CheckArgs, project: Option<&Path>) -> Result<()> {
    let files = if args.files.is_empty() {
        changed_luau_files(project)?
    } else {
        expand_folders(args.files)?
    };
    let result = check(&files, &mut io::stdin().lock())?;
    print_json_output(&result, false)?;
    if result["ok"] == false {
        return Err(ReportedFailure.into());
    }
    Ok(())
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

fn changed_luau_files(project: Option<&Path>) -> Result<Vec<PathBuf>> {
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
        bail!(
            "{} is not in a git repository; pass the files or folders to check",
            scope.display()
        );
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
    let files = changed_luau_paths(&listing, &top);
    if files.is_empty() {
        bail!(
            "git reports no changed .luau or .lua files under {}; pass the files or folders to check",
            scope.display()
        );
    }
    Ok(files)
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
            } else {
                fs::read_to_string(file)
            };
            match source
                .map_err(anyhow::Error::from)
                .and_then(|source| validate_luau_syntax(&source))
            {
                Ok(()) => json!({"file": file, "ok": true}),
                Err(error) => json!({"file": file, "ok": false, "error": format!("{error:#}")}),
            }
        })
        .collect();
    Ok(json!({"ok": results.iter().all(|item| item["ok"] == true), "files": results}))
}

#[cfg(test)]
mod tests {
    use super::*;

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
