//! Reference equations are a cheap, preferred search order. They can propagate
//! an entire ring before branching. If no exact correspondence exists, the
//! caller still searches with content-proven interchangeable reference targets.
use super::*;

struct Row {
    choices: Vec<(usize, usize)>,
    live: Vec<bool>,
    count: usize,
    selected: bool,
}

enum Undo {
    Required(usize, usize),
    Removed(usize, usize),
    Selected(usize),
}

struct Exact<'a, 'd> {
    references: &'a References<'a, 'd>,
    scope: ReferenceScope<'a>,
    members: AHashSet<usize>,
    required: AHashMap<usize, usize>,
    owners: AHashMap<usize, usize>,
    rows: Vec<Row>,
    by_source: AHashMap<usize, Vec<(usize, usize)>>,
    by_target: AHashMap<usize, Vec<(usize, usize)>>,
    trail: Vec<Undo>,
    plan: Assignment,
}

pub(super) fn resolve(
    references: &References<'_, '_>,
    units: &[Unit],
    component: &[usize],
) -> Option<Assignment> {
    if !balanced_degrees(references.matcher, units, component)? {
        return None;
    }
    let mut search = Exact {
        references,
        scope: ReferenceScope {
            targets: AHashMap::new(),
            parent: references.scope,
        },
        members: AHashSet::new(),
        required: AHashMap::new(),
        owners: AHashMap::new(),
        rows: Vec::new(),
        by_source: AHashMap::new(),
        by_target: AHashMap::new(),
        trail: Vec::new(),
        plan: Vec::new(),
    };
    for &index in component {
        let unit = &units[index];
        if !references.matcher.spend(unit.sources.len()) {
            return None;
        }
        search.members.extend(&unit.sources);
        search
            .scope
            .replace(unit, &Vec::new(), references.matcher.locked);
        search.add_rows(&unit.studio, &unit.editor)?;
    }
    let mut pending = Vec::new();
    if let Some(incoming) = references.incoming {
        for &(source, target) in incoming {
            search.equations(source, target, &mut pending)?;
        }
    }
    search.require(pending)?;
    search.visit()
}

/// In a complete bijection, incoming degree is invariant. Reject an impossible
/// exact graph before exploring permutations; interchangeable content still
/// gets the caller's general search. Subsets deliberately omit this check.
fn balanced_degrees(
    matcher: &Matcher<'_, '_>,
    units: &[Unit],
    component: &[usize],
) -> Option<bool> {
    if component
        .iter()
        .any(|&i| units[i].studio.len() != units[i].editor.len())
    {
        return Some(true);
    }
    if !matcher.spend(
        component
            .iter()
            .map(|&i| units[i].sources.len() + units[i].proposed.len())
            .sum(),
    ) {
        return None;
    }
    let sources = component
        .iter()
        .flat_map(|&i| units[i].sources.iter().copied())
        .collect::<AHashSet<_>>();
    let targets = component
        .iter()
        .flat_map(|&i| units[i].proposed.iter().map(|&(_, e)| e))
        .collect::<AHashSet<_>>();
    let mut histograms = Vec::new();
    for (tree, members) in [(matcher.studio, &sources), (matcher.editor, &targets)] {
        let mut degrees = AHashMap::<usize, usize>::new();
        for &source in members {
            if !matcher.spend(1 + tree.references[source].len()) {
                return None;
            }
            for value in &tree.references[source] {
                if let Some(target) = tree.target(value).filter(|t| members.contains(t)) {
                    *degrees.entry(target).or_default() += 1;
                }
            }
        }
        let mut histogram = AHashMap::<(&str, &str, usize), usize>::new();
        for &source in members {
            if !matcher.spend(1) {
                return None;
            }
            let record = &tree.document.instances[source];
            *histogram
                .entry((
                    &record.name,
                    &record.class_name,
                    degrees.get(&source).copied().unwrap_or(0),
                ))
                .or_default() += 1;
        }
        histograms.push(histogram);
    }
    Some(histograms[0] == histograms[1])
}

