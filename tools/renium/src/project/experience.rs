use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::system::files::{canonical_path, read_json_file};

const EXPERIENCE_FILE: &str = "renium.experience.json";

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

struct ExperienceLayout {
    root: PathBuf,
    game_id: Option<i64>,
    places: Vec<ExperiencePlace>,
}

fn find_experience_root(start: &Path) -> Result<Option<PathBuf>> {
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

fn load_experience(start: &Path) -> Result<Option<ExperienceLayout>> {
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
    }))
}

fn matches_selector(layout: &ExperienceLayout, place: &ExperiencePlace, selector: &str) -> bool {
    let selector = selector.trim();
    if let Some((game_id, place_id)) = selector.split_once(':')
        && let (Ok(game_id), Ok(place_id)) = (game_id.parse::<i64>(), place_id.parse::<i64>())
    {
        return layout.game_id == Some(game_id) && place.place_id == Some(place_id);
    }
    if let Ok(place_id) = selector.parse::<i64>() {
        return place.place_id == Some(place_id);
    }
    place.alias.eq_ignore_ascii_case(selector)
        || place
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(selector))
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
            .filter_map(|(index, place)| {
                matches_selector(&layout, place, selector).then_some(index)
            })
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
