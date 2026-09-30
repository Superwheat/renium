use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Studio 0.741 asks about a place's auto-recovery files in a dialog one of its
/// own plugins draws, which neither UI Automation nor the macOS accessibility
/// tree can reach. Before Renium opens a place, its recovery files are moved to
/// `AutoSaves/Archived` under the names Studio's Ignore gives them, so the
/// dialog has nothing to ask about and the files stay recoverable.
pub(crate) fn set_aside_recovery_files(place_name: &str) {
    let Some(directory) = auto_saves_directory() else {
        return;
    };
    match archive_recovery_files(&directory, place_name, SystemTime::now()) {
        Ok(0) => {}
        Ok(moved) => crate::app::output::log_global(
            2,
            format_args!(
                "[renium] Set aside {moved} Studio recovery file(s) for {place_name} in {}",
                directory.join("Archived").display()
            ),
        ),
        Err(error) => crate::app::output::log_global(
            2,
            format_args!(
                "[renium] Could not set aside Studio recovery files for {place_name}: {error:#}"
            ),
        ),
    }
}

fn auto_saves_directory() -> Option<PathBuf> {
    let studio = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Roblox")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library")
            .join("Application Support")
            .join("Roblox")
    } else {
        return None;
    };
    Some(studio.join("RobloxStudio").join("AutoSaves"))
}

fn archive_recovery_files(directory: &Path, place_name: &str, now: SystemTime) -> Result<usize> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", directory.display()));
        }
    };
    let archive = directory.join("Archived");
    let stamp = archive_stamp(now);
    let mut moved = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(stem) = file_name
            .to_str()
            .and_then(|name| recovery_file_stem(name, place_name))
        else {
            continue;
        };
        fs::create_dir_all(&archive)
            .with_context(|| format!("Failed to create {}", archive.display()))?;
        let target = archive.join(format!("{stem}_{stamp}.rbxl"));
        fs::rename(entry.path(), &target).with_context(|| {
            format!(
                "Failed to move {} to {}",
                entry.path().display(),
                target.display()
            )
        })?;
        moved += 1;
    }
    Ok(moved)
}

/// Studio names a place's recovery files `<place>_AutoRecovery_<n>.rbxl`.
fn recovery_file_stem<'a>(file_name: &'a str, place_name: &str) -> Option<&'a str> {
    let stem = file_name.strip_suffix(".rbxl")?;
    let counter = stem
        .get(..place_name.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(place_name))
        .and_then(|_| stem[place_name.len()..].strip_prefix("_AutoRecovery_"))?;
    (!counter.is_empty() && counter.bytes().all(|byte| byte.is_ascii_digit())).then_some(stem)
}

fn archive_stamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let (year, month, day) = crate::app::report::civil_from_days((seconds / 86_400) as i64);
    let remainder = seconds % 86_400;
    format!(
        "{year:04}{month:02}{day:02}_{:02}{:02}{:02}",
        remainder / 3600,
        remainder % 3600 / 60,
        remainder % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_files_match_only_their_place() {
        assert_eq!(
            recovery_file_stem("copycopy_AutoRecovery_7.rbxl", "copycopy"),
            Some("copycopy_AutoRecovery_7")
        );
        assert_eq!(
            recovery_file_stem("CopyCopy_AutoRecovery_0.rbxl", "copycopy"),
            Some("CopyCopy_AutoRecovery_0")
        );
        for name in [
            "copycopy2_AutoRecovery_0.rbxl",
            "copy_AutoRecovery_0.rbxl",
            "copycopy_AutoRecovery_.rbxl",
            "copycopy_AutoRecovery_0_20260929_123545.rbxl",
            "copycopy_AutoRecovery_0.rbxlx",
            "copycopy.rbxl",
        ] {
            assert_eq!(recovery_file_stem(name, "copycopy"), None, "{name}");
        }
    }

    #[test]
    fn recovery_files_move_to_studio_archive_names() {
        let directory = std::env::temp_dir().join(format!(
            "renium-recovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        for name in [
            "place_AutoRecovery_0.rbxl",
            "place_AutoRecovery_3.rbxl",
            "other_AutoRecovery_0.rbxl",
        ] {
            fs::write(directory.join(name), name).unwrap();
        }
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_790_730_945);
        assert_eq!(archive_recovery_files(&directory, "place", now).unwrap(), 2);
        let archived = directory.join("Archived");
        assert_eq!(
            fs::read_to_string(archived.join("place_AutoRecovery_3_20260930_011545.rbxl")).unwrap(),
            "place_AutoRecovery_3.rbxl"
        );
        assert!(
            archived
                .join("place_AutoRecovery_0_20260930_011545.rbxl")
                .is_file()
        );
        assert!(directory.join("other_AutoRecovery_0.rbxl").is_file());
        assert!(!directory.join("place_AutoRecovery_0.rbxl").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
