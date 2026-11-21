//! A pull of the instances Studio changed values of alone: their rows are
//! captured and replaced in the held stores, and every doubt leaves the pull
//! to the capture of their services.

use super::*;

/// The most stored instances a pull of chosen instances reads, captures and
/// compares under all the touched ones of a store; past it a capture of the
/// service takes about as long.
const INSTANCE_PULL_ROWS: usize = 4_096;

/// What a store or a script file a pull of chosen instances does not write
/// depends on: an instance's place, its file and whether it is exported.
const INSTANCE_PULL_REFUSED: [&str; 6] = [
    "Name",
    "Parent",
    "ClassName",
    "Archivable",
    "Source",
    crate::editor::paths::SOURCE_PATH_CONTEXT_PROPERTY,
];

/// What `entry` names that a pull of instances does not take: a refused
/// property, or anything of the session lock's folder, whose attribute
/// decides whether it is exported.
fn names_refused(entry: &TouchedInstance) -> Option<&str> {
    if entry
        .path_segments
        .iter()
        .any(|segment| segment == "__Renium_SessionLock")
    {
        return Some("the session lock");
    }
    let fields = entry.fields.as_ref()?;
    fields.properties.iter().map(String::as_str).find(|name| {
        INSTANCE_PULL_REFUSED
            .iter()
            .any(|refused| refused.eq_ignore_ascii_case(name))
    })
}

/// One store's part of a pull of the instances Studio changed values of
/// alone: the store as held, read under the touched instances, and the roots
/// a capture takes.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct StoreInstancePull {
    service: String,
    document: SettingsBytecode,
    retained: crate::settings::bytecode::property_edit::RetainedPropertyColumns,
    children: Vec<Vec<usize>>,
    /// Each touched instance: the rows its names lead to, and what changed.
    touched: Vec<(Vec<usize>, TouchedFields)>,
    pub(crate) roots: Vec<InstanceRoot>,
    /// The viewport camera's names where they lead to it alone.
    camera: Option<Vec<String>>,
}

/// A touched instance no other touched one stands above, which a capture
/// takes with its subtree.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct InstanceRoot {
    pub(crate) segments: Vec<String>,
    pub(crate) ordinals: Vec<usize>,
    pub(crate) debug_id: String,
    touched: usize,
}

/// One captured instance: its place among the instances of its root's
/// capture, and its values as a capture of its service converts them.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct CapturedInstance {
    pub(crate) name: String,
    pub(crate) class_name: String,
    pub(crate) parent: Option<usize>,
    pub(crate) values: CapturedValues,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct CapturedValues {
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
    /// The properties a capture of the service takes from the plugin's
    /// reading of the class in place of the native bytes.
    pub(crate) overlaid: Arc<HashSet<String>>,
}

/// How a capture of a service converts the instances of one class: with
/// the native filter of the class where the plugin's schema of it names a
/// property, and with what the plugin reads in place of the bytes.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct ClassConversion {
    pub(crate) filter: Option<crate::rbx::decode::NativePropertyFilter>,
    pub(crate) overlaid: Arc<HashSet<String>>,
}

