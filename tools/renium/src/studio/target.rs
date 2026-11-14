use std::path::{Path, PathBuf};

use crate::app::context;
use crate::studio::bridge::BridgeInfoPayload;
use crate::system::files::place_file_path;

pub(crate) fn set_place_filter(value: Option<String>) {
    context::set_place_selector(value);
}

pub(crate) fn place_filter() -> Option<String> {
    context::place_selector()
}

/// The selector as Studios are matched against it. Studio reports a local
/// place by its full path, so a selector that is a path to a place file
/// (`..\Place.rbxl`, `places/a.rbxl`, a path with 8.3 names) becomes that full
/// path, read from the working directory. A bare file name, a place name and
/// ids stay as written.
pub(crate) fn resolved_place_selector(selector: &str) -> String {
    let trimmed = selector.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.ends_with(".rbxl") || lower.ends_with(".rbxlx")) || !trimmed.contains(['/', '\\']) {
        return selector.to_string();
    }
    place_file_path(Path::new(trimmed))
        .to_string_lossy()
        .into_owned()
}

/// The place file the selector names, when it names one that exists: what
/// `rbx so` opens when it is given no file.
pub(crate) fn pinned_place_file() -> Option<PathBuf> {
    let selector = place_filter()?;
    let lower = selector.trim().to_ascii_lowercase();
    if !(lower.ends_with(".rbxl") || lower.ends_with(".rbxlx")) {
        return None;
    }
    Some(place_file_path(Path::new(selector.trim()))).filter(|file| file.is_file())
}

pub(crate) fn place_matches(info: &BridgeInfoPayload, selector: &str) -> bool {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return true;
    }
    if let Some((game_id, place_id)) = trimmed.split_once(':')
        && let (Ok(game_id), Ok(place_id)) = (game_id.parse::<i64>(), place_id.parse::<i64>())
    {
        return info.game_id == Some(game_id) && info.place_id == Some(place_id);
    }
    if let Ok(id) = trimmed.parse::<i64>() {
        return info.place_id == Some(id);
    }
    let name = info
        .place_name
        .trim()
        .trim_end_matches('*')
        .trim()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let wanted = trimmed.replace('\\', "/").to_ascii_lowercase();
    if wanted.ends_with(".rbxl") || wanted.ends_with(".rbxlx") {
        return if wanted.contains('/') {
            name == wanted
        } else {
            name.rsplit('/').next() == Some(wanted.as_str())
        };
    }
    !name.is_empty() && (name == wanted || name.contains(&wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_selectors_match_documents_instead_of_directory_substrings() {
        let info = BridgeInfoPayload {
            place_name: r"E:\projects\place2.rbxl\place.rbxl".into(),
            ..Default::default()
        };
        assert!(!place_matches(&info, "place2.rbxl"));
        assert!(place_matches(&info, "PLACE.RBXL"));
        assert!(place_matches(&info, "E:/projects/place2.rbxl/place.rbxl"));
        assert!(!place_matches(&info, "E:/other/place.rbxl"));
        let modified = BridgeInfoPayload {
            place_name: "/work/place2.rbxl*".into(),
            ..Default::default()
        };
        assert!(place_matches(&modified, "place2.rbxl"));
    }

    #[test]
    fn a_relative_place_path_selects_the_file_it_names_from_the_working_directory() {
        let here = std::env::current_dir().unwrap();
        let place = place_file_path(&here.parent().unwrap().join("Place.rbxl"));
        let info = BridgeInfoPayload {
            place_name: place.to_string_lossy().into_owned(),
            ..Default::default()
        };
        for selector in ["../Place.rbxl", r"..\Place.rbxl", "./../Place.rbxl"] {
            if cfg!(not(windows)) && selector.contains('\\') {
                continue;
            }
            assert!(
                place_matches(&info, &resolved_place_selector(selector)),
                "{selector}"
            );
        }
        assert!(!place_matches(
            &info,
            &resolved_place_selector("other/Place.rbxl")
        ));
        for kept in ["Place.rbxl", "Lobby", "12:34", "77"] {
            assert_eq!(resolved_place_selector(kept), kept);
        }
    }
}
