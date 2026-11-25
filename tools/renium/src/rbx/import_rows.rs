//! Host-only originals for fields that the final import leaves to setters.
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rbx_dom_weak::{WeakDom, types::Ref};
use sha2::{Digest, Sha256};

use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};

/// A small subtree is indivisible in both payload routes. With no retained live
/// packages, a reference inside it cannot become a post-import write. The
/// viewport and service/container anchors are excluded because they stay live.
pub(crate) fn reference_scopes(
    document: &SettingsBytecode,
    service: &str,
    root: usize,
    children: &[Vec<usize>],
    refs: &crate::rbx::model::BytecodeModelExportRefs<'_>,
    limit: usize,
    viewport: Option<Ref>,
) -> HashMap<Ref, usize> {
    let mut order = Vec::new();
    crate::bytecode::edit::collect_settings_subtree_preorder(children, root, &mut order);
    let mut sizes = vec![1usize; document.instances.len()];
    let mut contains_viewport = vec![false; document.instances.len()];
    for &index in order.iter().rev() {
        contains_viewport[index] |= refs.by_index.get(&index).copied() == viewport;
        if let Some(parent) = document.instances[index].parent_index {
            sizes[parent] += sizes[index];
            contains_viewport[parent] |= contains_viewport[index];
        }
    }
    let mut scopes = HashMap::new();
    let mut by_index = vec![None; document.instances.len()];
    let mut below_viewport = vec![false; document.instances.len()];
    for index in order {
        let row = &document.instances[index];
        below_viewport[index] = refs.by_index.get(&index).copied() == viewport
            || row
                .parent_index
                .is_some_and(|parent| below_viewport[parent]);
        if below_viewport[index]
            || index == root
            || row.parent_index == Some(root)
                && crate::roblox::services::is_engine_managed_container(service, &row.class_name)
        {
            continue;
        }
        let scope = row
            .parent_index
            .and_then(|parent| by_index[parent])
            .or_else(|| (sizes[index] <= limit && !contains_viewport[index]).then_some(index));
        by_index[index] = scope;
        if let (Some(scope), Some(referent)) = (scope, refs.by_index.get(&index)) {
            scopes.insert(*referent, scope);
        }
    }
    scopes
}

