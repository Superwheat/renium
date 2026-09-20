//! Keep the Explorer selection unchanged when a waypoint is undone or redone.
//! ChangeHistoryService selects every instance its restore re-parents, so
//! undoing a sync that replaced hundreds of objects selected and expanded all
//! of them. The helper turns the two Selection::set branches inside that
//! restore routine into no-ops for the connected process lifetime.
use super::*;
use crate::system::LockRecover;

const KEEP_SELECTION_REQUEST: u32 = 6;
const RESET_WAYPOINTS_NAME: &[u8] = b"\0ResetWaypoints\0";
const UNDO_NAME: &[u8] = b"\0Undo\0";
const TERRAIN_SELECTION_NAME: &[u8] = b"\0SetTerrainSelectionHack\0";

struct CachedSites {
    len: u64,
    modified: Option<SystemTime>,
    sites: Vec<(u64, u32)>,
    image_uuid: [u8; 16],
}
static SITES: OnceLock<Mutex<HashMap<PathBuf, CachedSites>>> = OnceLock::new();

fn cstring_addresses(image: &MachImage<'_>, needle: &[u8]) -> Result<Vec<u64>> {
    let addresses = memchr::memmem::find_iter(image.bytes, needle)
        .filter_map(|position| image.address_for_offset(position + 1))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !addresses.is_empty(),
        "Studio string {} was not found",
        String::from_utf8_lossy(&needle[1..needle.len() - 1])
    );
    Ok(addresses)
}

fn cstring_address(image: &MachImage<'_>, needle: &[u8]) -> Result<u64> {
    let addresses = cstring_addresses(image, needle)?;
    anyhow::ensure!(
        addresses.len() == 1,
        "Studio string {} resolved {} candidates",
        String::from_utf8_lossy(&needle[1..needle.len() - 1]),
        addresses.len()
    );
    Ok(addresses[0])
}

fn unique_xref(image: &MachImage<'_>, address: u64) -> Result<u64> {
    let sites = arm64_address_xrefs(image, address)?;
    anyhow::ensure!(
        sites.len() == 1,
        "Studio reflection registration resolved {} sites",
        sites.len()
    );
    Ok(sites[0])
}

fn code_target(image: &MachImage<'_>, text: &[u8], offset: usize) -> Option<u64> {
    let (_, target) = arm64_adrp_add_target(text, image.text.address, offset)?;
    image.text_offset_for_address(target).map(|_| target)
}

fn preceding_code_target(image: &MachImage<'_>, text: &[u8], site: u64) -> Option<u64> {
    let offset = image.text_offset_for_address(site)? - image.text.offset;
    (1..=0x10)
        .filter_map(|steps| offset.checked_sub(steps * 4))
        .find_map(|candidate| code_target(image, text, candidate))
}

fn function_range(image: &MachImage<'_>, address: u64) -> Result<(u64, u64)> {
    let index = image
        .function_starts
        .partition_point(|start| *start <= address)
        .checked_sub(1)
        .context("Studio function precedes every function start")?;
    let start = image.function_starts[index];
    let end = image
        .function_starts
        .get(index + 1)
        .copied()
        .unwrap_or(image.text.address + image.text.size);
    Ok((start, end))
}

fn branch_links(image: &MachImage<'_>, text: &[u8], range: (u64, u64)) -> Vec<(u64, u64)> {
    let mut links = Vec::new();
    let Some(start) = image.text_offset_for_address(range.0) else {
        return links;
    };
    let start = start - image.text.offset;
    let words = ((range.1 - range.0) / 4) as usize;
    for index in 0..words {
        let offset = start + index * 4;
        let Some(instruction) = read_u32(text, offset) else {
            break;
        };
        if instruction & 0xfc00_0000 != 0x9400_0000 {
            continue;
        }
        let mut immediate = (instruction & 0x03ff_ffff) as i64;
        if immediate & 0x0200_0000 != 0 {
            immediate -= 0x0400_0000;
        }
        let site = range.0 + (index as u64) * 4;
        let target = site.wrapping_add_signed(immediate * 4);
        if image.text_offset_for_address(target).is_some() {
            links.push((site, target));
        }
    }
    links
}