impl Exact<'_, '_> {
    fn add_rows(&mut self, studio: &[usize], editor: &[usize]) -> Option<()> {
        // Root domains need record evidence only. Descendant domains are
        // constructed when that root is selected, avoiding all-pairs subtree
        // materialization for identical models with nested pointers.
        for a in 0..studio.len().min(editor.len()) {
            let mut choices = Vec::new();
            for b in
                std::iter::once(a).chain((0..studio.len().max(editor.len())).filter(|&b| b != a))
            {
                if !self.references.matcher.spend(1) {
                    return None;
                }
                let (s, e) = if studio.len() <= editor.len() {
                    (studio[a], editor[b])
                } else {
                    (studio[b], editor[a])
                };
                let matcher = self.references.matcher;
                if self.allowed(s, e)
                    && matcher.allowed(s, e)
                    && (matcher.locked_pair(s, e) || matcher.record(s, e))
                {
                    choices.push((s, e));
                }
            }
            let row = self.rows.len();
            if !self.references.matcher.spend(choices.len() * 3) {
                return None;
            }
            let live = choices
                .iter()
                .map(|&(s, e)| self.allowed(s, e))
                .collect::<Vec<_>>();
            let count = live.iter().filter(|&&v| v).count();
            for (col, &(s, e)) in choices.iter().enumerate() {
                self.by_source.entry(s).or_default().push((row, col));
                self.by_target.entry(e).or_default().push((row, col));
            }
            self.rows.push(Row {
                choices,
                live,
                count,
                selected: false,
            });
            if count == 0 {
                return None;
            }
        }
        Some(())
    }

    fn allowed(&self, source: usize, target: usize) -> bool {
        self.required.get(&source).is_none_or(|&e| e == target)
            && self.owners.get(&target).is_none_or(|&s| s == source)
    }

    fn equations(&self, source: usize, target: usize, pending: &mut Assignment) -> Option<()> {
        let matcher = self.references.matcher;
        if !matcher.spend(1) {
            return None;
        }
        if matcher.locked_pair(source, target)
            || matcher.studio.references[source].is_empty()
                && matcher.editor.references[target].is_empty()
        {
            return Some(());
        }
        if !matcher
            .spend(matcher.studio.reference_work[source] + matcher.editor.reference_work[target])
        {
            return None;
        }
        let equations = RefCell::new(Vec::new());
        let valid =
            reference_fields_equal(matcher.editor, target, matcher.studio, source, &|a, b| {
                if !matcher.spend(1) {
                    return false;
                }
                match (matcher.studio.target(b), matcher.editor.target(a)) {
                    (Some(s), Some(e)) => {
                        equations.borrow_mut().push((s, e));
                        true
                    }
                    _ => reconciliation_values_equal(a, b, false),
                }
            });
        if !valid {
            return None;
        }
        pending.extend(equations.into_inner());
        Some(())
    }

    fn require(&mut self, mut pending: Assignment) -> Option<()> {
        while let Some((source, target)) = pending.pop() {
            let matcher = self.references.matcher;
            if !matcher.spend(1) {
                return None;
            }
            if !self.members.contains(&source) {
                if self.references.target(source) != Some(target) {
                    return None;
                }
                continue;
            }
            if !self.allowed(source, target) || !matcher.allowed(source, target) {
                return None;
            }
            if self.required.contains_key(&source) {
                continue;
            }
            if !matcher.locked_pair(source, target) && !matcher.record(source, target) {
                return None;
            }
            self.required.insert(source, target);
            self.owners.insert(target, source);
            self.trail.push(Undo::Required(source, target));
            self.prune(source, target)?;
            self.equations(source, target, &mut pending)?;
            match (
                matcher.studio.document.instances[source].parent_index,
                matcher.editor.document.instances[target].parent_index,
            ) {
                (Some(s), Some(e)) => pending.push((s, e)),
                (None, None) => {}
                _ => return None,
            }
            // Unique descendants carry equations before their parent row is
            // selected; duplicated children retain their own candidate domains.
            if !matcher.spend(matcher.studio.children[source].len()) {
                return None;
            }
            for &group in &matcher.studio.children[source] {
                let children = &matcher.studio.groups[group];
                let first = &matcher.studio.document.instances[children[0]];
                let other = matcher.editor.counterpart(Some(target), first)?;
                let targets = &matcher.editor.groups[other];
                if children.len() == 1 && targets.len() == 1 {
                    pending.push((children[0], targets[0]));
                }
            }
        }
        Some(())
    }