/// The conversion of each of `classes` by the plugin's schema `schemas` of
/// them, as `studio::native::editor` derives it for a capture of a service.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn class_conversions<'c>(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    schemas: &crate::roblox::schema::PropertySchemaMap,
    classes: impl IntoIterator<Item = &'c str>,
) -> HashMap<String, ClassConversion> {
    let classes = classes.into_iter().collect::<BTreeSet<_>>();
    let mut filters = (classes.iter())
        .filter(|class| schemas.contains_key(**class))
        .map(|class| {
            let filter = crate::rbx::decode::native_property_filter(database, class);
            (class.to_string(), filter)
        })
        .collect::<HashMap<_, _>>();
    let (overlay, ..) =
        crate::rbx::decode::native_overlay_property_schemas(database, schemas, &filters);
    (classes.into_iter())
        .map(|class| {
            let overlaid = (overlay.get(class).into_iter().flatten())
                .map(|entry| entry.name.clone())
                .collect();
            let conversion = ClassConversion {
                filter: filters.remove(class),
                overlaid: Arc::new(overlaid),
            };
            (class.to_string(), conversion)
        })
        .collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
impl StoreInstancePull {
    /// Plans the pull of the touched instances `entries` of `service` over
    /// its held store `bytes`, or says why there is none.
    pub(crate) fn plan(
        service: &str,
        bytes: &[u8],
        entries: &[TouchedInstance],
    ) -> std::result::Result<Self, String> {
        if service == crate::roblox::schema::MATERIAL_SERVICE_CLASS {
            return Err("MaterialService keeps a stored value no capture holds".into());
        }
        let (mut document, mut retained) =
            crate::settings::bytecode::property_edit::decode_for_property_edit(bytes)
                .map_err(|_| "the store does not decode")?;
        let count = document.instances.len();
        let mut children = vec![Vec::new(); count];
        for (row, instance) in document.instances.iter().enumerate() {
            match instance.parent_index {
                Some(parent) if parent < count => children[parent].push(row),
                Some(_) => return Err("the store names a parent it does not hold".into()),
                None => {}
            }
        }
        let tops = (0..count)
            .filter(|row| document.instances[*row].parent_index.is_none())
            .collect::<Vec<_>>();
        let rows_under = |names: &[String]| {
            let mut rows = tops.clone();
            for name in names {
                rows.retain(|row| document.instances[*row].name == *name);
                rows = rows
                    .iter()
                    .flat_map(|row| &children[*row])
                    .copied()
                    .collect();
            }
            rows
        };
        let leading_to = |path: &[String]| {
            let (last, parents) = path.split_last()?;
            Some(
                (rows_under(parents).into_iter())
                    .filter(|row| document.instances[*row].name == *last)
                    .collect::<Vec<_>>(),
            )
        };
        // A pull keeps the stored viewport camera whatever Studio holds, and
        // so does this one where the camera's names lead to it alone.
        let viewport = crate::settings::equivalence::workspace_current_camera_index(&document);
        let camera = viewport.and_then(|row| {
            let mut path = vec![document.instances[row].name.clone()];
            let mut above = document.instances[row].parent_index;
            while let Some(parent) = above.filter(|_| path.len() <= count) {
                path.insert(0, document.instances[parent].name.clone());
                above = document.instances[parent].parent_index;
            }
            (leading_to(&path).as_deref() == Some(&[row][..])).then_some(path)
        });
        let mut touched = Vec::<(&TouchedInstance, Vec<usize>, TouchedFields)>::new();
        for entry in entries {
            let fields = entry
                .fields
                .as_ref()
                .ok_or("an instance changed that Studio named nothing of")?;
            if let Some(name) = names_refused(entry) {
                return Err(format!("{name} changed"));
            }
            if entry.path_segments.first().map(String::as_str) != Some(service)
                || entry.path_segments.len() < 2
            {
                return Err("the service itself changed".into());
            }
            if camera.as_ref() == Some(&entry.path_segments) {
                continue;
            }
            let rows = leading_to(&entry.path_segments).unwrap_or_default();
            if rows.is_empty() {
                return Err("a touched instance is none the store holds".into());
            }
            if touched.iter().any(|(other, ..)| {
                other.path_segments == entry.path_segments
                    && other.path_ordinals == entry.path_ordinals
            }) {
                return Err("two touched instances stand at one path".into());
            }
            touched.push((entry, rows, fields.clone()));
        }
        let within = |inner: &TouchedInstance, outer: &TouchedInstance| {
            outer.path_segments.len() < inner.path_segments.len()
                && inner.path_segments.starts_with(&outer.path_segments)
                && inner.path_ordinals.starts_with(&outer.path_ordinals)
        };
        let roots = (touched.iter().enumerate())
            .filter(|(_, (entry, ..))| !touched.iter().any(|(outer, ..)| within(entry, outer)))
            .map(|(at, (entry, _, fields))| InstanceRoot {
                segments: entry.path_segments.clone(),
                ordinals: (entry.path_ordinals.iter())
                    .map(|ordinal| *ordinal as usize)
                    .collect(),
                debug_id: fields.debug_id.clone(),
                touched: at,
            })
            .collect::<Vec<_>>();
        let mut read = Vec::new();
        let mut seen = vec![false; count];
        let mut stack = (roots.iter())
            .flat_map(|root| touched[root.touched].1.iter().copied())
            .collect::<Vec<_>>();
        while let Some(row) = stack.pop() {
            if std::mem::replace(&mut seen[row], true) {
                continue;
            }
            let class = document.instances[row].class_name.as_str();
            if viewport == Some(row) || matches!(class, "Terrain" | "PackageLink") {
                return Err(format!("a {class} stands under a touched instance"));
            }
            read.push(row);
            if read.len() > INSTANCE_PULL_ROWS {
                return Err(format!(
                    "more than {INSTANCE_PULL_ROWS} stored instances stand under the touched ones"
                ));
            }
            stack.extend(&children[row]);
        }
        read.sort_unstable();
        retained
            .materialize(&mut document, &read)
            .map_err(|_| "the store's instances do not read")?;
        let touched = (touched.into_iter())
            .map(|(_, rows, fields)| (rows, fields))
            .collect();
        Ok(Self {
            service: service.to_string(),
            document,
            retained,
            children,
            touched,
            roots,
            camera,
        })
    }

    /// The root `entry` stands at or under, by its names and ordinals.
    fn root_of(&self, entry: &TouchedInstance) -> Option<&InstanceRoot> {
        self.roots.iter().find(|root| {
            entry.path_segments.starts_with(&root.segments)
                && entry.path_ordinals.len() >= root.ordinals.len()
                && (entry.path_ordinals.iter())
                    .zip(&root.ordinals)
                    .all(|(entry, root)| *entry as usize == *root)
        })
    }

    /// Whether a change Studio reports since the pull's proof leaves its
    /// capture what a whole pull would capture: values it names of a
    /// captured instance or one under it, or of the viewport camera.
    fn holds(&self, entry: &TouchedInstance) -> bool {
        entry.kind == "values"
            && entry.fields.is_some()
            && names_refused(entry).is_none()
            && (self.camera.as_ref() == Some(&entry.path_segments) || self.root_of(entry).is_some())
    }

    /// Takes what Studio names of the captured instances in `entries`, its
    /// changes since the pull's proof, for named as well: the capture came
    /// after the proof and may hold any of them.
    pub(crate) fn name_since(
        &mut self,
        entries: &[TouchedInstance],
    ) -> std::result::Result<(), String> {
        for entry in entries {
            if self.camera.as_ref() == Some(&entry.path_segments) {
                continue;
            }
            let (Some(fields), Some(root)) = (&entry.fields, self.root_of(entry)) else {
                return Err("a change since the proof is of no captured instance".into());
            };
            let mut rows = self.touched[root.touched].0.clone();
            for name in &entry.path_segments[root.segments.len()..] {
                rows = (rows.iter().flat_map(|row| &self.children[*row]).copied())
                    .filter(|row| self.document.instances[*row].name == *name)
                    .collect();
            }
            self.touched.push((rows, fields.clone()));
        }
        Ok(())
    }

    /// The store with each touched instance's row as `captured` (the
    /// instances of each root's capture, the root first) holds it; `None`
    /// where every row is as held. Says why where the capture holds anything
    /// Studio did not name, or a value it named the capture does not hold.
    pub(crate) fn apply(
        &mut self,
        captured: &[Vec<CapturedInstance>],
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        let database = rbx_reflection_database::get().map_err(|_| "no reflection database")?;
        if captured.len() != self.roots.len() {
            return Err("the capture holds other roots than asked".into());
        }
        let mut paired = HashMap::<usize, (usize, usize)>::new();
        for (index, (root, instances)) in self.roots.iter().zip(captured).enumerate() {
            let mut below = vec![Vec::new(); instances.len()];
            for (at, instance) in instances.iter().enumerate().skip(1) {
                let parent = (instance.parent)
                    .filter(|parent| *parent < at)
                    .ok_or("the capture holds an instance without its parent")?;
                below[parent].push(at);
            }
            let top = (instances.first())
                .filter(|top| top.parent.is_none())
                .and_then(|top| paired_row(&self.document, &self.touched[root.touched].0, top))
                .ok_or("a captured root is none of the stored instances at its path")?;
            let mut pending = vec![(0, top)];
            while let Some((at, row)) = pending.pop() {
                if paired.insert(row, (index, at)).is_some() {
                    return Err("two captured instances are one stored instance".into());
                }
                if below[at].len() != self.children[row].len() {
                    return Err("a captured instance holds other children than stored".into());
                }
                for child in &below[at] {
                    let held = paired_row(&self.document, &self.children[row], &instances[*child])
                        .ok_or("a captured instance is none of the stored ones at its place")?;
                    pending.push((*child, held));
                }
            }
        }
        let mut named = HashMap::<usize, TouchedFields>::new();
        for (rows, fields) in &self.touched {
            let mut found = rows.iter().filter(|row| paired.contains_key(row));
            let (Some(row), None) = (found.next(), found.next()) else {
                return Err("a touched instance is not told from its namesakes".into());
            };
            match named.get_mut(row) {
                Some(merged) => {
                    merged.properties.extend(fields.properties.iter().cloned());
                    merged.attributes |= fields.attributes;
                    merged.tags |= fields.tags;
                }
                None => {
                    named.insert(*row, fields.clone());
                }
            }
        }
        let mut rows = paired.into_iter().collect::<Vec<_>>();
        rows.sort_unstable();
        let mut changed = Vec::new();
        for (row, (index, at)) in rows {
            let held = &self.document.instances[row];
            let now = accepted_row(database, held, &captured[index][at].values, named.get(&row))?;
            if now != *held {
                changed.push((row, now));
            }
        }
        if changed.is_empty() {
            return Ok(None);
        }
        // A pull that counts every change as none leaves the store alone.
        let (mut as_held, mut as_named) = (true, true);
        for (row, now) in &changed {
            let held = &self.document.instances[*row];
            match crate::settings::equivalence::pulled_instance_values_unchanged(
                held,
                now,
                Some(&self.service),
            ) {
                Ok(Some((same_held, same_named))) => {
                    as_held &= same_held;
                    as_named &= same_named;
                }
                _ => return Err("an enum item of a changed instance gains its type".into()),
            }
        }
        if as_held || as_named {
            return Err("a pull counts the change as none".into());
        }
        let changed_rows = changed.len();
        for (row, now) in changed {
            self.document.instances[row] = now;
        }
        log_global(
            5,
            format_args!(
                "[renium] baseline: instance pull of {}: {changed_rows} rows changed of {} captured",
                self.service,
                captured.iter().map(Vec::len).sum::<usize>()
            ),
        );
        (self.retained.encode(&self.document))
            .map(Some)
            .map_err(|_| "the store does not encode".into())
    }
}

/// The stored row among `rows` that `captured` is: the one of its name and
/// class, told by its UniqueId among several.
fn paired_row(
    document: &SettingsBytecode,
    rows: &[usize],
    captured: &CapturedInstance,
) -> Option<usize> {
    let alike = (rows.iter().copied())
        .filter(|row| {
            let held = &document.instances[*row];
            held.name == captured.name && held.class_name == captured.class_name
        })
        .collect::<Vec<_>>();
    if let [row] = alike.as_slice() {
        return Some(*row);
    }
    let id = captured.values.properties.get("UniqueId")?;
    let mut told = alike.into_iter().filter(|row| {
        (document.instances[*row].properties.get("UniqueId"))
            .is_some_and(|held| crate::settings::held::same(held, id))
    });
    let row = told.next()?;
    told.next().is_none().then_some(row)
}

/// `held` with what Studio named of it taken from `captured`, or why the two
/// differ otherwise. A value the capture does not carry (one the plugin's
/// reading supplies, a reference) stays as held, where Studio did not name it.
fn accepted_row(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    held: &SettingsBytecodeInstance,
    captured: &CapturedValues,
    fields: Option<&TouchedFields>,
) -> std::result::Result<SettingsBytecodeInstance, String> {
    use crate::settings::equivalence::reconciliation_value_contains_reference as refers;
    let class = held.class_name.as_str();
    let named =
        |name: &str| fields.is_some_and(|fields| names_property(database, class, fields, name));
    if let Some(name) = (captured.overlaid.iter()).find(|name| fields.is_some() && named(name)) {
        return Err(format!("{name} changed, which the capture does not hold"));
    }
    let mut row = held.clone();
    let names = (held.properties.keys())
        .chain(captured.properties.keys())
        .collect::<BTreeSet<_>>();
    for name in names {
        let (was, now) = (held.properties.get(name), captured.properties.get(name));
        if was
            .zip(now)
            .is_some_and(|(was, now)| crate::settings::held::same(was, now))
        {
            continue;
        }
        let carried = !name.eq_ignore_ascii_case("Source")
            && !was.is_some_and(refers)
            && !now.is_some_and(refers)
            && !captured.overlaid.contains(name.as_str());
        if name == "UniqueId" || carried && !named(name) {
            return Err(format!("{name} differs where Studio named no change"));
        }
        if !carried {
            if named(name) {
                return Err(format!("{name} changed, which the capture does not hold"));
            }
            continue;
        }
        match now {
            Some(now) => row.properties.insert(name.clone(), now.clone()),
            None => row.properties.remove(name),
        };
    }
    if held.attributes != captured.attributes {
        if !fields.is_some_and(|fields| fields.attributes) {
            return Err("attributes differ where Studio named no change".into());
        }
        if (held.attributes.values())
            .chain(captured.attributes.values())
            .any(refers)
        {
            return Err("a changed attribute holds a reference".into());
        }
        row.attributes.clone_from(&captured.attributes);
    }
    Ok(row)
}

/// Whether Studio, naming `fields`, said the property a store holds as
/// `name` changed: under its name, its logical one or the one a file alone
/// gives it, as the content its named id became, or as the mirror of a
/// Decal's named texture.
fn names_property(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class: &str,
    fields: &TouchedFields,
    name: &str,
) -> bool {
    use crate::rbx::encode::rbx_logical_property_name as logical;
    use crate::settings::equivalence::serialized_only_logical_name as file_only;
    let held = logical(database, class, name).unwrap_or(name);
    name == "Tags" && fields.tags
        || fields.properties.iter().any(|named| {
            let named = match file_only(database, class, named) {
                Some(logical) => logical,
                None => logical(database, class, named).unwrap_or(named),
            };
            let successor = crate::bytecode::content_successor(class, named);
            [Some(named), successor.as_deref()]
                .into_iter()
                .flatten()
                .any(|named| {
                    named.eq_ignore_ascii_case(held)
                        || crate::bytecode::decal_texture_mirror(class, named)
                            .is_some_and(|mirror| mirror.eq_ignore_ascii_case(held))
                })
        })
}

/// The premise of a pull of chosen instances once Studio moved as `now`
/// says since its proof: inside where it changed only what `pulls` capture.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn instance_premise(
    now: &ProofScope,
    pulls: &BTreeMap<String, &StoreInstancePull>,
) -> ScopedPremise {
    let ProofScope::Services { services, touched } = now else {
        return ScopedPremise::False;
    };
    let inside = services.iter().all(|service| {
        let (Some(pull), Some(Some(entries))) = (pulls.get(service), touched.get(service)) else {
            return false;
        };
        entries.iter().all(|entry| pull.holds(entry))
    });
    if inside {
        ScopedPremise::Inside
    } else {
        ScopedPremise::False
    }
}

