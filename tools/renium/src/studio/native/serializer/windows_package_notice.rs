//! Disable Studio's package-modification popup for the connected process lifetime.
//! The engine still marks packages Changed; links and package contents are untouched.
use super::*;
use crate::system::LockRecover;

const FLAG: &[u8] = b"RemovePackageModificationPopupDialog\0";

#[derive(Clone)]
struct Layout {
    flag: usize,
    registration: usize,
    registration_bytes: Vec<u8>,
    image_stamp: [u32; 3],
}

struct CachedNoticeLayout {
    len: u64,
    modified: Option<SystemTime>,
    layout: Layout,
}
static LAYOUTS: OnceLock<Mutex<HashMap<PathBuf, CachedNoticeLayout>>> = OnceLock::new();

fn discover(image: &PeImage<'_>) -> Result<Layout> {
    let names = memmem::find_iter(image.bytes, FLAG).collect::<Vec<_>>();
    anyhow::ensure!(
        names.len() == 1,
        "Studio package popup flag name is not unique"
    );
    let name = image.offset_to_rva(names[0])?;
    let text = image.section(b".text")?;
    let code = slice(image.bytes, text.raw_offset, text.raw_size)?;
    let mut matches = Vec::new();
    // Native FFlag registration: mov r8d,1; lea rdx,storage; lea rcx,name; jmp registrar.
    // Resolve addresses from the named registration, never a version-specific offset.
    for index in memmem::find_iter(code, b"\x48\x8d\x0d") {
        if index < 13 || index + 12 > code.len() {
            continue;
        }
        let offset = text.raw_offset + index;
        if image.rip_target(offset, 7)? != name
            || code[index - 13..index - 7] != [0x41, 0xb8, 1, 0, 0, 0]
            || code[index - 7..index - 4] != [0x48, 0x8d, 0x15]
            || code[index + 7] != 0xe9
        {
            continue;
        }
        let flag = image.rip_target(offset - 7, 7)?;
        anyhow::ensure!(
            image.sections.iter().any(|section| {
                section.characteristics & 0x8000_0000 != 0
                    && flag
                        .checked_sub(section.virtual_address)
                        .is_some_and(|relative| relative < section.virtual_size)
            }),
            "Studio package popup flag is not writable data"
        );
        let registrar = image.offset_to_rva(offset + 12)? as i64
            + i64::from(read_i32(image.bytes, offset + 8)?);
        image.require_executable_rva(usize::try_from(registrar)?)?;
        anyhow::ensure!(
            flag_is_consumed(image, code, text.raw_offset, flag),
            "Studio package popup flag has no consumer"
        );
        matches.push(Layout {
            flag,
            registration: image.offset_to_rva(offset - 13)?,
            registration_bytes: image.bytes[offset - 13..offset + 12].to_vec(),
            image_stamp: image.image_stamp,
        });
    }
    anyhow::ensure!(
        matches.len() == 1,
        "Studio package popup flag registration is unsupported"
    );
    Ok(matches.remove(0))
}

