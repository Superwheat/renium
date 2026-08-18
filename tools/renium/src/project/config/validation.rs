use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::adapter_format::adapter_format;
use super::projection::{
    find_document_target_optional, ignore_glob_pattern, normalize_property_map,
    projection_source_owner_paths, target_segments, validate_sync_middleware,
};
use super::{
    AdapterDirection, LoadedProject, MountOwnership, PROJECT_SCHEMA_VERSION, ProjectNode,
    ProjectTarget, absolute_path, compile_glob, instance_target_overlaps,
    project_adapter_output_path, project_script_naming, project_tree_nodes,
    projection_path_contains, projection_path_key, validate_direct_owner_source,
    validate_filesystem_target, validate_instance_target, validate_relative_portable_path,
};

type MountTarget<'a> = (&'a ProjectTarget, &'a Path, MountOwnership);

fn invalid_portable_filename_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
        )
}

pub(crate) fn validate_project(loaded: &LoadedProject) -> Result<()> {
    validate_project_header(loaded)?;
    let mount_targets = validate_mount_targets(loaded)?;
    let writable_tree_targets = validate_tree_targets(loaded)?;
    validate_owner_target_overlaps(&writable_tree_targets, &mount_targets)?;
    validate_adapters(loaded, &writable_tree_targets, &mount_targets)?;
    validate_sync_rules_and_filters(loaded)?;
    validate_explicit_source_owners(loaded, &writable_tree_targets, &mount_targets)?;
    validate_reverse_source_owners(loaded)?;
    Ok(())
}

fn validate_project_header(loaded: &LoadedProject) -> Result<()> {
    if loaded.project.schema_version != PROJECT_SCHEMA_VERSION {
        bail!(
            "{} uses schema version {}; this Renium build supports {}",
            loaded.path.display(),
            loaded.project.schema_version,
            PROJECT_SCHEMA_VERSION
        );
    }
    validate_relative_portable_path(&loaded.project.source_root, "sourceRoot")?;
    let source_root = loaded.root.join(&loaded.project.source_root);
    if source_root.exists() && !source_root.is_dir() {
        bail!("sourceRoot must be a directory: {}", source_root.display());
    }
    if let Some(target) = loaded.project.build_target.as_ref() {
        validate_instance_target(target, "buildTarget")?;
    }
    validate_project_node_reserved_keys(&loaded.project.root, "root")?;
    let root = &loaded.project.root;
    if root
        .class_name
        .as_deref()
        .is_some_and(|class_name| class_name != "DataModel")
    {
        bail!("Top-level root $className must be DataModel");
    }
    if root.id.is_some()
        || root.path.is_some()
        || !root.properties.is_empty()
        || !root.attributes.is_empty()
        || root.tags.is_some()
        || !root.children.is_empty()
    {
        bail!(
            "Top-level DataModel root metadata is not supported; configure services under tree instead"
        );
    }
    for (name, node) in &loaded.project.tree {
        validate_project_node_reserved_keys(node, name)?;
    }
    for (field, suffix) in [
        (
            "exportNaming.serverSuffix",
            loaded.project.export_naming.server_suffix.as_str(),
        ),
        (
            "exportNaming.clientSuffix",
            loaded.project.export_naming.client_suffix.as_str(),
        ),
        (
            "exportNaming.moduleSuffix",
            loaded.project.export_naming.module_suffix.as_str(),
        ),
        (
            "exportNaming.pluginSuffix",
            loaded.project.export_naming.plugin_suffix.as_str(),
        ),
        (
            "exportNaming.clientRunContextSuffix",
            loaded
                .project
                .export_naming
                .client_run_context_suffix
                .as_str(),
        ),
    ] {
        if suffix.chars().any(invalid_portable_filename_character) {
            bail!("{field} contains characters that are invalid in portable file names");
        }
    }
    if loaded.project.export_naming.server_suffix.is_empty()
        || loaded.project.export_naming.client_suffix.is_empty()
        || loaded.project.export_naming.plugin_suffix.is_empty()
        || loaded
            .project
            .export_naming
            .client_run_context_suffix
            .is_empty()
    {
        bail!("exportNaming script suffixes other than moduleSuffix cannot be empty");
    }
    let suffixes = [
        loaded.project.export_naming.server_suffix.as_str(),
        loaded.project.export_naming.client_suffix.as_str(),
        loaded.project.export_naming.module_suffix.as_str(),
        loaded.project.export_naming.plugin_suffix.as_str(),
        loaded
            .project
            .export_naming
            .client_run_context_suffix
            .as_str(),
    ];
    for left in 0..suffixes.len() {
        for right in left + 1..suffixes.len() {
            if suffixes[left].eq_ignore_ascii_case(suffixes[right]) {
                bail!("exportNaming suffixes must be distinct");
            }
        }
    }
    Ok(())
}