#[cfg(windows)]
impl ScopedPull {
    /// Where Studio changed only values it named of instances it named,
    /// captures those instances and the instances under them alone and
    /// writes their rows into the held stores, checking after the capture
    /// that Studio moved nowhere else since the proof. `None`, having written
    /// nothing, where it cannot: the services are captured instead.
    pub(super) fn pull_instances(
        &self,
        bridge: &BridgeServer,
        runtime: &str,
        args: &crate::cli::PullArgs,
    ) -> Result<
        Option<(
            crate::snapshot::export::PublishedProjectChanges,
            ScopedPremise,
        )>,
    > {
        let started = Instant::now();
        let not_taken = |reason: &str| {
            log_global(
                5,
                format_args!("[renium] baseline: instance pull not taken: {reason}"),
            );
            Ok(None)
        };
        let mut plans = Vec::new();
        for service in &self.services {
            let Some(Some(entries)) = self
                .touched
                .get(service)
                .filter(|entries| entries.as_ref().is_some_and(|entries| !entries.is_empty()))
            else {
                return not_taken("a service moved as a whole");
            };
            let unnamed = (entries.iter())
                .filter(|entry| entry.kind != "values" || entry.fields.is_none())
                .count();
            if unnamed > 0 {
                return not_taken(&format!(
                    "{unnamed} of {} touched instances are not values Studio named",
                    entries.len()
                ));
            }
            let held = (self.stores.get(service))
                .and_then(|store| Some((store, self.held.project.entries.get(store)?)));
            let Some((store, SnapshotEntry::File(bytes))) = held else {
                return not_taken("the baseline holds no store of a service");
            };
            match StoreInstancePull::plan(service, bytes, entries) {
                Ok(plan) => plans.push((store, bytes, plan)),
                Err(reason) => return not_taken(&reason),
            }
        }
        let roots = plans
            .iter()
            .map(|(.., plan)| plan.roots.len())
            .sum::<usize>();
        if roots > 256 {
            return not_taken(&format!("{roots} instances to capture"));
        }
        let Ok(subtrees) = capture_touched_instances(bridge, runtime, &plans) else {
            return not_taken("the capture failed");
        };
        let captured_ms = elapsed_ms(started);
        let classes = (subtrees.iter().flatten().flatten())
            .map(|(instance, _)| instance.class.as_str())
            .collect::<BTreeSet<_>>();
        let state = bridge
            .call_for_runtime_with_timeout(
                "getStudioChangeState",
                json!({"start": false, "verifyPushProof": self.proof, "scopePushProof": true,
                    "propertySchemaClasses": classes}),
                BridgeTarget::Edit,
                runtime,
                Some(Duration::from_secs(10)),
            )
            .and_then(|state| ensure_plugin_api_ok(&state).map(|()| state));
        let Ok(state) = state else {
            return not_taken("Studio's state did not read");
        };
        let since = (state["pushProofMatches"] != true)
            .then(|| ProofScope::parse(&state["pushProofScope"]));
        let premise = match &since {
            None => ScopedPremise::Held,
            Some(now) => {
                let pulls = (plans.iter())
                    .map(|(.., plan)| (plan.service.clone(), plan))
                    .collect();
                instance_premise(now, &pulls)
            }
        };
        if premise == ScopedPremise::False {
            return not_taken("Studio moved outside the captured instances during the pull");
        }
        let schemas = (state.get("propertySchemaByClass"))
            .map(|schemas| crate::roblox::schema::parse_property_schema_map(Some(schemas)));
        let Some(Ok(schemas)) = schemas else {
            return not_taken("Studio gave no property schemas");
        };
        let conversions = class_conversions(rbx_reflection_database::get()?, &schemas, classes);
        let mut written = Vec::new();
        for ((store, held, plan), subtrees) in plans.iter_mut().zip(&subtrees) {
            if let Some(ProofScope::Services { touched, .. }) = &since
                && let Some(Some(entries)) = touched.get(&plan.service)
                && let Err(reason) = plan.name_since(entries)
            {
                return not_taken(&reason);
            }
            let captured = (subtrees.iter())
                .map(|subtree| captured_instances(&plan.service, subtree, &conversions))
                .collect::<Result<Vec<_>>>();
            let Ok(captured) = captured else {
                return not_taken("a captured instance does not convert");
            };
            match plan.apply(&captured) {
                Ok(Some(bytes)) => written.push(((*store).clone(), *held, bytes)),
                Ok(None) => {}
                Err(reason) => return not_taken(&reason),
            }
        }
        let published = if written.is_empty() {
            crate::snapshot::export::record_bridge_sync_completion(bridge)
                .map(|()| crate::snapshot::export::PublishedProjectChanges::default())?
        } else {
            let mut reached = false;
            let check = || {
                reached = true;
                Ok(())
            };
            match publish_instance_stores(args, &self.services, &written, check, bridge) {
                Err(_) if !reached => {
                    return not_taken("nothing was written: the publish failed");
                }
                published => published?,
            }
        };
        log_global(
            5,
            format_args!(
                "[renium] baseline: pull scoped to instances: {roots} captured in {captured_ms:.1}ms, {} of {} stores written, {:.1}ms in all",
                written.len(),
                plans.len(),
                elapsed_ms(started)
            ),
        );
        Ok(Some((published, premise)))
    }
}

