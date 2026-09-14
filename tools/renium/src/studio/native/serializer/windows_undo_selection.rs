//! Keep the Explorer selection unchanged when a waypoint is undone or redone.
//! ChangeHistoryService selects every instance its restore re-parents, so
//! undoing a sync that replaced hundreds of objects selected and expanded all
//! of them. The two Selection::set calls inside that restore routine become
//! no-ops for the connected process lifetime; the restore itself is untouched.
use super::*;
use crate::system::LockRecover;

const SELECTION_SET_DESCRIPTOR: &[u8] =
    b".?AV?$BoundFuncDesc@VSelection@RBX@@$$A6AXV?$shared_ptr@$$CBV?$vector@";
const HISTORY_VOID_DESCRIPTOR: &[u8] =
    b".?AV?$BoundFuncDesc@VChangeHistoryService@RBX@@$$A6AXXZ$0A@$0A@@Reflection@RBX@@";
const UNDO_NAME: &[u8] = b"Undo\0";
const NOP5: [u8; 5] = [0x0f, 0x1f, 0x44, 0x00, 0x00];

#[derive(Clone)]
struct Layout {
    sites: Vec<(usize, [u8; 5])>,
    image_stamp: [u32; 3],
}

struct CachedLayout {
    len: u64,
    modified: Option<SystemTime>,
    layout: Layout,
}
static LAYOUTS: OnceLock<Mutex<HashMap<PathBuf, CachedLayout>>> = OnceLock::new();

fn descriptor_vtable(image: &PeImage<'_>, name_prefix: &[u8]) -> Result<usize> {
    let names = memmem::find_iter(image.bytes, name_prefix).collect::<Vec<_>>();
    anyhow::ensure!(
        names.len() == 1,
        "Studio reflection descriptor {} is not unique",
        String::from_utf8_lossy(name_prefix)
    );
    let type_descriptor = image.offset_to_rva(
        names[0]
            .checked_sub(16)
            .context("Studio reflection descriptor precedes its type")?,
    )?;
    let rdata = image.section(b".rdata")?;
    let table = slice(image.bytes, rdata.raw_offset, rdata.raw_size)?;
    let mut vtables = Vec::new();
    for index in memmem::find_iter(table, &(type_descriptor as u32).to_le_bytes()) {
        if index < 12 || table[index - 12..index - 4] != [1, 0, 0, 0, 0, 0, 0, 0] {
            continue;
        }
        let locator = image.image_base + image.offset_to_rva(rdata.raw_offset + index - 12)?;
        for pointer in memmem::find_iter(table, &(locator as u64).to_le_bytes()) {
            if pointer % 8 == 0 {
                vtables.push(image.offset_to_rva(rdata.raw_offset + pointer + 8)?);
            }
        }
    }
    vtables.sort_unstable();
    vtables.dedup();
    anyhow::ensure!(
        vtables.len() == 1,
        "Studio reflection descriptor {} resolved {} vtables",
        String::from_utf8_lossy(name_prefix),
        vtables.len()
    );
    Ok(vtables[0])
}

fn rip_lea_target(image: &PeImage<'_>, offset: usize) -> Option<usize> {
    let bytes = image.bytes.get(offset..offset + 7)?;
    if !matches!(bytes[0], 0x48 | 0x4c) || bytes[1] != 0x8d || bytes[2] & 0xc7 != 0x05 {
        return None;
    }
    let displacement = i32::from_le_bytes(bytes[3..7].try_into().ok()?) as isize;
    let source = image.offset_to_rva(offset + 7).ok()? as isize;
    usize::try_from(source.checked_add(displacement)?).ok()
}

fn lea_xrefs(image: &PeImage<'_>, target: usize) -> Result<Vec<usize>> {
    let text = image.section(b".text")?;
    let code = slice(image.bytes, text.raw_offset, text.raw_size)?;
    Ok(memchr::memchr_iter(0x8d, code)
        .filter_map(|index| index.checked_sub(1))
        .map(|index| text.raw_offset + index)
        .filter(|offset| rip_lea_target(image, *offset) == Some(target))
        .collect())
}

fn preceding_lea(
    image: &PeImage<'_>,
    site: usize,
    accept: impl Fn(usize) -> bool,
) -> Option<usize> {
    (site.saturating_sub(0x80)..site)
        .rev()
        .filter_map(|offset| rip_lea_target(image, offset).filter(|target| accept(*target)))
        .next()
}

fn direct_calls(image: &PeImage<'_>, bounds: (usize, usize)) -> Vec<(usize, usize)> {
    let code = &image.bytes[bounds.0..bounds.1];
    memchr::memchr_iter(0xe8, code)
        .filter_map(|index| {
            let offset = bounds.0 + index;
            image
                .call_target(offset)
                .ok()
                .filter(|target| target & 0xf == 0)
                .map(|target| (offset, target))
        })
        .collect()
}

