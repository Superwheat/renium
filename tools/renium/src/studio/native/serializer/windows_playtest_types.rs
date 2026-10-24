//! Process-validated reflection singletons, cached by executable identity.
use super::*;
use crate::system::LockRecover;
type Key = ([u32; 3], String, Option<String>);
static TYPES: OnceLock<Mutex<HashMap<Key, usize>>> = OnceLock::new();
fn descriptor_name(memory: &ProcessMemory, object: usize) -> Option<String> {
    read_msvc_string(memory, memory.read_u64(object + 8).ok()? as usize)
}

pub(super) fn member(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    kind: &str,
    name: &str,
    lua: bool,
) -> Result<(usize, usize)> {
    let descriptor = singleton(memory, studio, kind, Some(name))?;
    let layout = package_layout(&studio.path)?;
    let (field, function) = if lua {
        properties::lua_binding(memory, studio, &layout, descriptor)?
    } else {
        properties::member_binding(memory, studio, &layout, descriptor)?
    };
    Ok((descriptor + field, function))
}

pub(super) fn singleton(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    kind: &str,
    name: Option<&str>,
) -> Result<usize> {
    let key = (
        package_layout(&studio.path)?.image_stamp,
        kind.to_owned(),
        name.map(str::to_owned),
    );
    let cached = TYPES
        .get_or_init(Default::default)
        .lock_recover()
        .get(&key)
        .copied();
    if let Some(rva) = cached {
        let object = studio.base + rva;
        if properties::complete_reflection_type(memory, studio, object).as_deref() == Some(kind)
            && name
                .is_none_or(|expected| descriptor_name(memory, object).as_deref() == Some(expected))
        {
            return Ok(object);
        }
    }
    let mapped = observation::ImageView::open(&studio.path)?;
    let pe = PeImage::parse(mapped.bytes())?;
    // Some typed event signatures exceed the diagnostic anchor limit. Match
    // their bounded prefix here, then require the complete RTTI below.
    let anchors = image::anchors(&pe, &[kind.chars().take(256).collect()])?;
    let tables = image::rtti(&pe, &anchors)?
        .into_iter()
        .filter(|row| row["type"] == kind && row["offsetToTop"] == 0)
        .collect::<Vec<_>>();
    anyhow::ensure!(tables.len() == 1, "Native reflection type is ambiguous");
    let table = studio.base
        + tables[0]["vtableRva"]
            .as_u64()
            .context("Missing reflection vtable")? as usize;
    let mut found = Vec::new();
    let mut remaining = 256usize * 1024 * 1024;
    // Reflection's type singleton is a static complete object. Only examine
    // this image's writable sections and validate its complete RTTI identity.
    for section in pe
        .sections
        .iter()
        .filter(|section| section.characteristics & 0x8000_0000 != 0)
    {
        remaining = remaining
            .checked_sub(section.virtual_size)
            .context("Reflection type image exceeds its byte budget")?;
        let bytes = memory.read_vec(studio.base + section.virtual_address, section.virtual_size)?;
        for at in (0..bytes.len().saturating_sub(7)).step_by(8) {
            if read_u64(&bytes, at)? as usize == table {
                let object = studio.base + section.virtual_address + at;
                if properties::complete_reflection_type(memory, studio, object).as_deref()
                    == Some(kind)
                    && name.is_none_or(|expected| {
                        descriptor_name(memory, object).as_deref() == Some(expected)
                    })
                {
                    found.push(object);
                }
            }
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Native reflection type has {} singleton candidates",
        found.len()
    );
    let mut cache = TYPES.get().unwrap().lock_recover();
    if cache.len() >= 128 {
        cache.clear();
    }
    cache.insert(key, found[0] - studio.base);
    Ok(found[0])
}