/// Publishes the stores `written`, each with the bytes the pull read and
/// those it writes, as a pull publishes: staged, with `check` before the
/// files are written, its undo entry and its completion.
#[cfg(windows)]
fn publish_instance_stores(
    args: &crate::cli::PullArgs,
    services: &BTreeSet<String>,
    written: &[(PathBuf, &Vec<u8>, Vec<u8>)],
    check: impl FnOnce() -> Result<()>,
    bridge: &BridgeServer,
) -> Result<crate::snapshot::export::PublishedProjectChanges> {
    use crate::snapshot::export::{collect_publish_hashes, file_publish_state};
    let root = &args.project_root;
    let services = services.iter().cloned().collect::<Vec<_>>();
    let mut stage = ExportProjectStage::create_for_comparison(root, &args.src_dir, &services)?;
    let paths = written
        .iter()
        .map(|(path, ..)| path.clone())
        .collect::<HashSet<_>>();
    stage.restrict_publish_paths(&paths);
    anyhow::ensure!(
        stage.publish_paths().len() == paths.len(),
        "A store lies outside the paths a pull publishes"
    );
    stage.capture_publish_baseline(root)?;
    let current = collect_publish_hashes(root, stage.publish_paths())?;
    for (path, held, bytes) in written {
        anyhow::ensure!(
            current.get(path) == Some(&file_publish_state(held)),
            "A store changed since the pull read it"
        );
        let staged = stage.project_root.join(path);
        if let Some(parent) = staged.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&staged, bytes)?;
    }
    let (published, _) = crate::snapshot::export::finish_export_publication(
        Some(stage),
        root,
        false,
        Some(&args.src_dir),
        check,
        || crate::snapshot::export::record_bridge_sync_completion(bridge),
    )?;
    Ok(published)
}

