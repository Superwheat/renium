use super::*;

fn retained_paths(dom: &rbx_dom_weak::WeakDom) -> Result<Vec<[String; 2]>> {
    let mut paths = vec![
        ["StarterPlayer", "StarterPlayerScripts"],
        ["StarterPlayer", "StarterCharacterScripts"],
        ["TestService", "LuauLSP_Settings"],
        ["TextChatService", "ChatWindowConfiguration"],
        ["TextChatService", "ChatInputBarConfiguration"],
        ["TextChatService", "BubbleChatConfiguration"],
        ["TextChatService", "ChannelTabsConfiguration"],
        ["Workspace", "Camera"],
        ["Workspace", "Terrain"],
    ].into_iter().map(|[service, name]| [service.to_owned(), name.to_owned()]).collect::<Vec<_>>();
    if std::env::var("RENIUM_RETAINED_EXTRA_FOLDERS").as_deref() == Ok("1") {
        let mut added = 0;
        for root in dom.root().children() {
            let service = dom.get_by_ref(*root).unwrap();
            for child in service.children() {
                let node = dom.get_by_ref(*child).unwrap();
                if node.class.as_str() == "Folder" && added < 2 {
                    paths.push([service.class.to_string(), node.name.clone()]);
                    added += 1;
                }
            }
        }
        ensure!(added == 2, "Fixture lacks two Folder roots for shared-class retention");
    }
    Ok(paths)
}

pub(super) struct PreparedInput {
    pub path: PathBuf,
    dom: rbx_dom_weak::WeakDom,
    bindings: Vec<rbx_binary::SerializedInstanceBinding>,
}

pub(super) fn prepare_input(project: &Path, title: &str, bytes: &[u8]) -> Result<PreparedInput> {
    let rollback = std::env::var("RENIUM_RETAINED_ROLLBACK").as_deref() == Ok("1");
    let backup = if rollback {
        let bytes = fs::read(project.join(format!("audit/native-retained-reader/{title}.baseline.rbxl")))?;
        use sha2::{Digest, Sha256};
        ensure!(format!("{:x}", Sha256::digest(&bytes)) == std::env::var("RENIUM_RETAINED_ROLLBACK_SHA256")?,
            "Rollback baseline hash differs");
        Some(bytes)
    } else { None };
    let dom = rbx_binary::from_reader(backup.as_deref().unwrap_or(bytes))?;
    let run = std::env::var("RENIUM_RETAINED_RUN").unwrap_or_else(|_| "1".into()).parse::<u16>()?;
    ensure!((1..=100).contains(&run),"Invalid retained reader run");
    let label = if run == 1 { title.to_owned() } else { format!("{title}.run{run}.rbxl") };
    let output = project.join(format!("audit/native-retained-input-{label}"));
    let mut requested = HashMap::new();
    let reference_camera = std::env::var("RENIUM_RETAINED_REFERENCE_CAMERA").as_deref() == Ok("1");
    for path in retained_paths(&dom)? {
        let id = crate::rbx::model::rbx_dom_instance_by_path_unique(&dom, &path, &[])?;
        let mode = if reference_camera && dom.get_by_ref(id).unwrap().class.as_str() == "Camera" {
            rbx_binary::InstanceBindingMode::ReferenceOnly
        } else { rbx_binary::InstanceBindingMode::Replace };
        requested.insert(id, mode);
    }
    let bindings = rbx_binary::Serializer::new().serialize_with_bindings(
        std::io::BufWriter::new(fs::OpenOptions::new().write(true).create_new(true).open(&output)?),
        &dom,
        dom.root().children(),
        &requested,
    )?;
    Ok(PreparedInput { path: output, dom, bindings })
}