fn validate_mount_targets(loaded: &LoadedProject) -> Result<Vec<MountTarget<'_>>> {
    let mut mount_targets = Vec::<(&ProjectTarget, &Path, MountOwnership)>::new();
    for (index, mount) in loaded.project.mounts.iter().enumerate() {
        validate_relative_portable_path(&mount.source, &format!("mounts[{index}].source"))?;
        let mount_source = loaded.root.join(&mount.source);
        if mount_source.exists() || !mount.optional {
            validate_direct_owner_source(&mount_source, &format!("mounts[{index}].source"))?;
        }
        validate_instance_target(&mount.target, &format!("mounts[{index}].target"))?;
        validate_filesystem_target(&mount.target, &format!("mounts[{index}].target"))?;
        for (target, source, _) in &mount_targets {
            if instance_target_overlaps(target, &mount.target) {
                bail!(
                    "Mounts {} and {} overlap at Studio targets '{}' and '{}'; nested mount targets require explicit ownership provenance and are not supported",
                    source.display(),
                    mount.source.display(),
                    target,
                    mount.target
                );
            }
        }
        mount_targets.push((&mount.target, &mount.source, mount.ownership));
    }
    Ok(mount_targets)
}

fn validate_tree_targets(loaded: &LoadedProject) -> Result<Vec<ProjectTarget>> {
    let tree_nodes = project_tree_nodes(&loaded.project.tree);
    for (target, node) in &tree_nodes {
        if let Some(source) = node.path.as_deref() {
            validate_direct_owner_source(
                &loaded.root.join(source),
                &format!("tree target {}", target.join(".")),
            )?;
        }
    }
    let writable_tree_targets = tree_nodes
        .into_iter()
        .filter_map(|(target, node)| {
            node.path.map(|_| ProjectTarget::Structured {
                segments: target,
                ordinals: Vec::new(),
            })
        })
        .collect::<Vec<_>>();
    for (index, target) in writable_tree_targets.iter().enumerate() {
        validate_filesystem_target(target, &format!("tree writable target {index}"))?;
    }
    for left in 0..writable_tree_targets.len() {
        for right in left + 1..writable_tree_targets.len() {
            if instance_target_overlaps(&writable_tree_targets[left], &writable_tree_targets[right])
            {
                bail!(
                    "Writable tree targets '{}' and '{}' overlap",
                    writable_tree_targets[left],
                    writable_tree_targets[right]
                );
            }
        }
    }
    Ok(writable_tree_targets)
}

