use crate::app::context;
use crate::studio::bridge::BridgeInfoPayload;

pub(crate) fn set_place_filter(value: Option<String>) {
    context::set_place_selector(value);
}

pub(crate) fn place_filter() -> Option<String> {
    context::place_selector()
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
    fn numeric_place_selectors_never_fall_back_to_display_names() {
        let info = BridgeInfoPayload {
            place_id: Some(20),
            game_id: Some(10),
            place_name: "Unrelated 30".into(),
            ..Default::default()
        };
        assert!(place_matches(&info, "20"));
        assert!(place_matches(&info, "10:20"));
        assert!(!place_matches(&info, "30"));
        assert!(!place_matches(&info, "11:20"));
        assert!(place_matches(&info, "Unrelated"));
    }
}