fn undo_selection_sites(image: &MachImage<'_>) -> Result<Vec<(u64, u32)>> {
    if image.cpu != CPU_TYPE_ARM64 {
        bail!("The undo selection patch requires Apple Silicon Roblox Studio");
    }
    let text = image.text_bytes()?;
    let reset_site = unique_xref(image, cstring_address(image, RESET_WAYPOINTS_NAME)?)?;
    let mut undo_sites = Vec::new();
    for address in cstring_addresses(image, UNDO_NAME)? {
        undo_sites.extend(
            arm64_address_xrefs(image, address)?
                .into_iter()
                .filter(|site| site.abs_diff(reset_site) < 0x800),
        );
    }
    anyhow::ensure!(
        undo_sites.len() == 1,
        "Studio ChangeHistoryService.Undo registration resolved {} sites",
        undo_sites.len()
    );
    let undo = preceding_code_target(image, text, undo_sites[0])
        .context("Studio ChangeHistoryService.Undo registration has no bound function")?;
    let hack_site = unique_xref(image, cstring_address(image, TERRAIN_SELECTION_NAME)?)?;
    let hack_offset = image
        .text_offset_for_address(hack_site)
        .context("Studio Selection registration is outside __text")?
        - image.text.offset;
    let mut selection_functions = Vec::new();
    for offset in (hack_offset.saturating_sub(0x600)..hack_offset + 0x300).step_by(4) {
        if let Some(target) = code_target(image, text, offset) {
            selection_functions.push(target);
        }
    }
    selection_functions.sort_unstable();
    selection_functions.dedup();
    let mut visited = Vec::new();
    let mut pending = vec![(undo, 0usize)];
    let mut restore = Vec::new();
    while let Some((function, depth)) = pending.pop() {
        if visited.contains(&function) {
            continue;
        }
        visited.push(function);
        let range = function_range(image, function)?;
        let mut sites = Vec::new();
        for (site, target) in branch_links(image, text, range) {
            if selection_functions.binary_search(&target).is_ok() {
                sites.push((site, target));
            } else if depth < 2 {
                pending.push((target, depth + 1));
            }
        }
        if !sites.is_empty() {
            restore.push(sites);
        }
    }
    anyhow::ensure!(
        restore.len() == 1 && restore[0].len() == 2 && restore[0][0].1 == restore[0][1].1,
        "Studio undo restore selection resolved {} functions",
        restore.len()
    );
    restore
        .remove(0)
        .into_iter()
        .map(|(site, _)| {
            let offset = image
                .text_offset_for_address(site)
                .context("Studio undo restore site is outside __text")?;
            let original =
                read_u32(image.bytes, offset).context("Studio undo restore site is truncated")?;
            Ok((
                site.checked_sub(image.image_base)
                    .context("Studio undo restore site precedes __TEXT")?,
                original,
            ))
        })
        .collect()
}

type SelectionSites = (Vec<(u64, u32)>, [u8; 16]);

fn cached_sites(path: &Path) -> Result<SelectionSites> {
    let metadata =
        fs::metadata(path).with_context(|| format!("Could not inspect {}", path.display()))?;
    let modified = metadata.modified().ok();
    let cache = SITES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock_recover()
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
    {
        return Ok((cached.sites.clone(), cached.image_uuid));
    }
    let bytes = fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;
    let image = MachImage::parse(&bytes)?;
    let sites = undo_selection_sites(&image)?;
    cache.lock_recover().insert(
        path.to_path_buf(),
        CachedSites {
            len: metadata.len(),
            modified,
            sites: sites.clone(),
            image_uuid: image.image_uuid,
        },
    );
    Ok((sites, image.image_uuid))
}

// A process patch like the package notice flag: it survives reconnects and
// daemon restarts until Studio exits, and is idempotent.
pub(crate) fn keep_selection_across_undo(pid: u32) -> Result<()> {
    let (sites, image_uuid) = cached_sites(&process_executable_path(pid)?)?;
    let socket_path = PathBuf::from(format!("/tmp/renium-studio-{pid}.sock"));
    // The helper answers one request per connection.
    for (rva, original) in sites {
        let mut socket = UnixStream::connect(&socket_path).with_context(|| {
            format!(
                "Studio process {pid} was not launched with Renium's native helper; restart Roblox Studio"
            )
        })?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut request = Vec::with_capacity(48);
        request.extend_from_slice(&REQUEST_MAGIC.to_le_bytes());
        request.extend_from_slice(&REQUEST_VERSION.to_le_bytes());
        request.extend_from_slice(&KEEP_SELECTION_REQUEST.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes());
        request.extend_from_slice(&u64::from(original).to_le_bytes());
        request.extend_from_slice(&rva.to_le_bytes());
        request.extend_from_slice(&image_uuid);
        socket
            .write_all(&request)
            .context("Could not send the undo selection patch to Studio")?;
        let mut response = [0u8; RESPONSE_SIZE];
        socket
            .read_exact(&mut response)
            .context("Studio native helper closed without an undo selection response")?;
        if read_u32(&response, 0) != Some(REQUEST_MAGIC) {
            bail!("Studio native helper returned an invalid undo selection response");
        }
        if read_u32(&response, 4).unwrap_or(u32::MAX) != 0 {
            let text_bytes = &response[24..];
            let end = text_bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(text_bytes.len());
            bail!(
                "Studio undo selection patch failed: {}",
                String::from_utf8_lossy(&text_bytes[..end])
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_studio_resolves_two_restore_selection_branches() {
        let path = Path::new("/Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio");
        let Ok(bytes) = fs::read(path) else {
            return;
        };
        let image = MachImage::parse(&bytes).unwrap();
        let sites = undo_selection_sites(&image).unwrap();
        assert_eq!(sites.len(), 2);
        for (rva, original) in sites {
            assert_eq!(original & 0xfc00_0000, 0x9400_0000, "{rva:#x}");
        }
    }
}
