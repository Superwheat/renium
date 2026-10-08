use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::system::files::{canonical_path, read_json_file, write_utf8_file};

pub(crate) const EXPERIENCE_FILE: &str = "renium.experience.json";

#[derive(Debug)]
pub(crate) struct AmbiguousExperiencePlace(String);

impl fmt::Display for AmbiguousExperiencePlace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AmbiguousExperiencePlace {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExperienceManifest {
    game_id: Option<i64>,
    places: BTreeMap<String, ExperiencePlaceEntry>,
    #[serde(default)]
    shared_links: Vec<SharedLinkRecord>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SharedLinkRecord {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) service: String,
    pub(crate) path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ords: Vec<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExperiencePlaceEntry {
    place_id: Option<i64>,
    name: Option<String>,
    root: PathBuf,
}

pub(crate) struct ExperiencePlace {
    pub(crate) alias: String,
    pub(crate) place_id: Option<i64>,
    pub(crate) root: PathBuf,
    pub(crate) experience_root: PathBuf,
    pub(crate) game_id: Option<i64>,
    name: Option<String>,
}

pub(crate) struct ExperienceLayout {
    pub(crate) root: PathBuf,
    game_id: Option<i64>,
    pub(crate) places: Vec<ExperiencePlace>,
    pub(crate) shared_links: Vec<SharedLinkRecord>,
}

pub(crate) fn find_experience_root(start: &Path) -> Result<Option<PathBuf>> {
    let mut current =
        canonical_path(start).with_context(|| format!("Failed to resolve {}", start.display()))?;
    if current.is_file() {
        current.pop();
    }
    loop {
        if current.join(EXPERIENCE_FILE).is_file() {
            return Ok(Some(current));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

pub(crate) fn load_experience(start: &Path) -> Result<Option<ExperienceLayout>> {
    let Some(root) = find_experience_root(start)? else {
        return Ok(None);
    };
    let path = root.join(EXPERIENCE_FILE);
    let manifest: ExperienceManifest = read_json_file(&path)?;
    let mut places = Vec::with_capacity(manifest.places.len());
    for (alias, place) in manifest.places {
        if place.root.is_absolute()
            || place.root.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("Place '{alias}' has an invalid root in {}", path.display());
        }
        let place_root = canonical_path(&root.join(&place.root)).with_context(|| {
            format!(
                "Failed to resolve place '{alias}' at {}",
                root.join(&place.root).display()
            )
        })?;
        if place_root == root || !place_root.starts_with(&root) {
            bail!("Place '{alias}' resolves outside {}", root.display());
        }
        places.push(ExperiencePlace {
            alias,
            place_id: place.place_id,
            root: place_root,
            experience_root: root.clone(),
            game_id: manifest.game_id,
            name: place.name,
        });
    }
    if places.is_empty() {
        bail!("{} has no places", path.display());
    }
    Ok(Some(ExperienceLayout {
        root,
        game_id: manifest.game_id,
        places,
        shared_links: manifest.shared_links,
    }))
}

impl ExperienceLayout {
    pub(crate) fn place(&self, alias: &str) -> Option<&ExperiencePlace> {
        self.places.iter().find(|place| place.alias == alias)
    }

    pub(crate) fn place_containing(&self, path: &Path) -> Option<&ExperiencePlace> {
        let path = canonical_path(path).ok()?;
        self.places
            .iter()
            .filter(|place| path.starts_with(&place.root))
            .max_by_key(|place| place.root.components().count())
    }
}

/// Rewrites one top-level member in place: serde_json maps here sort their
/// keys, so a parse and re-serialise would reorder the rest of the file.
pub(crate) fn write_experience_member(root: &Path, key: &str, value: &Value) -> Result<()> {
    let path = root.join(EXPERIENCE_FILE);
    let text =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let updated = set_top_level_member(&text, key, value)
        .with_context(|| format!("Failed to update {key} in {}", path.display()))?;
    write_utf8_file(&path, &updated)
}

fn set_top_level_member(text: &str, key: &str, value: &Value) -> Result<String> {
    let body_start = text.len() - text.trim_start_matches('\u{feff}').len();
    let mut expected: Value = serde_json::from_str(&text[body_start..])?;
    expected
        .as_object_mut()
        .context("The manifest must contain a JSON object")?
        .insert(key.to_string(), value.clone());
    let bytes = text.as_bytes();
    let open = skip_json_whitespace(bytes, body_start);
    if bytes.get(open) != Some(&b'{') {
        bail!("The manifest must contain a JSON object");
    }
    let mut index = open + 1;
    let mut indent = None;
    let mut found = None;
    let mut last_value_end = None;
    let close = loop {
        let member = skip_json_whitespace(bytes, index);
        match bytes.get(member) {
            Some(b'}') => break member,
            Some(b'"') => {}
            _ => bail!("Unexpected content at byte {member}"),
        }
        if indent.is_none() {
            let line_start = text[..member]
                .rfind('\n')
                .map_or(0, |position| position + 1);
            indent = Some(&text[line_start..member]);
        }
        let name_end = json_string_end(bytes, member)?;
        let name: String = serde_json::from_str(&text[member..name_end])?;
        let colon = skip_json_whitespace(bytes, name_end);
        if bytes.get(colon) != Some(&b':') {
            bail!("Expected ':' at byte {colon}");
        }
        let value_start = skip_json_whitespace(bytes, colon + 1);
        let value_end = json_value_end(bytes, value_start)?;
        if name == key {
            found = Some((value_start, value_end));
        }
        last_value_end = Some(value_end);
        index = skip_json_whitespace(bytes, value_end);
        match bytes.get(index) {
            Some(b',') => index += 1,
            Some(b'}') => break index,
            _ => bail!("Expected ',' or '}}' at byte {index}"),
        }
    };
    let unit = indent
        .filter(|indent| !indent.is_empty() && indent.chars().all(|ch| ch == ' ' || ch == '\t'))
        .unwrap_or("  ");
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut rendered = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut rendered,
        serde_json::ser::PrettyFormatter::with_indent(unit.as_bytes()),
    );
    value.serialize(&mut serializer)?;
    let rendered = String::from_utf8(rendered)?.replace('\n', &format!("{newline}{unit}"));
    let member = format!("{}: {rendered}", serde_json::to_string(key)?);
    let updated = match (found, last_value_end) {
        (Some((start, end)), _) => format!("{}{rendered}{}", &text[..start], &text[end..]),
        (None, Some(end)) => format!("{},{newline}{unit}{member}{}", &text[..end], &text[end..]),
        (None, None) => format!(
            "{}{newline}{unit}{member}{newline}{}",
            &text[..=open],
            &text[close..]
        ),
    };
    if serde_json::from_str::<Value>(&updated[body_start..])? != expected {
        bail!("The rewritten manifest does not match the intended change");
    }
    Ok(updated)
}

fn skip_json_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn json_string_end(bytes: &[u8], start: usize) -> Result<usize> {
    let mut index = start + 1;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'\\' => index += 2,
            b'"' => return Ok(index + 1),
            _ => index += 1,
        }
    }
    bail!("Unterminated string at byte {start}")
}

fn json_value_end(bytes: &[u8], start: usize) -> Result<usize> {
    let mut depth = 0usize;
    let mut index = start;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'"' => {
                index = json_string_end(bytes, index)?;
                if depth == 0 {
                    return Ok(index);
                }
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => return Ok(index),
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            b',' if depth == 0 => return Ok(index),
            byte if depth == 0 && byte.is_ascii_whitespace() => return Ok(index),
            _ => {}
        }
        index += 1;
    }
    if depth == 0 && index > start {
        Ok(index)
    } else {
        bail!("Unterminated value at byte {start}")
    }
}

