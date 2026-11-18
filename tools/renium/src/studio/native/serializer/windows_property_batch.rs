//! One process/document lookup and hierarchy index for a group of property targets.
//! Targets are still validated by the existing engine task on every operation.
use super::*;
use std::rc::Rc;

pub(crate) struct NativePropertyPath<'a> {
    pub(crate) segments: &'a [String],
    pub(crate) ordinals: &'a [usize],
}

#[derive(Clone, Copy)]
struct Binding {
    descriptor: usize,
    table: usize,
    getter: usize,
    setter: usize,
    identity: usize,
    identity_getter: usize,
}

pub(crate) struct NativePropertyBatch {
    memory: Rc<ProcessMemory>,
    studio: ModuleEntry,
    layout: PackageLayout,
    model: ActiveDataModel,
    targets: Vec<Vec<SharedEntry>>,
    common: Vec<u8>,
    entrypoint: usize,
    classes: HashMap<usize, String>,
    identities: HashMap<usize, (usize, usize)>,
    bindings: HashMap<(usize, String), Binding>,
}

impl NativePropertyBatch {
    pub(crate) fn new(
        pid: u32,
        title: &str,
        paths: &[NativePropertyPath<'_>],
        timeout: Duration,
        model_path: Option<&[String]>,
    ) -> Result<Self> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut current = modules(pid)?;
        let position = current
            .iter()
            .position(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
            .context("Roblox Studio module was not found; reconnect the selected place")?;
        let studio = &current[position];
        let layout = package_layout(&studio.path)?;
        let memory = Rc::new(ProcessMemory::open(pid)?);
        verify_loaded_image(&memory, studio, layout.image_stamp)?;
        let model =
            active_data_model_for_path(pid, &memory, studio, layout.data, title, model_path)?;
        let targets = resolve_paths(&memory, &model, paths, deadline)?;
        // These positions belong to Renium's PropertyReadParams ABI, not Studio.
        // The Studio offsets and entrypoints are discovered and validated above.
        let mut common = vec![0; SIZE];
        for (offset, value) in [
            (
                0,
                data_model_task_context(&memory, studio, &layout, &model)?,
            ),
            (8, studio.base + layout.submit_task),
            (32, model.owner),
            (72, model.layout.class_descriptor),
            (131488, parent_offset(&memory, &model)?),
            (132024, model.layout.self_pointer),
        ] {
            put_u64(&mut common, offset, value);
        }
        memory.authorize(NativeOperation::DescriptorBinding, title)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Native target preparation exceeded its deadline; retry the push")?;
        let helper = ensure_helper_loaded_with_timeout(
            pid,
            &memory,
            &current,
            remaining.as_millis().clamp(1, 3000) as u32,
        )?;
        let entrypoint = helper + helper_export_rva("ReniumReadProperty")?;
        crate::app::profile::record_between(
            "native property batch target preparation",
            started,
            Instant::now(),
        );
        crate::app::output::log_global(
            5,
            format_args!(
                "[renium] native property batch: targets={} preparation_ms={:.3}",
                paths.len(),
                started.elapsed().as_secs_f64() * 1000.0
            ),
        );
        Ok(Self {
            memory,
            studio: current.remove(position),
            layout,
            model,
            targets,
            common,
            entrypoint,
            classes: HashMap::new(),
            identities: HashMap::new(),
            bindings: HashMap::new(),
        })
    }

    fn binding(&mut self, instance: usize, class: usize, property: &str) -> Result<Binding> {
        let key = (class, property.to_owned());
        if let Some(binding) = self.bindings.get(&key) {
            return Ok(*binding);
        }
        let descriptor =
            find_class_member_descriptor(&self.memory, instance, self.model.layout, property)?;
        let (getter, setter) =
            reflection_functions(&self.memory, &self.studio, &self.layout, descriptor)?;
        let (identity, identity_getter) = match self.identities.get(&class) {
            Some(binding) => *binding,
            None => {
                let found = identity_binding(
                    &self.memory,
                    &self.studio,
                    &self.layout,
                    &self.model,
                    instance,
                )?;
                self.identities.insert(class, found);
                found
            }
        };
        let binding = Binding {
            descriptor,
            table: self.memory.read_u64(descriptor)? as usize,
            getter,
            setter,
            identity,
            identity_getter,
        };
        self.bindings.insert(key, binding);
        Ok(binding)
    }

    pub(crate) fn prepare(
        &mut self,
        index: usize,
        property: &str,
        timeout: Duration,
    ) -> Result<NativeProperty> {
        self.prepare_read(index, property, timeout, None)
            .map(|(property, _)| property)
    }

    pub(super) fn prepare_read(
        &mut self,
        index: usize,
        property: &str,
        timeout: Duration,
        read_class: Option<&str>,
    ) -> Result<(NativeProperty, String)> {
        let started = Instant::now();
        let ancestors = self
            .targets
            .get(index)
            .context("Native target is absent from the prepared batch; retry the push")?
            .clone();
        let entry = *ancestors
            .last()
            .context("Native target has no ancestry; retry the push")?;
        let class = self
            .memory
            .read_u64(entry.instance + self.model.layout.class_descriptor)?
            as usize;
        let class_name = match self.classes.get(&class) {
            Some(name) => name.clone(),
            None => {
                let name = read_instance_class(&self.memory, entry.instance, self.model.layout)
                    .context("Property target class is unavailable; retry the push")?;
                self.classes.insert(class, name.clone());
                name
            }
        };
        anyhow::ensure!(
            read_class.is_none_or(|expected| class_name == expected),
            "Native snapshot target changed class"
        );
        let binding = self.binding(entry.instance, class, property)?;
        let mut parameters = self.common.clone();
        for (offset, value) in [
            (16, entry.instance),
            (24, entry.owner),
            (40, binding.descriptor),
            (48, binding.getter),
            (56, class),
            (64, binding.table),
            (65888, binding.identity),
            (65896, binding.identity_getter),
            (65904, binding.setter),
        ] {
            put_u64(&mut parameters, offset, value);
        }
        put_u32(&mut parameters, 131496, ancestors.len() as u32 + 1);
        for (index, ancestor) in ancestors.iter().rev().enumerate() {
            put_u64(&mut parameters, 131504 + index * 8, ancestor.instance);
        }
        put_u64(
            &mut parameters,
            131504 + ancestors.len() * 8,
            self.model.outer + self.model.layout.data_model_instance,
        );
        let mut prepared = NativeProperty {
            class_name,
            property: property.into(),
            instance_id: String::new(),
            memory: Rc::clone(&self.memory),
            parameters,
            entrypoint: self.entrypoint,
            deadline: started + timeout,
        };
        prepared.invoke(if read_class.is_some() { 3 } else { 0 })?;
        let identity: [u8; 16] = prepared.parameters[IDENTITY..IDENTITY + 16].try_into()?;
        anyhow::ensure!(
            identity != [0; 16],
            "Studio target has no stable instance identity; retry after Studio settles"
        );
        prepared.instance_id = identity.iter().map(|byte| format!("{byte:02x}")).collect();
        prepared.parameters[EXPECTED_IDENTITY..EXPECTED_IDENTITY + 16].copy_from_slice(&identity);
        let value = if read_class.is_some() {
            prepared.value()?
        } else {
            String::new()
        };
        crate::app::profile::record_between(
            "native property binding and identity preparation",
            started,
            Instant::now(),
        );
        Ok((prepared, value))
    }
}

fn resolve_paths(
    memory: &ProcessMemory,
    model: &ActiveDataModel,
    paths: &[NativePropertyPath<'_>],
    deadline: Instant,
) -> Result<Vec<Vec<SharedEntry>>> {
    let roots = model
        .roots
        .iter()
        .map(|entry| {
            // Unreadable unrelated roots never participate in a path match.
            // A requested root still has to resolve uniquely below.
            let class = read_instance_class(memory, entry.instance, model.layout);
            let name = read_instance_name(memory, entry.instance, model.layout);
            (*entry, class, name)
        })
        .collect::<Vec<_>>();
    let database = rbx_reflection_database::get()?;
    resolve_indexed(
        paths,
        deadline,
        |name| {
            let service = database
                .classes
                .get(name)
                .is_some_and(|class| class.tags.contains(&rbx_reflection::ClassTag::Service));
            Ok(matching_root_labels(&roots, name, service))
        },
        |parent| {
            read_children(memory, parent, model.layout)
                .context("Native target hierarchy changed during lookup; retry the push")?
                .into_iter()
                .map(|entry| {
                    let name = read_instance_name(memory, entry.instance, model.layout)
                        .context("Native target child name became unavailable; retry the push")?;
                    Ok((name, entry))
                })
                .collect()
        },
    )
}

fn matching_root_labels(
    roots: &[(SharedEntry, Option<String>, Option<String>)],
    name: &str,
    service: bool,
) -> Vec<SharedEntry> {
    roots
        .iter()
        .filter(|(_, class, label)| (if service { class } else { label }).as_deref() == Some(name))
        .map(|(entry, _, _)| *entry)
        .collect()
}

fn resolve_indexed(
    paths: &[NativePropertyPath<'_>],
    deadline: Instant,
    mut roots: impl FnMut(&str) -> Result<Vec<SharedEntry>>,
    mut read: impl FnMut(usize) -> Result<Vec<(String, SharedEntry)>>,
) -> Result<Vec<Vec<SharedEntry>>> {
    let mut root_matches = HashMap::<String, Vec<SharedEntry>>::new();
    let mut children = HashMap::<usize, HashMap<String, Vec<SharedEntry>>>::new();
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        validate_property_path(path.segments, path.ordinals)?;
        anyhow::ensure!(
            Instant::now() < deadline,
            "Native target lookup exceeded its deadline; retry the push"
        );
        let mut ancestors = Vec::<SharedEntry>::with_capacity(path.segments.len());
        for (index, name) in path.segments.iter().enumerate() {
            let matches = if index == 0 {
                if !root_matches.contains_key(name) {
                    root_matches.insert(name.clone(), roots(name)?);
                }
                root_matches.get(name).unwrap().as_slice()
            } else {
                let parent = ancestors.last().unwrap().instance;
                if let std::collections::hash_map::Entry::Vacant(slot) = children.entry(parent) {
                    let mut named = HashMap::<String, Vec<SharedEntry>>::new();
                    for (name, child) in read(parent)? {
                        named.entry(name).or_default().push(child);
                    }
                    slot.insert(named);
                }
                children[&parent]
                    .get(name)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            };
            let entry = match path.ordinals.get(index) {
                Some(ordinal) => matches.get(ordinal - 1).copied(),
                None if matches.len() == 1 => Some(matches[0]),
                _ => None,
            }.with_context(|| format!("Native target segment '{name}' has {} matches; retry with the current path and ordinals", matches.len()))?;
            ancestors.push(entry);
        }
        result.push(ancestors);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_lookup_ignores_unreadable_unrelated_labels() -> Result<()> {
        // Synthetic objects in this test process exercise the real memory
        // readers and resolve_paths. No Studio process or installed layout.
        let string = |text: &str| {
            let mut bytes = Box::new([0u8; 32]);
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            bytes[16..24].copy_from_slice(&(text.len() as u64).to_le_bytes());
            bytes[24..32].copy_from_slice(&15u64.to_le_bytes());
            bytes
        };
        let class = string("Workspace");
        let label = string("ordinary");
        let descriptor = Box::new([0usize, class.as_ptr() as usize]);
        let service = Box::new([descriptor.as_ptr() as usize, 0]);
        let ordinary = Box::new([0usize, label.as_ptr() as usize]);
        let unreadable = Box::new([0usize; 2]);
        let model = ActiveDataModel {
            outer: 0,
            owner: 0,
            roots: [&unreadable, &service, &ordinary]
                .into_iter()
                .map(|row| SharedEntry {
                    instance: row.as_ptr() as usize,
                    owner: 0,
                })
                .collect(),
            layout: InstanceLayout {
                class_descriptor: 0,
                name: 8,
                data_model_instance: 0,
                self_pointer: 0,
                children: 0,
            },
        };
        let memory = ProcessMemory::open_read_only(std::process::id())?;
        for (name, expected) in [
            ("Workspace", service.as_ptr() as usize),
            ("ordinary", ordinary.as_ptr() as usize),
        ] {
            let names = vec![name.to_string()];
            let found = resolve_paths(
                &memory,
                &model,
                &[NativePropertyPath {
                    segments: &names,
                    ordinals: &[1],
                }],
                Instant::now() + Duration::from_secs(1),
            )?;
            assert_eq!(found[0][0].instance, expected);
        }
        assert!(
            resolve_paths(
                &memory,
                &model,
                &[NativePropertyPath {
                    segments: &["missing".into()],
                    ordinals: &[1]
                }],
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn batch_paths_scan_shared_ancestors_once_and_require_exact_ordinals() -> Result<()> {
        let names = vec!["Workspace".into(), "same".into(), "leaf".into()];
        let first = vec![1, 1, 1];
        let second = vec![1, 2, 1];
        let paths = [
            NativePropertyPath {
                segments: &names,
                ordinals: &first,
            },
            NativePropertyPath {
                segments: &names,
                ordinals: &second,
            },
            NativePropertyPath {
                segments: &names,
                ordinals: &first,
            },
        ];
        let entry = |instance| SharedEntry {
            instance,
            owner: instance + 100,
        };
        let mut scans = HashMap::<usize, usize>::new();
        let found = resolve_indexed(
            &paths,
            Instant::now() + Duration::from_secs(1),
            |name| {
                assert_eq!(name, "Workspace");
                Ok(vec![entry(1)])
            },
            |parent| {
                *scans.entry(parent).or_default() += 1;
                Ok(match parent {
                    1 => vec![("same".into(), entry(2)), ("same".into(), entry(3))],
                    2 => vec![("leaf".into(), entry(4))],
                    3 => vec![("leaf".into(), entry(5))],
                    _ => unreachable!(),
                })
            },
        )?;
        assert_eq!(
            found
                .iter()
                .map(|path| path.last().unwrap().instance)
                .collect::<Vec<_>>(),
            [4, 5, 4]
        );
        assert_eq!(scans, HashMap::from([(1, 1), (2, 1), (3, 1)]));
        for ordinals in [vec![], vec![1, 3, 1], vec![1, 0, 1]] {
            assert!(
                resolve_indexed(
                    &[NativePropertyPath {
                        segments: &names,
                        ordinals: &ordinals
                    }],
                    Instant::now() + Duration::from_secs(1),
                    |_| Ok(vec![entry(1)]),
                    |_| Ok(vec![("same".into(), entry(2)), ("same".into(), entry(3))])
                )
                .is_err()
            );
        }
        assert!(
            resolve_indexed(
                &paths,
                Instant::now() - Duration::from_secs(1),
                |_| panic!(),
                |_| panic!()
            )
            .is_err()
        );
        assert!(
            resolve_indexed(
                &paths,
                Instant::now() + Duration::from_secs(1),
                |_| Ok(vec![entry(1)]),
                |_| bail!("hierarchy changed")
            )
            .is_err()
        );
        Ok(())
    }
}
