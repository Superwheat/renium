//! Index the paired records whose references can constrain a unit. Walk incoming
//! edges on both sides, including references to enclosing subtrees. This index is
//! shared by solving and publication, so outside proofs keep the same allowance.
use super::*;
use std::collections::VecDeque;

pub(super) struct Context {
    pub(super) incoming: Vec<Assignment>,
    pub(super) dependencies: Vec<AHashSet<usize>>,
    pub(super) owner: AHashMap<usize, usize>,
    pub(super) work: Vec<usize>,
}

struct Index<'a, 'd> {
    references: &'a References<'a, 'd>,
    result: Context,
    owners: [Vec<Option<usize>>; 2],
    blocked: [Vec<bool>; 2],
    paired: [Vec<Option<usize>>; 2],
    queue: VecDeque<(usize, usize, usize)>,
}

impl Context {
    /// Candidate-local content proofs constrain outgoing edges only. Incoming
    /// identity evidence belongs to the enclosing component's complete proof.
    pub(super) fn local(matcher: &Matcher<'_, '_>, units: &[Unit]) -> Self {
        let owner = units
            .iter()
            .enumerate()
            .flat_map(|(i, u)| u.sources.iter().map(move |&s| (s, i)))
            .collect();
        let dependencies = reference_dependencies(matcher, units, &owner);
        let work = units
            .iter()
            .map(|u| {
                u.sources.len() * 2
                    + u.sources
                        .iter()
                        .map(|&s| matcher.studio.references[s].len())
                        .sum::<usize>()
            })
            .collect();
        Self {
            incoming: vec![Vec::new(); units.len()],
            dependencies,
            owner,
            work,
        }
    }

    pub(super) fn new(references: &References<'_, '_>, units: &[Unit]) -> Self {
        let matcher = references.matcher;
        let result = Self {
            incoming: vec![Vec::new(); units.len()],
            dependencies: vec![AHashSet::new(); units.len()],
            owner: AHashMap::new(),
            work: vec![0; units.len()],
        };
        if units.is_empty()
            || !matcher
                .spend(3 * (references.partners.len() + matcher.editor.document.instances.len()))
        {
            return result;
        }
        let mut paired = [
            vec![None; references.partners.len()],
            vec![None; matcher.editor.document.instances.len()],
        ];
        for source in 0..paired[0].len() {
            let target = references
                .scope
                .and_then(|s| s.target(source))
                .unwrap_or(references.partners[source]);
            paired[0][source] = target;
            if let Some(target) = target {
                paired[1][target] = Some(source);
            }
        }
        let mut index = Index {
            references,
            result,
            owners: [vec![None; paired[0].len()], vec![None; paired[1].len()]],
            blocked: [vec![false; paired[0].len()], vec![false; paired[1].len()]],
            paired,
            queue: VecDeque::new(),
        };
        for (unit, item) in units.iter().enumerate() {
            let members = [
                if item.fixed && item.proposed.is_empty() {
                    subtree_sources(matcher.studio, &item.studio)
                } else {
                    item.sources.clone()
                },
                if item.fixed && !item.proposed.is_empty() {
                    item.editor.clone()
                } else {
                    subtree_sources(matcher.editor, &item.editor)
                },
            ];
            for (side, members) in members.into_iter().enumerate() {
                index.result.work[unit] += members.len();
                for node in members {
                    index.blocked[side][node] = true;
                    if !item.fixed {
                        index.assign(side, node, unit);
                    }
                }
            }
        }
        while let Some((side, node, unit)) = index.queue.pop_front() {
            index.visit(side, node, unit);
        }
        index.result.owner = index.owners[0]
            .iter()
            .enumerate()
            .filter_map(|(s, &u)| u.map(|u| (s, u)))
            .collect();
        for (unit, incoming) in index.result.incoming.iter_mut().enumerate() {
            incoming.sort_unstable_by(|a, b| {
                index.result.work[unit] += 1;
                a.cmp(b)
            });
            incoming.dedup();
        }
        index.result
    }
}

impl Index<'_, '_> {
    fn connect(&mut self, unit: usize, other: usize) {
        if unit != other {
            self.result.dependencies[unit].insert(other);
            self.result.dependencies[other].insert(unit);
        }
    }

    fn assign(&mut self, side: usize, node: usize, unit: usize) {
        if let Some(other) = self.owners[side][node] {
            self.connect(unit, other);
        } else {
            self.owners[side][node] = Some(unit);
            self.queue.push_back((side, node, unit));
        }
    }

    fn include(&mut self, side: usize, node: usize, unit: usize) {
        self.result.work[unit] += 1;
        if let Some(other) = self.owners[side][node] {
            self.connect(unit, other);
            return;
        }
        if self.blocked[side][node] {
            return;
        }
        let Some(peer) = self.paired[side][node] else {
            return;
        };
        if self.blocked[1 - side][peer] && self.owners[1 - side][peer].is_none() {
            return;
        }
        let pair = if side == 0 {
            (node, peer)
        } else {
            (peer, node)
        };
        self.result.incoming[unit].push(pair);
        self.assign(side, node, unit);
        self.assign(1 - side, peer, unit);
    }

    fn visit(&mut self, side: usize, node: usize, unit: usize) {
        let tree = if side == 0 {
            self.references.matcher.studio
        } else {
            self.references.matcher.editor
        };
        self.result.work[unit] += 1 + tree.incoming[node].len();
        for &source in &tree.incoming[node] {
            self.include(side, source, unit);
        }
        if let Some(parent) = tree.reference_parent[node] {
            self.include(side, parent, unit);
        }
    }
}
