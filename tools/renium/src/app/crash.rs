use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::app::build::{GIT_HASH, TIMESTAMP_UNIX, VERSION};
use crate::app::timing::current_millis;
use crate::daemon::local_app_data_daemon_path;
use crate::project::config;
use crate::studio::target::place_filter;

pub(crate) fn install_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("Renium panicked");
        let location = info.location().map(|location| {
            json!({
                "file": location.file(),
                "line": location.line(),
                "column": location.column(),
            })
        });
        let directory = report_directory();
        let _ = fs::create_dir_all(&directory);
        let path = directory.join(format!(
            "crash-{}-{}.json",
            std::process::id(),
            current_millis()
        ));
        let report = json!({
            "schemaVersion": 1,
            "version": VERSION,
            "gitHash": GIT_HASH,
            "buildTimestampUnix": TIMESTAMP_UNIX,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "message": message,
            "location": location,
            "backtrace": std::backtrace::Backtrace::force_capture().to_string(),
            "arguments": std::env::args().collect::<Vec<_>>(),
            "currentDirectory": std::env::current_dir().ok(),
            "thread": std::thread::current().name(),
            "place": place_filter(),
            "daemon": std::env::var("RENIUM_DAEMON_NAME").ok(),
        });
        if let Ok(text) = serde_json::to_vec_pretty(&report) {
            let _ = fs::write(&path, text);
        }
        eprintln!("Renium crashed. Report: {}", path.display());
    }));
}

fn report_directory() -> PathBuf {
    if let Ok(current) = std::env::current_dir()
        && let Ok(project) = config::load_project(None, Some(&current))
    {
        return project
            .root
            .join(".renium")
            .join("diagnostics")
            .join("crashes");
    }
    local_app_data_daemon_path()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("diagnostics")
        .join("crashes")
}