pub(crate) fn has_selected_raw_property(
    class: &str,
    properties: &[(&str, crate::settings::bytecode::RawBlockValue<'_>)],
    names: Option<&HashSet<String>>,
) -> Result<bool> {
    let Some(names) = names else { return Ok(false) };
    let database = rbx_reflection_database::get()?;
    let matched = properties.iter().any(|(name, _)| {
        let logical =
            crate::settings::equivalence::serialized_only_logical_name(database, class, name)
                .or_else(|| crate::rbx::encode::rbx_logical_property_name(database, class, name))
                .unwrap_or(name);
        names.contains(logical)
    });
    if matched {
        return Ok(true);
    }
    let mut map = serde_json::Map::with_capacity(properties.len());
    for (name, value) in properties {
        map.insert((*name).to_string(), value.to_json());
    }
    has_selected_property(class, &map, Some(names))
}

pub(crate) fn has_selected_property(
    class: &str,
    properties: &serde_json::Map<String, serde_json::Value>,
    names: Option<&HashSet<String>>,
) -> Result<bool> {
    let Some(names) = names else { return Ok(false) };
    let database = rbx_reflection_database::get()?;
    let selected = |properties: &serde_json::Map<String, serde_json::Value>| {
        properties.keys().any(|name| {
            let logical =
                crate::settings::equivalence::serialized_only_logical_name(database, class, name)
                    .or_else(|| {
                        crate::rbx::encode::rbx_logical_property_name(database, class, name)
                    })
                    .unwrap_or(name);
            names.contains(logical)
        })
    };
    if selected(properties) {
        return Ok(true);
    }
    // Migrations and content aliases are owned by the caller's canonicalizer.
    // Do not duplicate their list here or expand a held payload.
    let mut row = SettingsBytecodeInstance::new(String::new(), String::new(), class.into(), None);
    row.properties = properties.clone();
    crate::settings::equivalence::canonicalize_instances(std::slice::from_mut(&mut row))?;
    Ok(selected(&row.properties))
}

pub(crate) fn identity(document: &SettingsBytecode) -> [u8; 32] {
    let mut hash = Sha256::new();
    for instance in &document.instances {
        hash.update((instance.settings_id.len() as u64).to_le_bytes());
        hash.update(instance.settings_id.as_bytes());
    }
    hash.finalize().into()
}

pub(crate) struct UnreadRows {
    identity: [u8; 32],
    unread: Vec<bool>,
}

impl UnreadRows {
    pub(crate) fn new(document: &SettingsBytecode) -> Self {
        Self {
            identity: identity(document),
            unread: document
                .instances
                .iter()
                .map(|row| crate::settings::held::stands_unchanged(&row.properties))
                .collect(),
        }
    }

    pub(crate) fn validate(&self, document: &SettingsBytecode) -> Result<()> {
        anyhow::ensure!(
            self.identity == identity(document) && self.unread.len() == document.instances.len(),
            "The store's instance identities changed during import preparation; retry the push from the current files"
        );
        Ok(())
    }

    pub(crate) fn contains(&self, index: usize) -> bool {
        self.unread[index]
    }
}

pub(crate) struct KeptImportRows {
    identity: [u8; 32],
    pub(crate) rows: Vec<(usize, SettingsBytecodeInstance)>,
}

type CandidateRows = ([u8; 32], Vec<(Ref, usize, SettingsBytecodeInstance)>);

#[derive(Default)]
pub(crate) struct ImportRowCandidates {
    pub(crate) services: HashMap<String, CandidateRows>,
}

impl ImportRowCandidates {
    /// Save paths before payload grouping can move or remove DOM nodes.
    pub(crate) fn paths(&self, dom: &WeakDom) -> HashMap<Ref, String> {
        let mut paths = crate::rbx::model::RbxDomPaths::new(dom);
        self.services
            .values()
            .flat_map(|(_, rows)| rows)
            .map(|(referent, _, _)| {
                let (segments, ordinals) = paths.parts(*referent);
                (
                    *referent,
                    crate::bytecode::edit::instance_path_parts_key(&segments, &ordinals),
                )
            })
            .collect()
    }

    pub(crate) fn finish(
        self,
        paths: &HashMap<Ref, String>,
        classes: &HashMap<String, HashSet<String>>,
        omitted_paths: &HashMap<String, HashSet<String>>,
    ) -> Result<HashMap<String, KeptImportRows>> {
        self.services
            .into_iter()
            .map(|(service, (identity, rows))| {
                let mut rows = rows
                    .into_iter()
                    .map(|(referent, index, row)| {
                        let class = classes.get(&row.class_name);
                        let path = paths
                            .get(&referent)
                            .and_then(|path| omitted_paths.get(path));
                        let selected =
                            has_selected_property(&row.class_name, &row.properties, class)?
                                || has_selected_property(&row.class_name, &row.properties, path)?;
                        Ok(selected.then_some((index, row)))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                rows.sort_unstable_by_key(|(index, _)| *index);
                Ok((service, KeptImportRows { identity, rows }))
            })
            .collect()
    }
}

impl KeptImportRows {
    /// Reuse the exact decoded originals, then apply the same reference and
    /// alias canonicalization as `read_instances`, without reading the file.
    pub(crate) fn materialize(self, document: &SettingsBytecode) -> Result<SettingsBytecode> {
        anyhow::ensure!(
            self.identity == identity(document),
            "The store's instance identities changed during import preparation; retry the push from the current files"
        );
        let unread = |row: &SettingsBytecodeInstance| {
            crate::settings::held::stands_unchanged(&row.properties)
        };
        let mut partly = SettingsBytecode {
            version: document.version,
            instances: document
                .instances
                .iter()
                .map(|row| {
                    if unread(row) {
                        SettingsBytecodeInstance::new(
                            row.settings_id.clone(),
                            row.name.clone(),
                            row.class_name.clone(),
                            row.parent_index,
                        )
                    } else {
                        row.clone()
                    }
                })
                .collect(),
        };
        let mut indices = Vec::new();
        for (index, row) in self.rows {
            let held = document.instances.get(index).ok_or_else(|| anyhow::anyhow!(
                "A retained import row is outside its store; retry the push from the current files"
            ))?;
            anyhow::ensure!(
                row.settings_id == held.settings_id
                    && row.name == held.name
                    && row.class_name == held.class_name
                    && row.parent_index == held.parent_index,
                "A retained import row changed identity or hierarchy; retry the push from the current files"
            );
            if unread(held) {
                partly.instances[index] = row;
                indices.push(index);
            }
        }
        let ids = partly
            .instances
            .iter()
            .map(|row| row.settings_id.clone())
            .collect::<Vec<_>>();
        for index in indices {
            crate::settings::equivalence::stabilize_rows(&mut partly, index..index + 1, &ids);
            crate::settings::equivalence::canonicalize_instances(std::slice::from_mut(
                &mut partly.instances[index],
            ))?;
        }
        Ok(partly)
    }
}