impl ExperiencePlace {
    pub(crate) fn matches_selector(&self, selector: &str) -> bool {
        let selector = selector.trim();
        if let Some((game_id, place_id)) = selector.split_once(':')
            && let (Ok(game_id), Ok(place_id)) = (game_id.parse::<i64>(), place_id.parse::<i64>())
        {
            return self.game_id == Some(game_id) && self.place_id == Some(place_id);
        }
        if let Ok(place_id) = selector.parse::<i64>() {
            return self.place_id == Some(place_id);
        }
        self.alias.eq_ignore_ascii_case(selector)
            || self
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(selector))
    }
}

fn choices(layout: &ExperienceLayout) -> String {
    layout
        .places
        .iter()
        .map(|place| match place.place_id {
            Some(place_id) => format!("{} ({place_id})", place.alias),
            None => place.alias.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn resolve_experience_place(
    start: &Path,
    selector: Option<&str>,
) -> Result<Option<ExperiencePlace>> {
    let Some(mut layout) = load_experience(start)? else {
        return Ok(None);
    };
    let current =
        canonical_path(start).with_context(|| format!("Failed to resolve {}", start.display()))?;
    if let Some(selector) = selector.filter(|value| !value.trim().is_empty()) {
        let matches = layout
            .places
            .iter()
            .enumerate()
            .filter_map(|(index, place)| place.matches_selector(selector).then_some(index))
            .collect::<Vec<_>>();
        return match matches.len() {
            1 => Ok(Some(layout.places.swap_remove(matches[0]))),
            0 => bail!(
                "Place '{selector}' is not configured in {}. Choose one of: {}",
                layout.root.display(),
                choices(&layout)
            ),
            _ => Err(AmbiguousExperiencePlace(format!(
                "Place selector '{selector}' is ambiguous. Choose one of: {}",
                choices(&layout)
            ))
            .into()),
        };
    }
    if let Some(index) = layout
        .places
        .iter()
        .position(|place| current == place.root || current.starts_with(&place.root))
    {
        return Ok(Some(layout.places.swap_remove(index)));
    }
    if layout.places.len() == 1 {
        return Ok(layout.places.pop());
    }
    Err(AmbiguousExperiencePlace(format!(
        "This is a multi-place Renium project. Choose a place with --place <alias|placeId>: {}",
        choices(&layout)
    ))
    .into())
}

pub(crate) fn resolve_experience_game_id(start: &Path) -> Result<Option<i64>> {
    Ok(load_experience(start)?.and_then(|layout| layout.game_id))
}

#[cfg(test)]
mod tests {
    use super::set_top_level_member;
    use serde_json::json;

    #[test]
    fn top_level_member_writes_keep_the_rest_of_the_manifest() {
        let original =
            "{\r\n    \"places\": {\"a\": {\"root\": \"places/a\"}},\r\n    \"gameId\": 5\r\n}\r\n";
        let inserted =
            set_top_level_member(original, "sharedLinks", &json!([{ "id": "x" }])).unwrap();
        assert_eq!(
            inserted,
            "{\r\n    \"places\": {\"a\": {\"root\": \"places/a\"}},\r\n    \"gameId\": 5,\r\n    \"sharedLinks\": [\r\n        {\r\n            \"id\": \"x\"\r\n        }\r\n    ]\r\n}\r\n"
        );
        assert_eq!(
            set_top_level_member(&inserted, "sharedLinks", &json!([])).unwrap(),
            "{\r\n    \"places\": {\"a\": {\"root\": \"places/a\"}},\r\n    \"gameId\": 5,\r\n    \"sharedLinks\": []\r\n}\r\n"
        );
        assert_eq!(
            set_top_level_member("{}", "k", &json!(1)).unwrap(),
            "{\n  \"k\": 1\n}"
        );
        assert!(set_top_level_member("[1]", "k", &json!(1)).is_err());
    }
}
