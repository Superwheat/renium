//! Host-only originals for fields that the final import leaves to setters.
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rbx_dom_weak::{WeakDom, types::Ref};
use sha2::{Digest, Sha256};

use crate::editor::types::IMPORT_MODEL_PIVOT;
use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};

pub(crate) fn identity(document: &SettingsBytecode) -> [u8; 32] {
    let mut hash = Sha256::new();
    for instance in &document.instances {
        hash.update((instance.settings_id.len() as u64).to_le_bytes());
        hash.update(instance.settings_id.as_bytes());
    }
    hash.finalize().into()
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
            .filter(|(_, _, row)| row.class_name != IMPORT_MODEL_PIVOT.0)
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
        let database = rbx_reflection_database::get()?;
        Ok(self
            .services
            .into_iter()
            .map(|(service, (identity, rows))| {
                let mut rows = rows
                    .into_iter()
                    .filter_map(|(referent, index, row)| {
                        let class = classes.get(&row.class_name);
                        let path = paths
                            .get(&referent)
                            .and_then(|path| omitted_paths.get(path));
                        let selected = row.class_name == IMPORT_MODEL_PIVOT.0
                            || row.properties.keys().any(|name| {
                                let logical = crate::rbx::encode::rbx_logical_property_name(
                                    database,
                                    &row.class_name,
                                    name,
                                )
                                .unwrap_or(name);
                                class.is_some_and(|names| names.contains(logical))
                                    || path.is_some_and(|names| names.contains(logical))
                            });
                        selected.then_some((index, row))
                    })
                    .collect::<Vec<_>>();
                rows.sort_unstable_by_key(|(index, _)| *index);
                (service, KeptImportRows { identity, rows })
            })
            .collect())
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
