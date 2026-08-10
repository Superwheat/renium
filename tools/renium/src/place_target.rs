use std::sync::Mutex;

use super::bridge_server::BridgeInfoPayload;

static PLACE_FILTER: Mutex<Option<String>> = Mutex::new(None);

pub(super) fn set_place_filter(value: Option<String>) {
    let mut guard = PLACE_FILTER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *guard = value.filter(|text| !text.trim().is_empty());
}

pub(super) fn place_filter() -> Option<String> {
    PLACE_FILTER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub(super) fn place_matches(info: &BridgeInfoPayload, selector: &str) -> bool {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return true;
    }
    if let Some((game_id, place_id)) = trimmed.split_once(':')
        && let (Ok(game_id), Ok(place_id)) = (game_id.parse::<i64>(), place_id.parse::<i64>())
    {
        return info.game_id == Some(game_id) && info.place_id == Some(place_id);
    }
    if let Ok(id) = trimmed.parse::<i64>()
        && info.place_id == Some(id)
    {
        return true;
    }
    let name = info.place_name.to_ascii_lowercase();
    let wanted = trimmed.to_ascii_lowercase();
    !name.is_empty() && (name == wanted || name.contains(&wanted))
}