/// Independently confirm that the named storage controls a byte comparison
/// followed by a conditional branch in executable code. Studio compiles that
/// check as `cmp [rip+flag], r8b..r15b`, `cmp [rip+flag], al..bh` or
/// `cmp byte [rip+flag], imm8`, with a short or near jcc; 0.741 uses all three.
fn flag_is_consumed(image: &PeImage<'_>, code: &[u8], code_offset: usize, flag: usize) -> bool {
    let jcc_follows = |index: usize| {
        matches!(code.get(index), Some(0x74 | 0x75))
            || matches!(code.get(index..index + 2), Some([0x0f, 0x84 | 0x85]))
    };
    (0..code.len()).any(|index| {
        // The displacement is relative to the end of the whole instruction,
        // including the immediate of the third form.
        let (displacement, length) = match code.get(index..index + 3) {
            Some([0x44, 0x38, modrm]) if modrm & 0xc7 == 0x05 => (3, 7),
            Some([0x38, modrm, _]) if modrm & 0xc7 == 0x05 => (2, 6),
            Some([0x80, 0x3d, _]) => (2, 7),
            _ => return false,
        };
        let offset = code_offset + index;
        let target = read_i32(image.bytes, offset + displacement)
            .ok()
            .zip(image.offset_to_rva(offset + length).ok())
            .map(|(displacement, next)| i64::from(displacement) + next as i64);
        target == Some(flag as i64) && jcc_follows(index + length)
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
        CachedNoticeLayout {
            len: metadata.len(),
            modified,
            layout: layout.clone(),
        },
    );
    Ok(layout)
}

// This is a process patch, not a transaction guard. Studio can enqueue package
// notifications during load, raw Luau, commit, or delayed rollback. Restoring the
// flag between requests re-enables those notices after the command has returned.
// It deliberately survives disconnects/daemon restarts until Studio exits.
pub(crate) fn suppress_package_notices(pid: u32) -> Result<()> {
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Studio module was not found for package popup suppression")?;
    let layout = layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    anyhow::ensure!(
        memory.read_vec(
            studio.base + layout.registration,
            layout.registration_bytes.len()
        )? == layout.registration_bytes,
        "Studio package popup registration changed in memory"
    );
    let address = studio.base + layout.flag;
    let current = memory.read_vec(address, 1)?[0];
    anyhow::ensure!(current <= 1, "Studio package popup flag is not boolean");
    if current == 0 {
        memory.write(address, &[1])?;
    }
    anyhow::ensure!(
        memory.read_vec(address, 1)? == [1],
        "Studio package popup suppression was not retained"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(storage: usize) -> Vec<u8> {
        let mut bytes = vec![0xcc; 1024];
        bytes[512..512 + FLAG.len()].copy_from_slice(FLAG);
        bytes[32..57].copy_from_slice(&[
            0x41, 0xb8, 1, 0, 0, 0, 0x48, 0x8d, 0x15, 0, 0, 0, 0, 0x48, 0x8d, 0x0d, 0, 0, 0, 0,
            0xe9, 0, 0, 0, 0,
        ]);
        bytes[41..45].copy_from_slice(&((storage as i32 - 45).to_le_bytes()));
        bytes[48..52].copy_from_slice(&(512i32 - 52).to_le_bytes());
        bytes[53..57].copy_from_slice(&(200i32 - 57).to_le_bytes());
        bytes[96..105].copy_from_slice(&[0x44, 0x38, 0x3d, 0, 0, 0, 0, 0x0f, 0x85]);
        bytes[99..103].copy_from_slice(&((storage as i32 - 103).to_le_bytes()));
        bytes
    }

    fn image(bytes: &[u8]) -> PeImage<'_> {
        PeImage {
            bytes,
            image_base: 0,
            image_stamp: [1, 2, 3],
            sections: vec![
                PeSection {
                    name: *b".text\0\0\0",
                    virtual_size: 512,
                    virtual_address: 0,
                    raw_size: 512,
                    raw_offset: 0,
                    characteristics: 0x2000_0000,
                },
                PeSection {
                    name: *b".data\0\0\0",
                    virtual_size: 512,
                    virtual_address: 512,
                    raw_size: 512,
                    raw_offset: 512,
                    characteristics: 0x8000_0000,
                },
            ],
        }
    }

    #[test]
    fn package_popup_flag_tracks_named_storage_and_rejects_unsafe_layouts() -> Result<()> {
        for storage in [768, 824] {
            let bytes = fixture(storage);
            let layout = discover(&image(&bytes))?;
            assert_eq!(layout.flag, storage);
            assert_eq!(layout.registration, 32);
        }
        let mut bytes = fixture(768);
        bytes[96..98].copy_from_slice(&[0x90, 0x90]);
        assert!(discover(&image(&bytes)).is_err(), "missing consumer");
        for consumer in [
            &[0x44, 0x38, 0x35, 0, 0, 0, 0, 0x0f, 0x85][..],
            &[0x80, 0x3d, 0, 0, 0, 0, 0, 0x75, 0x0c],
            &[0x80, 0x3d, 0, 0, 0, 0, 0, 0x0f, 0x84],
            &[0x38, 0x05, 0, 0, 0, 0, 0x74, 0x0c, 0x90],
        ] {
            let mut bytes = fixture(768);
            bytes[96..105].copy_from_slice(consumer);
            let rip_end = if consumer[0] == 0x38 { 102 } else { 103 };
            let displacement = if consumer[0] == 0x44 { 99 } else { 98 };
            bytes[displacement..displacement + 4]
                .copy_from_slice(&(768i32 - rip_end).to_le_bytes());
            assert_eq!(
                discover(&image(&bytes))?.flag,
                768,
                "consumer {consumer:x?}"
            );
        }
        let mut bytes = fixture(768);
        bytes[128..137].copy_from_slice(&[0x44, 0x38, 0x3d, 0, 0, 0, 0, 0x0f, 0x85]);
        bytes[131..135].copy_from_slice(&(768i32 - 135).to_le_bytes());
        assert_eq!(discover(&image(&bytes))?.flag, 768, "two consumers");
        let mut bytes = fixture(768);
        bytes[103] = 0x90;
        bytes[104] = 0x90;
        assert!(
            discover(&image(&bytes)).is_err(),
            "comparison without a branch"
        );
        let mut bytes = fixture(768);
        bytes[600..600 + FLAG.len()].copy_from_slice(FLAG);
        assert!(discover(&image(&bytes)).is_err(), "ambiguous name");
        let bytes = fixture(300);
        assert!(discover(&image(&bytes)).is_err(), "executable storage");
        let mut bytes = fixture(768);
        bytes[34] = 0;
        assert!(discover(&image(&bytes)).is_err(), "wrong registration ABI");
        Ok(())
    }

    /// `RENIUM_STUDIO_EXES=path;path cargo test -- --ignored` checks installed builds.
    #[test]
    #[ignore]
    fn installed_studio_builds_expose_the_package_popup_flag() -> Result<()> {
        for path in std::env::var("RENIUM_STUDIO_EXES")?.split(';') {
            let bytes = fs::read(path)?;
            let layout = discover(&PeImage::parse(&bytes)?)?;
            println!(
                "{path}: flag {:#x} registration {:#x}",
                layout.flag, layout.registration
            );
        }
        Ok(())
    }
}