    fn prune(&mut self, source: usize, target: usize) -> Option<()> {
        for entries in [self.by_source.get(&source), self.by_target.get(&target)]
            .into_iter()
            .flatten()
        {
            if !self.references.matcher.spend(entries.len()) {
                return None;
            }
            for &(row, col) in entries {
                let (s, e) = self.rows[row].choices[col];
                if self.rows[row].selected
                    || !self.rows[row].live[col]
                    || (s == source && e == target)
                {
                    continue;
                }
                self.rows[row].live[col] = false;
                self.rows[row].count -= 1;
                self.trail.push(Undo::Removed(row, col));
                if self.rows[row].count == 0 {
                    return None;
                }
            }
        }
        Some(())
    }

    fn visit(&mut self) -> Option<Assignment> {
        if !self.references.matcher.spend(self.rows.len()) {
            return None;
        }
        let row = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.selected)
            .min_by_key(|(_, r)| r.count)
            .map(|(i, _)| i);
        let Some(row) = row else {
            if !self.references.matcher.spend(self.plan.len()) {
                return None;
            }
            return self
                .references
                .in_scope(&self.scope)
                .complete(&self.plan)
                .then(|| self.plan.clone());
        };
        for col in 0..self.rows[row].choices.len() {
            if !self.references.matcher.spend(1) {
                return None;
            }
            if !self.rows[row].live[col] {
                continue;
            }
            let (source, target) = self.rows[row].choices[col];
            let mark = self.trail.len();
            let rows = self.rows.len();
            self.rows[row].selected = true;
            self.trail.push(Undo::Selected(row));
            self.plan.push((source, target));
            self.scope.targets.insert(source, Some(target));
            let result = self
                .require(vec![(source, target)])
                .and_then(|()| self.children(source, target))
                .and_then(|()| self.visit());
            self.plan.pop();
            self.scope.targets.insert(source, None);
            self.restore(mark, rows);
            if result.is_some() || !self.references.matcher.spend(0) {
                return result;
            }
        }
        None
    }

    fn children(&mut self, source: usize, target: usize) -> Option<()> {
        let matcher = self.references.matcher;
        if matcher.studio.children[source].len() != matcher.editor.children[target].len() {
            return None;
        }
        for &group in &matcher.studio.children[source] {
            let children = &matcher.studio.groups[group];
            let first = &matcher.studio.document.instances[children[0]];
            let other = matcher.editor.counterpart(Some(target), first)?;
            if children.len() != matcher.editor.groups[other].len() {
                return None;
            }
            self.add_rows(children, &matcher.editor.groups[other])?;
        }
        Some(())
    }

    fn restore(&mut self, mark: usize, rows: usize) {
        self.references.matcher.spend(self.trail.len() - mark);
        while self.trail.len() > mark {
            match self.trail.pop().unwrap() {
                Undo::Required(s, e) => {
                    self.required.remove(&s);
                    self.owners.remove(&e);
                }
                Undo::Removed(row, col) => {
                    self.rows[row].live[col] = true;
                    self.rows[row].count += 1;
                }
                Undo::Selected(row) => self.rows[row].selected = false,
            }
        }
        while self.rows.len() > rows {
            let row = self.rows.pop().unwrap();
            self.references.matcher.spend(row.choices.len() * 2);
            for (s, e) in row.choices {
                self.by_source.get_mut(&s).unwrap().pop();
                self.by_target.get_mut(&e).unwrap().pop();
            }
        }
    }
}
