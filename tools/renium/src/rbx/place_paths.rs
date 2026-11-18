//! Resolve exported reference paths without copying every ancestor path.
use ahash::AHashMap;
use rbx_dom_weak::types::Ref;

use crate::settings::bytecode::SettingsBytecode;

struct Node {
    referent: Ref,
    children: AHashMap<String, Vec<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::edit::{
        collect_settings_subtree_preorder, insert_unique_rbx_path, instance_path_key,
    };
    use crate::settings::bytecode::{SETTINGS_BINARY_VERSION, SettingsBytecodeInstance};

    #[test]
    fn compact_paths_match_full_paths_including_ambiguous_ancestors() {
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        for (name, parent) in [
            ("Workspace", None),
            ("Same", Some(0)),
            ("Same", Some(0)),
            ("OnlyA", Some(1)),
            ("OnlyB", Some(2)),
            ("Both", Some(1)),
            ("Both", Some(2)),
            ("LateChild", Some(8)),
            ("LateParent", Some(0)),
        ] {
            let index = document.instances.len();
            document.instances.push(SettingsBytecodeInstance::new(
                index.to_string(),
                name.into(),
                if index == 0 { "Workspace" } else { "Folder" }.into(),
                parent,
            ));
        }
        let children = crate::settings::tree::settings_children_by_parent(&document);
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children, 0, &mut subtree);
        let refs = (0..document.instances.len())
            .map(|index| (index, Ref::new()))
            .collect();
        let mut compact = PlaceReferencePaths::default();
        compact.add_service(&document, 0, &subtree, &refs);
        let paths = crate::editor::paths::build_editor_instance_paths_with_children(
            &document,
            "Workspace",
            &children,
        );
        let mut unique = std::collections::HashMap::new();
        for (index, path) in paths.iter().enumerate() {
            let path = path.as_ref().unwrap();
            insert_unique_rbx_path(
                &mut unique,
                instance_path_key(&path.path_segments),
                refs[&index],
            );
            assert_eq!(
                compact.resolve(&path.path_segments, Some(&path.path_ordinals)),
                Some(refs[&index])
            );
        }
        for path in paths.iter().flatten() {
            assert_eq!(
                compact.resolve(&path.path_segments, None),
                unique[&instance_path_key(&path.path_segments)]
            );
            let mut outside = path.path_ordinals.clone();
            *outside.last_mut().unwrap() = usize::MAX;
            assert!(
                compact
                    .resolve(&path.path_segments, Some(&outside))
                    .is_none()
            );
        }
        assert_eq!(
            compact.resolve(&["Workspace".into(), "Same".into(), "OnlyB".into()], None),
            Some(refs[&4])
        );
        assert!(
            compact
                .resolve(&["Workspace".into(), "Same".into(), "Both".into()], None)
                .is_none()
        );
    }
}

#[derive(Default)]
pub(crate) struct PlaceReferencePaths {
    roots: AHashMap<String, Vec<usize>>,
    nodes: Vec<Node>,
}

impl PlaceReferencePaths {
    pub(crate) fn add_service(
        &mut self,
        document: &SettingsBytecode,
        root: usize,
        subtree: &[usize],
        referents: &AHashMap<usize, Ref>,
    ) {
        let start = self.nodes.len();
        let indices = subtree
            .iter()
            .enumerate()
            .map(|(offset, &index)| (index, start + offset))
            .collect::<AHashMap<_, _>>();
        self.nodes.extend(subtree.iter().map(|index| Node {
            referent: referents[index],
            children: AHashMap::new(),
        }));
        for &index in subtree {
            let row = &document.instances[index];
            let node = indices[&index];
            if index == root {
                self.roots.entry(row.name.clone()).or_default().push(node);
            } else if let Some(parent) = row.parent_index.and_then(|parent| indices.get(&parent)) {
                self.nodes[*parent]
                    .children
                    .entry(row.name.clone())
                    .or_default()
                    .push(node);
            }
        }
    }

    pub(crate) fn resolve(&self, segments: &[String], ordinals: Option<&[usize]>) -> Option<Ref> {
        if ordinals.is_some_and(|ordinals| {
            ordinals.len() != segments.len() || ordinals.first() != Some(&1)
        }) {
            return None;
        }
        let mut candidates = self.roots.get(segments.first()?)?.clone();
        for (depth, name) in segments.iter().enumerate().skip(1) {
            let mut next = Vec::new();
            for node in candidates {
                if let Some(children) = self.nodes[node].children.get(name) {
                    if let Some(ordinals) = ordinals {
                        if let Some(&child) = ordinals[depth]
                            .checked_sub(1)
                            .and_then(|ordinal| children.get(ordinal))
                        {
                            next.push(child);
                        }
                    } else {
                        next.extend(children);
                    }
                }
            }
            candidates = next;
        }
        if ordinals.is_some() {
            // The existing exact-path index replaces an earlier identical
            // service path; the ambiguous path-only index rejects duplicates.
            candidates.last().map(|index| self.nodes[*index].referent)
        } else {
            match candidates.as_slice() {
                [index] => Some(self.nodes[*index].referent),
                _ => None,
            }
        }
    }
}