pub(super) fn configure(
    params: &mut [u8], memory: &ProcessMemory, studio: &ModuleEntry,
    model: &ActiveDataModel, image: &PeImage<'_>, exe: &[u8], input: &PreparedInput,
) -> Result<()> {
    let dom = &input.dom;
    // Use the same cached contract as ordinary connection preparation. The
    // audit runner still pins other unpromoted native helper contracts.
    let discovery_started = Instant::now();
    let trace = loader::prepare(&studio.path)?.factory;
    let (lookup, factory_return) = (trace.lookup, trace.origin);
    println!("Retained factory discovery: {:.3}ms",discovery_started.elapsed().as_secs_f64()*1000.);
    for rva in [lookup, trace.intern_name, trace.instance_reader] {
        let start = image.rva_to_offset(rva)?;
        let (begin, end) = image.function_bounds(start)?;
        ensure!(start == begin && memory.read_vec(studio.base + rva, end - begin)? == exe[begin..end], "Loaded factory contract changed");
    }
    put_u32(params, 20120, trace.context_bytes as u32);
    if std::env::var("RENIUM_RETAINED_ROLLBACK").as_deref() == Ok("1") {
        put_u32(params, 20124, 1);
    } else if std::env::var("RENIUM_RETAINED_BACKUP").as_deref() == Ok("1") {
        let serializer = trace_serializer(&studio.path, exe)?.serializer;
        let start = image.rva_to_offset(serializer)?;
        let (_, end) = image.function_bounds(start)?;
        ensure!(memory.read_vec(studio.base + serializer, end - start)? == exe[start..end], "Loaded retained serializer changed");
        put_u64(params, 20128, studio.base + serializer);
    }
    put_u64(params, 1368, studio.base + lookup);
    put_u64(params, 1376, studio.base + factory_return);
    put_u32(params, 1384, model.layout.class_descriptor as u32);
    let mut counts = HashMap::<String, u32>::new();
    for node in dom.descendants().skip(1) {
        *counts.entry(node.class.to_string()).or_default() += 1;
    }
    let paths = retained_paths(dom)?;
    let mut retained = HashSet::new();
    let mut containers = Vec::new();
    for source in dom.root().children().iter().map(|id| dom.get_by_ref(*id).unwrap()) {
        let target = model.roots.iter().find(|root|
            read_instance_class(memory, root.instance, model.layout).as_deref() == Some(source.class.as_str())
        ).context("Missing replacement service")?;
        containers.push(*target);
    }
    for (index, [service, name]) in paths.iter().enumerate() {
        let source_service = dom.root().children().iter().map(|id| dom.get_by_ref(*id).unwrap())
            .find(|node| node.class.as_str() == service).context("Missing input service")?;
        let source = source_service.children().iter().map(|id| dom.get_by_ref(*id).unwrap())
            .filter(|node| node.name == *name).collect::<Vec<_>>();
        ensure!(source.len() == 1, "Ambiguous retained input {service}.{name}");
        let source = source[0];
        let binding = input.bindings.iter().find(|binding| binding.referent == source.referent()).context("Missing serialized binding receipt")?;
        let target_service = model.roots.iter().find(|root|
            read_instance_class(memory, root.instance, model.layout).as_deref() == Some(service.as_str())
        ).context("Missing target service")?;
        let targets = read_children(memory, target_service.instance, model.layout).context("Missing target children")?
            .into_iter().filter(|child|
                (source.class.as_str() == "Camera" || read_instance_name(memory, child.instance, model.layout).as_deref() == Some(name.as_str()))
                && read_instance_class(memory, child.instance, model.layout).as_deref() == Some(source.class.as_str())
            ).collect::<Vec<_>>();
        ensure!(targets.len() == 1, "Ambiguous retained target {service}.{name}");
        let target = targets[0];
        retained.insert(target.instance);
        containers.push(target);
        ensure!(valid_owner(memory, target.owner, studio.base, studio.size), "Invalid retained owner");
        let at = 1392 + index * 24;
        put_u64(params, at, target.instance);
        put_u64(params, at + 8, target.owner);
        put_u32(params, at + 16, binding.class_count);
        put_u32(params, at + 20, binding.ordinal);
        println!("Retained reader binding: {service}.{name} class={} ordinal={}/{}", source.class, binding.ordinal, counts[source.class.as_str()]);
    }
    put_u32(params, 1388, paths.len() as u32);
    let parent = image.offset_to_rva(0x6d79e90)?;
    ensure!(memory.read_vec(studio.base + parent, 39)? == exe[0x6d79e90..0x6d79e90 + 39], "Native replacement setter changed");
    put_u64(params, 2168, studio.base + parent);
    let mut outgoing = Vec::new();
    for container in &containers {
        outgoing.extend(read_children(memory, container.instance, model.layout).context("Unreadable replacement container")?
            .into_iter().filter(|child| !retained.contains(&child.instance)));
    }
    ensure!(outgoing.len() + containers.len() <= 256, "Replacement forest exceeds isolated fixture bound");
    put_u32(params, 2176, outgoing.len() as u32);
    put_u32(params, 2180, containers.len() as u32);
    put_u32(params, 6292, model.layout.children as u32);
    for (index, root) in outgoing.iter().enumerate() {
        ensure!(valid_owner(memory, root.owner, studio.base, studio.size), "Invalid outgoing owner");
        put_u64(params, 2184 + index * 16, root.instance);
        put_u64(params, 2192 + index * 16, root.owner);
    }
    for (index, container) in containers.iter().enumerate() {
        let at = 2184 + (outgoing.len() + index) * 16;
        put_u64(params, at, container.instance);
        put_u64(params, at + 8, container.owner);
    }
    println!("Native outgoing forest: {} roots", outgoing.len());
    let intern = trace.intern_name;
    put_u64(params, 6280, studio.base + intern);
    let database = rbx_reflection_database::get()?;
    let mut classes = counts.iter().filter(|(name,_)| !database.classes.get(name.as_str())
        .is_some_and(|class| class.tags.contains(&rbx_reflection::ClassTag::Service))).collect::<Vec<_>>();
    classes.sort_unstable_by_key(|(name,_)| *name);
    ensure!(classes.len() <= 192,"Native class plan exceeds fixture bound");
    put_u32(params, 6288, classes.len() as u32);
    for (index,(name,count)) in classes.iter().enumerate() {
        ensure!(name.len() < 64,"Native class name too long");
        let at = 6296 + index * 72;
        params[at..at+name.len()].copy_from_slice(name.as_bytes());
        put_u32(params, at+64, **count);
    }
    println!("Native explicit-context factories: {} classes",classes.len());
    Ok(())
}