fn discover(image: &PeImage<'_>) -> Result<Layout> {
    let selection_set = {
        let vtable = descriptor_vtable(image, SELECTION_SET_DESCRIPTOR)?;
        let sites = lea_xrefs(image, vtable)?;
        anyhow::ensure!(
            sites.len() == 1,
            "Studio Selection.Set registration resolved {} sites",
            sites.len()
        );
        preceding_lea(image, sites[0], |target| {
            image.require_executable_rva(target).is_ok()
        })
        .context("Studio Selection.Set registration has no bound function")?
    };
    let undo = {
        let vtable = descriptor_vtable(image, HISTORY_VOID_DESCRIPTOR)?;
        let mut functions = Vec::new();
        for site in lea_xrefs(image, vtable)? {
            let named = preceding_lea(image, site, |target| {
                image
                    .rva_to_offset(target)
                    .ok()
                    .and_then(|offset| image.bytes.get(offset..offset + UNDO_NAME.len()))
                    == Some(UNDO_NAME)
            });
            if named.is_none() {
                continue;
            }
            if let Some(function) = preceding_lea(image, site, |target| {
                image.require_executable_rva(target).is_ok()
            }) {
                functions.push(function);
            }
        }
        functions.sort_unstable();
        functions.dedup();
        anyhow::ensure!(
            functions.len() == 1,
            "Studio ChangeHistoryService.Undo resolved {} functions",
            functions.len()
        );
        functions[0]
    };
    let mut visited = HashSet::new();
    let mut pending = vec![(undo, 0usize)];
    let mut restore_sites = Vec::new();
    while let Some((function, depth)) = pending.pop() {
        if !visited.insert(function) {
            continue;
        }
        let bounds = match image.function_bounds(image.rva_to_offset(function)?) {
            Ok(bounds) => bounds,
            Err(error) if depth > 0 => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut sites = Vec::new();
        for (offset, target) in direct_calls(image, bounds) {
            if target == selection_set {
                sites.push(offset);
            } else if depth < 2 {
                pending.push((target, depth + 1));
            }
        }
        if !sites.is_empty() {
            restore_sites.push((function, sites));
        }
    }
    anyhow::ensure!(
        restore_sites.len() == 1 && restore_sites[0].1.len() == 2,
        "Studio undo restore selection resolved {} functions",
        restore_sites.len()
    );
    let sites = restore_sites
        .remove(0)
        .1
        .into_iter()
        .map(|offset| {
            let mut original = [0; 5];
            original.copy_from_slice(slice(image.bytes, offset, 5)?);
            Ok((image.offset_to_rva(offset)?, original))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Layout {
        sites,
        image_stamp: image.image_stamp,
    })
}

fn layout(path: &Path) -> Result<Layout> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().ok();
    let cache = LAYOUTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock_recover()
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
    {
        return Ok(cached.layout.clone());
    }
    let bytes = fs::read(path)?;
    let layout = discover(&PeImage::parse(&bytes)?)?;
    cache.lock_recover().insert(
        path.to_owned(),
        CachedLayout {
            len: metadata.len(),
            modified,
            layout: layout.clone(),
        },
    );
    Ok(layout)
}

// A process patch like the package notice flag: it survives reconnects and
// daemon restarts until Studio exits, and is idempotent.
pub(crate) fn keep_selection_across_undo(pid: u32) -> Result<()> {
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Studio module was not found for the undo selection patch")?;
    let layout = layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    for (rva, original) in &layout.sites {
        let address = studio.base + rva;
        let current = memory.read_vec(address, 5)?;
        if current == NOP5 {
            continue;
        }
        anyhow::ensure!(
            current == original,
            "Studio undo restore call site changed in memory"
        );
        memory.write(address, &NOP5)?;
        anyhow::ensure!(
            memory.read_vec(address, 5)? == NOP5,
            "Studio undo selection patch was not retained"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_studio_resolves_two_restore_selection_calls() {
        let Some(path) = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("Roblox").join("Versions"))
            .and_then(|versions| fs::read_dir(versions).ok())
            .and_then(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path().join("RobloxStudioBeta.exe"))
                    .find(|candidate| candidate.is_file())
            })
        else {
            return;
        };
        let bytes = fs::read(&path).unwrap();
        let image = PeImage::parse(&bytes).unwrap();
        let layout = discover(&image).unwrap();
        assert_eq!(layout.sites.len(), 2);
        for (rva, original) in &layout.sites {
            assert_eq!(original[0], 0xe8, "{rva:#x}");
            image.require_executable_rva(*rva).unwrap();
        }
    }
}
