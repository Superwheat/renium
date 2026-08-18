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
    if let Ok(id) = trimmed.parse::<i64>()
        && info.place_id == Some(id)
    {
        return true;
    }
    let name = info.place_name.to_ascii_lowercase();
    let wanted = trimmed.to_ascii_lowercase();
    !name.is_empty() && (name == wanted || name.contains(&wanted))
}
