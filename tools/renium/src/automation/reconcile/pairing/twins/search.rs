//! Bounded, forward-checking search for stalled reference components.
use super::*;
use std::cell::Cell;
mod exact;

// The allowance covers collection, search, graph rebuilding and publication.
// The same counter covers local refinement and all nested alternative searches.
pub(super) const WORK_LIMIT: usize = 65_536;

pub(super) struct Budget {
    remaining: Cell<usize>,
    pub(super) exhausted: Cell<bool>,
}

impl Budget {
    pub(super) fn new(remaining: usize) -> Self {
        Self {
            remaining: Cell::new(remaining),
            exhausted: Cell::new(false),
        }
    }

    pub(super) fn spend(&self, work: usize) -> bool {
        if let Some(left) = self.remaining.get().checked_sub(work) {
            self.remaining.set(left);
            !self.exhausted.get()
        } else {
            self.remaining.set(0);
            self.exhausted.set(true);
            false
        }
    }

    pub(super) fn used(&self, initial: usize) -> usize {
        initial - self.remaining() + usize::from(self.exhausted.get())
    }

    pub(super) fn remaining(&self) -> usize {
        self.remaining.get()
    }
}

pub(super) enum Outcome {
    Paired(Assignment),
    Partial {
        plan: Assignment,
        proven: AHashSet<usize>,
    },
    NoCounterpart,
    Exhausted,
}

impl Outcome {
    pub(super) fn plan(&self) -> Option<&Assignment> {
        match self {
            Self::Paired(plan) | Self::Partial { plan, .. } => Some(plan),
            Self::NoCounterpart | Self::Exhausted => None,
        }
    }
}

type Choices = Vec<Vec<(usize, usize)>>;

struct Search<'a, 'd> {
    references: &'a References<'a, 'd>,
    scope: ReferenceScope<'a>,
    plan: Assignment,
    sources: AHashSet<usize>,
    targets: AHashSet<usize>,
    pending: AHashSet<usize>,
    evidence: RefCell<AHashMap<(usize, usize), Access>>,
}

#[derive(Default)]
struct Access {
    sources: AHashSet<usize>,
    targets: AHashSet<usize>,
}

pub(super) fn resolve(
    references: &References<'_, '_>,
    units: &[Unit],
    component: &[usize],
) -> Option<Assignment> {
    if let Some(plan) = exact::resolve(references, units, component) {
        return Some(plan);
    }
    if !references.matcher.spend(
        references.claimed.map_or(0, |v| v.len()) + references.pending.map_or(0, |v| v.len()),
    ) {
        return None;
    }
    let mut search = Search {
        references,
        scope: ReferenceScope {
            targets: AHashMap::new(),
            parent: references.scope,
        },
        plan: Vec::new(),
        sources: AHashSet::new(),
        targets: references.claimed.cloned().unwrap_or_default(),
        pending: references.pending.cloned().unwrap_or_default(),
        evidence: RefCell::default(),
    };
    let mut rows = Vec::new();
    for &index in component {
        let unit = &units[index];
        if !references.matcher.spend(unit.sources.len() * 2) {
            return None;
        }
        search
            .scope
            .replace(unit, &Vec::new(), references.matcher.locked);
        search.pending.extend(&unit.sources);
        rows.extend(choices(references.matcher, &unit.studio, &unit.editor)?);
    }
    search.visit(rows)
}

fn choices(matcher: &Matcher<'_, '_>, studio: &[usize], editor: &[usize]) -> Option<Choices> {
    (0..studio.len().min(editor.len()))
        .map(|a| {
            let mut row = Vec::new();
            for b in 0..studio.len().max(editor.len()) {
                if !matcher.spend(1) {
                    return None;
                }
                let (s, e) = if studio.len() <= editor.len() {
                    (a, b)
                } else {
                    (b, a)
                };
                if studio.len().max(editor.len()) > GROUP_LIMIT && s != e {
                    continue;
                }
                if matcher.subtree(studio[s], editor[e]).is_some() {
                    row.push((studio[s], editor[e]));
                }
            }
            row.sort_by(|&(s, e), &(t, f)| {
                matcher.spend(1);
                (s != studio[a] || e != editor[a], s, e).cmp(&(
                    t != studio[a] || f != editor[a],
                    t,
                    f,
                ))
            });
            (!row.is_empty() && matcher.spend(0)).then_some(row)
        })
        .collect()
}