/// The instances under each root of `plans`, from one native capture of
/// them all, each root's breadth first.
#[cfg(windows)]
fn capture_touched_instances(
    bridge: &BridgeServer,
    runtime: &str,
    plans: &[(&PathBuf, &Vec<u8>, StoreInstancePull)],
) -> Result<Vec<Vec<CapturedSubtree>>> {
    use crate::studio::native::serializer;
    let roots = (plans.iter())
        .flat_map(|(.., plan)| &plan.roots)
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Ok(plans.iter().map(|_| Vec::new()).collect());
    }
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime)?;
    let title = serializer::target_name(pid, &info.place_name)?;
    let asked = (roots.iter())
        .map(|root| serializer::NativeCaptureRoot {
            path: serializer::NativePropertyPath {
                segments: &root.segments,
                ordinals: &root.ordinals,
            },
            descendants: true,
            expected_debug_id: &root.debug_id,
        })
        .collect::<Vec<_>>();
    let capture = serializer::capture_live_instances(pid, &title, &asked, Duration::from_secs(5))?;
    let mut subtrees = captured_subtrees(capture, asked.len())?.into_iter();
    Ok((plans.iter())
        .map(|(.., plan)| (&mut subtrees).take(plan.roots.len()).collect())
        .collect())
}

/// The instances of one root's capture, breadth first, each with its parent
/// among them.
#[cfg(windows)]
type CapturedSubtree = Vec<(rbx_binary::FlatInstance, Option<usize>)>;