fn validate_owner_target_overlaps(
    writable_tree_targets: &[ProjectTarget],
    mount_targets: &[MountTarget<'_>],
) -> Result<()> {
    for tree_target in writable_tree_targets {
        for (mount_target, mount_source, _) in mount_targets {
            if instance_target_overlaps(tree_target, mount_target) {
                bail!(
                    "Writable tree target '{}' overlaps mount {} at '{}'",
                    tree_target,
                    mount_source.display(),
                    mount_target
                );
            }
        }
    }
    Ok(())
}

fn validate_adapters(
    loaded: &LoadedProject,
    writable_tree_targets: &[ProjectTarget],
    mount_targets: &[MountTarget<'_>],
) -> Result<()> {
    let mut outputs = BTreeSet::new();
    let mut adapter_targets = Vec::<(&ProjectTarget, &Path)>::new();
    let mut reverse_adapter_sources = BTreeMap::<PathBuf, ProjectTarget>::new();
    for (index, adapter) in loaded.project.adapters.iter().enumerate() {
        validate_relative_portable_path(&adapter.source, &format!("adapters[{index}].source"))?;
        validate_instance_target(&adapter.target, &format!("adapters[{index}].target"))?;
        validate_filesystem_target(&adapter.target, &format!("adapters[{index}].target"))?;
        for (target, source) in &adapter_targets {
            if instance_target_overlaps(target, &adapter.target) {
                bail!(
                    "Adapters {} and {} have overlapping Studio targets '{}' and '{}'",
                    source.display(),
                    adapter.source.display(),
                    target,
                    adapter.target
                );
            }
        }
        if writable_tree_targets
            .iter()
            .any(|target| instance_target_overlaps(target, &adapter.target))
            || mount_targets
                .iter()
                .any(|(target, _, _)| instance_target_overlaps(target, &adapter.target))
        {
            bail!(
                "Adapter {} overlaps a tree or mount owner at '{}'",
                adapter.source.display(),
                adapter.target
            );
        }
        adapter_targets.push((&adapter.target, &adapter.source));
        if let Some(output) = adapter.output.as_deref() {
            validate_relative_portable_path(output, &format!("adapters[{index}].output"))?;
        }
        let format = adapter_format(adapter)?;
        if let Some(output) = project_adapter_output_path(loaded, adapter)? {
            let output_key = projection_path_key(&output);
            let output_label = output.strip_prefix(&loaded.root).unwrap_or(&output);
            if !outputs.insert(output_key.clone()) {
                bail!("More than one adapter writes {}", output_label.display());
            }
            if loaded
                .project
                .adapters
                .iter()
                .any(|other| projection_path_key(&loaded.root.join(&other.source)) == output_key)
            {
                bail!(
                    "Adapter output {} collides with an adapter source",
                    output_label.display()
                );
            }
        }
        if adapter.direction != AdapterDirection::ToProject && !format.is_reversible() {
            bail!(
                "Adapter {} uses {} in {:?}; this format is not reversible and must use to-project",
                adapter.source.display(),
                format.as_str(),
                adapter.direction
            );
        }
        if format == super::adapter_format::AdapterFormat::ModelJson
            && target_segments(&adapter.target)?.len() < 2
        {
            bail!(
                "Model JSON adapter {} must target a child below a Studio service",
                adapter.source.display()
            );
        }
        if adapter.direction == AdapterDirection::ToProject
            && format.generates_module()
            && !adapter.generated
        {
            bail!(
                "Adapter {} uses a generated {} projection; set generated to true",
                adapter.source.display(),
                format.as_str()
            );
        }
        if adapter.direction != AdapterDirection::ToProject
            && mount_targets.iter().any(|(target, _, ownership)| {
                *ownership == MountOwnership::ReadOnly
                    && instance_target_overlaps(target, &adapter.target)
            })
        {
            bail!(
                "Adapter {} cannot sync back through a read-only mount",
                adapter.source.display()
            );
        }
        if adapter.direction != AdapterDirection::ToProject {
            let source = absolute_path(&loaded.root.join(&adapter.source));
            if let Some(previous) = reverse_adapter_sources.insert(source, adapter.target.clone()) {
                bail!(
                    "Adapters '{}' and '{}' both sync back to {}",
                    previous,
                    adapter.target,
                    adapter.source.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_sync_rules_and_filters(loaded: &LoadedProject) -> Result<()> {
    for (index, rule) in loaded.project.sync_rules.iter().enumerate() {
        compile_glob(&rule.pattern)
            .with_context(|| format!("Invalid syncRules[{index}].pattern '{}'", rule.pattern))?;
        if let Some(exclude) = rule.exclude.as_deref() {
            compile_glob(exclude)
                .with_context(|| format!("Invalid syncRules[{index}].exclude '{exclude}'"))?;
        }
        validate_sync_middleware(&rule.middleware)
            .with_context(|| format!("Invalid syncRules[{index}].use"))?;
        if let Some(suffix) = rule.suffix.as_deref()
            && (suffix.is_empty() || suffix.chars().any(invalid_portable_filename_character))
        {
            bail!("syncRules[{index}].suffix must be a non-empty file-name suffix");
        }
    }
    for (index, pattern) in loaded.project.glob_ignore_paths.iter().enumerate() {
        let pattern = ignore_glob_pattern(pattern)?;
        compile_glob(pattern)
            .with_context(|| format!("Invalid globIgnorePaths[{index}] '{pattern}'"))?;
    }
    for (index, rule) in loaded.project.filters.iter().enumerate() {
        if let Some(pattern) = rule.glob.as_deref() {
            compile_glob(pattern)
                .with_context(|| format!("Invalid filters[{index}].glob '{pattern}'"))?;
        }
        if rule.glob.is_none()
            && rule.name.is_none()
            && rule.class.is_none()
            && rule.tag.is_none()
            && rule.attribute.is_none()
            && rule.property.is_none()
            && rule.id.is_none()
        {
            bail!("filters[{index}] has no matching condition");
        }
    }
    Ok(())
}

fn validate_explicit_source_owners(
    loaded: &LoadedProject,
    writable_tree_targets: &[ProjectTarget],
    mount_targets: &[MountTarget<'_>],
) -> Result<()> {
    let mut explicit_targets = writable_tree_targets
        .iter()
        .map(ProjectTarget::segments)
        .chain(mount_targets.iter().map(|(target, _, _)| target.segments()))
        .chain(
            loaded
                .project
                .adapters
                .iter()
                .filter(|adapter| adapter.direction != AdapterDirection::FromProject)
                .map(|adapter| adapter.target.segments()),
        )
        .collect::<Vec<_>>();
    explicit_targets.sort();
    explicit_targets.dedup();
    let source_root = loaded.root.join(&loaded.project.source_root);
    let source_naming = project_script_naming(&loaded.project);
    let mut claimed_sources = projection_source_owner_paths(loaded);
    for adapter in &loaded.project.adapters {
        if let Some(output) = project_adapter_output_path(loaded, adapter)? {
            claimed_sources.push(absolute_path(&output));
        }
    }
    let mut source_documents = HashMap::new();
    for target in explicit_targets {
        let Some(service) = target.first() else {
            continue;
        };
        let service_dir = source_root.join(service);
        if !service_dir.is_dir() {
            continue;
        }
        if !source_documents.contains_key(service) {
            source_documents.insert(
                service.clone(),
                crate::rbx::model::source_structure_settings_document(
                    &service_dir,
                    service,
                    &source_naming,
                    &claimed_sources,
                )?,
            );
        }
        if find_document_target_optional(&source_documents[service], &target)?.is_some() {
            bail!(
                "Explicit project owner '{}' overlaps content already projected from sourceRoot; move or remove one owner",
                target.join(".")
            );
        }
    }
    Ok(())
}

fn validate_reverse_source_owners(loaded: &LoadedProject) -> Result<()> {
    let mut reverse_sources = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(target, node)| {
            node.path.map(|source| {
                (
                    format!("tree {}", target.join(".")),
                    absolute_path(&loaded.root.join(source)),
                )
            })
        })
        .chain(
            loaded
                .project
                .mounts
                .iter()
                .filter(|mount| mount.ownership != MountOwnership::ReadOnly)
                .map(|mount| {
                    (
                        format!("mount {}", mount.target),
                        absolute_path(&loaded.root.join(&mount.source)),
                    )
                }),
        )
        .chain(
            loaded
                .project
                .adapters
                .iter()
                .filter(|adapter| adapter.direction != AdapterDirection::ToProject)
                .map(|adapter| {
                    (
                        format!("adapter {}", adapter.target),
                        absolute_path(&loaded.root.join(&adapter.source)),
                    )
                }),
        )
        .collect::<Vec<_>>();
    reverse_sources.sort_by_key(|entry| projection_path_key(&entry.1));
    for left in 0..reverse_sources.len() {
        for right in left + 1..reverse_sources.len() {
            let (left_name, left_path) = &reverse_sources[left];
            let (right_name, right_path) = &reverse_sources[right];
            if projection_path_contains(left_path, right_path)
                || projection_path_contains(right_path, left_path)
            {
                bail!(
                    "{left_name} and {right_name} have overlapping writable source paths '{}' and '{}'",
                    left_path.display(),
                    right_path.display()
                );
            }
        }
    }
    Ok(())
}

pub(super) fn validate_nested_project(loaded: &LoadedProject) -> Result<()> {
    if loaded.project.root.path.is_some() || !loaded.project.root.children.is_empty() {
        bail!(
            "Nested project root $path and child nodes are unsupported; put mounted content in sourceRoot or tree"
        );
    }
    let root_class = loaded
        .project
        .root
        .class_name
        .as_deref()
        .unwrap_or("Folder");
    normalize_property_map(Some(root_class), &loaded.project.root.properties).with_context(
        || {
            format!(
                "Invalid nested root properties in {}",
                loaded.path.display()
            )
        },
    )?;
    normalize_property_map(None, &loaded.project.root.attributes).with_context(|| {
        format!(
            "Invalid nested root attributes in {}",
            loaded.path.display()
        )
    })?;
    let mut top_level = LoadedProject {
        path: loaded.path.clone(),
        root: loaded.root.clone(),
        project: loaded.project.clone(),
    };
    top_level.project.root = ProjectNode::default();
    validate_project(&top_level)
}

fn validate_project_node_reserved_keys(node: &ProjectNode, path: &str) -> Result<()> {
    for (name, value) in &node.children {
        if name.starts_with('$') {
            bail!("Unknown reserved project-node key '{name}' at '{path}'");
        }
        let child: ProjectNode = serde_json::from_value(value.clone())
            .with_context(|| format!("Project tree node '{path}.{name}' must be an object"))?;
        validate_project_node_reserved_keys(&child, &format!("{path}.{name}"))?;
    }
    Ok(())
}