impl Search<'_, '_> {
    fn children(&self, source: usize, target: usize) -> Option<Choices> {
        let matcher = self.references.matcher;
        let mut rows = Vec::new();
        for &group in &matcher.studio.children[source] {
            let studio = &matcher.studio.groups[group];
            let first = &matcher.studio.document.instances[studio[0]];
            let other = matcher.editor.counterpart(Some(target), first)?;
            rows.extend(choices(matcher, studio, &matcher.editor.groups[other])?);
        }
        Some(rows)
    }

    fn visit(&mut self, mut rows: Choices) -> Option<Assignment> {
        if !self.references.matcher.spend(1) {
            return None;
        }
        if rows.is_empty() {
            // Partial propagation never authorizes publication. In particular,
            // unselected sources on the larger side are now absent, not unknown.
            return self
                .references
                .in_scope(&self.scope)
                .complete(&self.plan)
                .then(|| self.plan.clone());
        }
        self.propagate(&mut rows)?;
        let row = (0..rows.len()).min_by_key(|&index| rows[index].len())?;
        let candidates = rows.remove(row);
        for (source, target) in candidates {
            let previous = self.bind(source, target);
            let result = self.children(source, target).and_then(|children| {
                if !self
                    .references
                    .matcher
                    .spend(rows.iter().map(Vec::len).sum::<usize>() + rows.len())
                {
                    return None;
                }
                let mut next = rows.clone();
                next.extend(children);
                self.visit(next)
            });
            self.unbind(source, target, previous);
            if result.is_some() || !self.references.matcher.spend(0) {
                return result;
            }
        }
        None
    }

    /// Forward-check every domain before choosing the most constrained variable.
    /// Each trial checks its outgoing references AND all already-bound referrers,
    /// so an incoming edge can reject a binding even if an earlier edge is unknown.
    fn propagate(&mut self, rows: &mut Choices) -> Option<()> {
        for row in rows {
            let mut remaining = Vec::new();
            for &(source, target) in row.iter() {
                if !self.references.matcher.spend(1) {
                    return None;
                }
                if self.sources.contains(&source) || self.targets.contains(&target) {
                    continue;
                }
                let previous = self.bind(source, target);
                let viable = self.viable(source, target);
                self.unbind(source, target, previous);
                if viable {
                    remaining.push((source, target));
                }
            }
            if remaining.is_empty() {
                return None;
            }
            *row = remaining;
        }
        Some(())
    }

    fn bind(&mut self, source: usize, target: usize) -> Option<usize> {
        self.plan.push((source, target));
        self.sources.insert(source);
        self.targets.insert(target);
        self.pending.remove(&source);
        self.scope.targets.insert(source, Some(target)).flatten()
    }

    fn unbind(&mut self, source: usize, target: usize, previous: Option<usize>) {
        self.scope.targets.insert(source, previous);
        self.pending.insert(source);
        self.sources.remove(&source);
        self.targets.remove(&target);
        self.plan.pop();
    }

    fn viable(&self, source: usize, target: usize) -> bool {
        if !self
            .references
            .matcher
            .spend(self.plan.len() + self.references.incoming.map_or(0, Vec::len))
        {
            return false;
        }
        let mut references = self.references.in_scope(&self.scope);
        references.pending = Some(&self.pending);
        references.claimed = Some(&self.targets);
        self.plan
            .iter()
            .chain(references.incoming.into_iter().flatten())
            .all(|&(s, e)| {
                let affected = (s, e) == (source, target)
                    || self.evidence.borrow().get(&(s, e)).is_none_or(|access| {
                        access.sources.contains(&source) || access.targets.contains(&target)
                    });
                !affected || self.check_record(&references, s, e)
            })
    }
    fn check_record(&self, references: &References<'_, '_>, source: usize, target: usize) -> bool {
        let sources = RefCell::default();
        let targets = RefCell::default();
        let mut proof = references.in_scope(&self.scope);
        proof.reads = &sources;
        proof.availability_reads = Some(&targets);
        let valid = proof.matcher.locked_pair(source, target)
            || proof.record_references_equal(source, target);
        let sources = sources.into_inner();
        let targets = targets.into_inner();
        if !references.matcher.spend(sources.len() + targets.len()) {
            return false;
        }
        references.reads.borrow_mut().extend(&sources);
        let mut evidence = self.evidence.borrow_mut();
        let access = evidence.entry((source, target)).or_default();
        access.sources.extend(sources);
        access.targets.extend(targets);
        valid
    }
}