/// The subtree each root of a native capture stands at the top of: the bytes
/// decoded as a capture of a service decodes them, each root told by its
/// UniqueId.
#[cfg(windows)]
fn captured_subtrees(
    capture: crate::studio::native::serializer::NativeCapture,
    roots: usize,
) -> Result<Vec<CapturedSubtree>> {
    use crate::studio::native::serializer::NativeCaptureRootResult;
    use rbx_dom_weak::types::{UniqueId, Variant};
    anyhow::ensure!(
        capture.roots.len() == roots
            && capture.identities.len() >= roots * 72
            && (capture.roots.iter().enumerate()).all(|(index, root)| {
                matches!(root, NativeCaptureRootResult::Captured { identity_row, .. } if *identity_row == index)
            }),
        "A touched instance is gone or stands elsewhere"
    );
    let flat = rbx_binary::Deserializer::new()
        .elide_defaults(true)
        .retain_defaults_for_properties(
            (crate::rbx::encode::RETAINED_DEFAULT_PROPERTIES.iter())
                .map(|name| name.to_string())
                .collect(),
        )
        .flat_property_filter(Arc::new(HashMap::new()))
        .deserialize_flat(std::io::Cursor::new(capture.bytes.as_slice()))?;
    anyhow::ensure!(
        flat.root_indices.len() == roots,
        "The capture holds other roots than asked"
    );
    let unique_id = |instance: &rbx_binary::FlatInstance| {
        instance
            .properties
            .iter()
            .find_map(|(name, value)| match value {
                Variant::UniqueId(id) if name.as_str() == "UniqueId" => Some(*id),
                _ => None,
            })
    };
    let mut children = vec![Vec::new(); flat.instances.len()];
    for (index, instance) in flat.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index {
            (children.get_mut(parent))
                .context("A captured instance names a parent the capture lacks")?
                .push(index);
        }
    }
    let orders = (capture.identities.chunks_exact(72).take(roots))
        .map(|row| {
            let word =
                |at: usize| u32::from_le_bytes([row[at], row[at + 1], row[at + 2], row[at + 3]]);
            let id = UniqueId::new(
                word(12),
                word(8),
                ((u64::from(word(0)) << 32) | u64::from(word(4))) as i64,
            );
            let mut found = (flat.root_indices.iter().copied())
                .filter(|index| unique_id(&flat.instances[*index]) == Some(id));
            let (Some(top), None) = (found.next(), found.next()) else {
                bail!("A captured root is not told by its UniqueId");
            };
            let mut order = vec![(top, None)];
            let mut at = 0;
            while let Some(&(index, _)) = order.get(at) {
                order.extend(children[index].iter().map(|child| (*child, Some(at))));
                at += 1;
            }
            Ok(order)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut instances = flat.instances.into_iter().map(Some).collect::<Vec<_>>();
    orders
        .into_iter()
        .map(|order| {
            (order.into_iter())
                .map(|(index, parent)| {
                    let instance = instances[index]
                        .take()
                        .context("Two captured roots share an instance")?;
                    Ok((instance, parent))
                })
                .collect()
        })
        .collect()
}

/// The captured instances of one store as a capture of its service converts
/// them, each class by its conversion in `conversions`.
#[cfg(windows)]
fn captured_instances(
    service: &str,
    subtree: &[(rbx_binary::FlatInstance, Option<usize>)],
    conversions: &HashMap<String, ClassConversion>,
) -> Result<Vec<CapturedInstance>> {
    let conversion = |class: &str| {
        conversions
            .get(class)
            .context("A captured class has no conversion")
    };
    let rows = converted_rows(service, subtree, |class| {
        conversion(class)
            .ok()
            .and_then(|conversion| conversion.filter.as_ref())
    })?;
    (subtree.iter().zip(rows))
        .map(|((instance, parent), row)| {
            Ok(CapturedInstance {
                name: instance.name.clone(),
                class_name: instance.class.to_string(),
                parent: *parent,
                values: CapturedValues {
                    properties: row.properties,
                    attributes: row.attributes,
                    overlaid: conversion(instance.class.as_str())?.overlaid.clone(),
                },
            })
        })
        .collect()
}

/// The rows of a store of the captured instances alone under their
/// service's root, each converted with the native filter `filter` gives
/// its class as `studio::native::editor` converts a service's capture.
#[cfg(windows)]
fn converted_rows<'f>(
    service: &str,
    subtree: &[(rbx_binary::FlatInstance, Option<usize>)],
    filter: impl Fn(&str) -> Option<&'f crate::rbx::decode::NativePropertyFilter>,
) -> Result<Vec<SettingsBytecodeInstance>> {
    use crate::snapshot::types::{
        ExportedSnapshotParts, NativeSettingsProperty, NativeSettingsValue, SnapshotInstance,
    };
    let database = rbx_reflection_database::get()?;
    let refs = crate::rbx::model::BytecodeModelImportRefs::default();
    let mut instances = vec![SnapshotInstance {
        name: service.to_string(),
        class_name: service.into(),
        instance_index: Some(1),
        ..Default::default()
    }];
    let mut natives = vec![Vec::new()];
    for (index, (instance, parent)) in subtree.iter().enumerate() {
        let class = instance.class.as_str();
        let native_filter = filter(class);
        let (mut native, mut properties, attributes, source) =
            crate::rbx::decode::rbx_properties_to_native_settings_records(
                class,
                instance
                    .properties
                    .iter()
                    .map(|(name, value)| (name, value)),
                database,
                &refs,
                native_filter,
            );
        if native_filter.is_some_and(|filter| filter.reconstruct_decal_color_map) {
            let value = (properties.get("TextureContent").cloned())
                .unwrap_or_else(|| Value::String(String::new()));
            properties.insert("ColorMapContent".to_string(), value);
        }
        if native_filter.is_some_and(|filter| filter.reconstruct_weld_enabled)
            && let Some(raw) = properties.remove("State")
        {
            let state = crate::rbx::encode::json_i64(&raw)
                .context("WeldConstraint.State was not an integer")?;
            native.push(NativeSettingsProperty {
                name: "State".into(),
                value: NativeSettingsValue::Int(state),
            });
            if state == 0 {
                properties.insert("Enabled".to_string(), Value::Bool(false));
            }
        }
        if let Some(source) = source {
            properties.insert("Source".to_string(), Value::String(source));
        }
        instances.push(SnapshotInstance {
            name: instance.name.clone(),
            class_name: instance.class,
            properties,
            attributes,
            instance_index: Some(index + 2),
            parent_index: Some(parent.map_or(1, |parent| parent + 2)),
            ..Default::default()
        });
        natives.push(native);
    }
    let state = crate::snapshot::export::exported_parts_to_service_state(
        service,
        ExportedSnapshotParts {
            class_defaults: Value::Object(Map::new()),
            instances,
            native_properties_by_instance: Some(natives),
        },
    )?;
    anyhow::ensure!(
        crate::settings::bytecode::pulled_digest_from_service_state(&state)?
            .rows
            .encodes_as_its_document(),
        "A captured instance encodes otherwise than its document"
    );
    let document = crate::settings::bytecode::settings_bytecode_from_service_state(&state)?;
    anyhow::ensure!(
        document.instances.len() == subtree.len() + 1
            && (document.instances[1..].iter().zip(subtree)).all(|(row, (instance, parent))| {
                row.name == instance.name
                    && row.class_name == instance.class.as_str()
                    && row.parent_index == Some(parent.map_or(0, |parent| parent + 1))
            }),
        "The captured instances convert in another order"
    );
    Ok(document.instances.into_iter().skip(1).collect())
}

#[cfg(test)]
#[path = "instance_pull_tests.rs"]
mod instance_pull_tests;
