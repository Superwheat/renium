use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Args;
use serde_json::{Value, json};

use crate::app::output::{ReportedFailure, print_json_output};
use crate::studio::automation::validate_luau_syntax;

#[derive(Args)]
pub(crate) struct CheckArgs {
    /// Script files to parse; use - to read UTF-8 source from stdin
    #[arg(required = true, num_args = 1.., value_name = "FILE")]
    files: Vec<PathBuf>,
}

pub(crate) fn run(args: CheckArgs) -> Result<()> {
    let result = check(&args.files, &mut io::stdin().lock())?;
    print_json_output(&result, false)?;
    if result["ok"] == false {
        return Err(ReportedFailure.into());
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
                stdin.read_to_string(&mut source).map(|_| source)
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
                .contains("Invalid Luau syntax at")
        );
        assert_eq!(result["files"][2]["ok"], false);
        assert_eq!(result["files"][3]["ok"], true);
        assert_eq!(check(&[valid], &mut io::empty()).unwrap()["ok"], true);
        assert!(check(&["-".into(), "-".into()], &mut io::empty()).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
