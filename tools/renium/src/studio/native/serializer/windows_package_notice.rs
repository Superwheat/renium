//! Find unique storage by engine names; only GetFastFlag can confirm a write.
//! Discovery runs in the background. Transient failures retry; the watcher stays available.
use super::*;
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};
use std::sync::Arc;
use windows_sys::Win32::System::Memory::{
    MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY,
    PAGE_GUARD, PAGE_NOCACHE, PAGE_READONLY, PAGE_WRITECOMBINE, PAGE_WRITECOPY, VirtualQueryEx,
};

const NAME: &[u8] = b"RemovePackageModificationPopupDialog\0";
const TYPE: &[u8] = b".?AV?$ValueGetSet@VValue@FFlag@@@FLog@@\0";
// Resource bounds, not Studio layouts: an unrecognised/larger record is skipped.
const RECORD_WINDOW: usize = 512;
const SCAN_BYTES: usize = 64 * 1024 * 1024;
const SCAN_REGIONS: usize = 16_384;
const MAX_WITNESSES: usize = 262_144; // Memory budget, never a coverage limit.

struct Anchors {
    table: usize,
    pointer: isize,
    getter: (usize, Vec<u8>),
    name: usize,
}

// Decode only a leaf accessor returning a pointer loaded from its receiver.
// No registration/constructor/callee analysis, and no selected vtable slot.
fn getter(image: &PeImage<'_>, function: usize) -> Option<(isize, Vec<u8>)> {
    let section = image.sections.iter().find(|section| {
        function
            .checked_sub(section.virtual_address)
            .is_some_and(|offset| offset < section.raw_size)
    })?;
    let length = (section.raw_size - (function - section.virtual_address)).min(512);
    let code = slice(image.bytes, image.rva_to_offset(function).ok()?, length).ok()?;
    let mut values = HashMap::from([(Register::RCX, (false, 0isize))]);
    let mut decoder = Decoder::with_ip(64, code, function as u64, DecoderOptions::NONE);
    while decoder.can_decode() {
        let i = decoder.decode();
        match i.mnemonic() {
            Mnemonic::Nop => (),
            Mnemonic::Ret if values.get(&Register::RAX).is_some_and(|v| v.0) => {
                return Some((
                    values[&Register::RAX].1,
                    code[..decoder.position()].to_vec(),
                ));
            }
            Mnemonic::Mov | Mnemonic::Lea
                if i.op0_kind() == OpKind::Register && i.op0_register().size() == 8 =>
            {
                let value = match i.op1_kind() {
                    OpKind::Register
                        if i.mnemonic() == Mnemonic::Mov && i.op1_register().size() == 8 =>
                    {
                        *values.get(&i.op1_register())?
                    }
                    OpKind::Memory
                        if i.memory_index() == Register::None
                            && !matches!(i.memory_segment(), Register::FS | Register::GS) =>
                    {
                        let (loaded, offset) = *values.get(&i.memory_base())?;
                        if loaded || i.mnemonic() == Mnemonic::Mov && i.memory_size().size() != 8 {
                            return None;
                        }
                        (
                            i.mnemonic() == Mnemonic::Mov,
                            offset.checked_add(i.memory_displacement64() as isize)?,
                        )
                    }
                    _ => return None,
                };
                values.insert(i.op0_register(), value);
            }
            _ => return None,
        }
    }
    None
}

fn anchors(image: &PeImage<'_>) -> Result<Anchors> {
    (|| {
        let table = undo_selection::descriptor_vtable(image, TYPE)?;
        let offset = image.rva_to_offset(table)?;
        let mut getters = Vec::new();
        let mut ended = false;
        for slot in (0..4096).step_by(size_of::<usize>()) {
            let Some(function) =
                (read_u64(image.bytes, offset + slot)? as usize).checked_sub(image.image_base)
            else {
                ended = true;
                break;
            };
            if image.require_executable_rva(function).is_err() {
                ended = true;
                break;
            }
            if let Some((field, code)) = getter(image, function) {
                getters.push((field, function, code));
            }
        }
        anyhow::ensure!(
            ended,
            "Flag getter table scan limit reached; use the dialog watcher"
        );
        getters.sort_by_key(|row| row.0);
        getters.dedup_by_key(|row| row.0);
        anyhow::ensure!(
            getters.len() == 1,
            "Named flag type has no unique pointer getter; use the dialog watcher"
        );
        let (pointer, function, code) = getters.remove(0);
        let names = memmem::find_iter(image.bytes, NAME)
            .take(2)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            names.len() == 1,
            "Package flag name is missing or ambiguous; use the dialog watcher"
        );
        Ok(Anchors {
            table,
            pointer,
            getter: (function, code),
            name: image.offset_to_rva(names[0])?,
        })
    })()
    .context(Disposition::Terminal)
}

#[derive(Default)]
struct Matches {
    value: Option<usize>,
    witnesses: HashMap<usize, Option<usize>>,
}
impl Matches {
    fn observe(&mut self, name: usize, value: usize, target: bool) -> Result<()> {
        self.witnesses
            .entry(value)
            .and_modify(|n| {
                if *n != Some(name) {
                    *n = None;
                }
            })
            .or_insert(Some(name));
        anyhow::ensure!(self.witnesses.len() <= MAX_WITNESSES, Disposition::Terminal);
        if target {
            anyhow::ensure!(
                self.value.is_none_or(|old| old == value),
                Disposition::Terminal
            );
            self.value = Some(value);
        }
        Ok(())
    }
    fn finish(self) -> Result<usize> {
        let value = self.value.context(Disposition::Terminal)?;
        anyhow::ensure!(
            self.witnesses[&value].is_some()
                && self
                    .witnesses
                    .values()
                    .any(|n| n.is_some() && *n != self.witnesses[&value]),
            Disposition::Terminal
        );
        Ok(value)
    }
}

struct Scanner<'a> {
    memory: &'a ProcessMemory,
    studio: &'a ModuleEntry,
    image: &'a PeImage<'a>,
    anchors: &'a Anchors,
    writable: &'a [std::ops::Range<usize>],
    needle: [u8; 8],
    deadline: Instant,
    budget: &'a mut usize,
    found: &'a mut Matches,
}
impl Scanner<'_> {
    fn read(&self, address: usize, length: usize) -> Result<Option<Vec<u8>>> {
        match self.memory.read_vec(address, length) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(_) if region(self.memory, address)?.is_some_and(|r| r.State != MEM_COMMIT) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    fn range(&mut self, range: std::ops::Range<usize>) -> Result<usize> {
        let mut at = range.start;
        let mut bytes = Vec::new();
        while at < range.end {
            if Instant::now() >= self.deadline || *self.budget == 0 {
                // Revisit the overlap next slice, including a record split there.
                return Ok(at.saturating_sub(RECORD_WINDOW).max(range.start));
            }
            let length = (range.end - at).min(1024 * 1024).min(*self.budget);
            *self.budget -= length;
            // Retained bytes precede at; each read and match stays inside range.
            let base = at - bytes.len();
            let Some(part) = self.read(at, length)? else {
                return Ok(at);
            };
            bytes.extend_from_slice(&part);
            for object in memmem::find_iter(&bytes, &self.needle) {
                if Instant::now() >= self.deadline {
                    return Ok(base + object);
                }
                let Some(field) = (base + object).checked_add_signed(self.anchors.pointer) else {
                    continue;
                };
                let Some(value) = self.read(field, size_of::<usize>())? else {
                    continue;
                };
                let value = read_u64(&value, 0)? as usize;
                if !self.writable.iter().any(|r| r.contains(&value)) {
                    continue;
                }
                let mut end = object.saturating_add(RECORD_WINDOW).min(bytes.len());
                if let Some(next) =
                    memmem::find(&bytes[object + self.needle.len()..end], &self.needle)
                {
                    end = object + self.needle.len() + next;
                }
                // A pointer/length record names the nearby typed object. This
                // only narrows candidates: the engine confirms every toggle.
                for pair in bytes[object..end].windows(16) {
                    let pointer = read_u64(pair, 0)? as usize;
                    let length = read_u64(pair, 8)? as usize;
                    if length == 0 || length > RECORD_WINDOW {
                        continue;
                    }
                    let Some(rva) = pointer.checked_sub(self.studio.base) else {
                        continue;
                    };
                    let Ok(offset) = self.image.rva_to_offset(rva) else {
                        continue;
                    };
                    let Ok(name) = slice(self.image.bytes, offset, length + 1) else {
                        continue;
                    };
                    if name[length] != 0
                        || name[..length].contains(&0)
                        || std::str::from_utf8(name).is_err()
                    {
                        continue;
                    }
                    self.found
                        .observe(rva, value, rva == self.anchors.name && name == NAME)?;
                }
            }
            let keep = bytes.len().min(RECORD_WINDOW);
            bytes.drain(..bytes.len() - keep);
            at += length;
        }
        Ok(at)
    }
}

fn region(memory: &ProcessMemory, address: usize) -> Result<Option<MEMORY_BASIC_INFORMATION>> {
    let mut region = unsafe { zeroed() };
    let count = unsafe {
        VirtualQueryEx(
            memory.handle,
            address as *const c_void,
            &mut region,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if count == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error()
            == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32)
        {
            return Ok(None);
        }
        return Err(error.into());
    }
    anyhow::ensure!(
        count == size_of::<MEMORY_BASIC_INFORMATION>(),
        "Incomplete flag region query"
    );
    Ok(Some(region))
}

fn scan_slice(scan: &mut Scanner<'_>, private: bool, cursor: &mut usize) -> Result<bool> {
    // Visit every readable main-image region before the private-region pass.
    // The region limit is per slice, like bytes/time; it never truncates coverage.
    for _ in 0..SCAN_REGIONS {
        if *scan.budget == 0 || Instant::now() >= scan.deadline {
            return Ok(false);
        }
        let Some(region) = region(scan.memory, *cursor)? else {
            return Ok(true);
        };
        let low = region.BaseAddress as usize;
        let end = low
            .checked_add(region.RegionSize)
            .context("Flag region overflowed")?;
        anyhow::ensure!(
            low <= *cursor && *cursor < end,
            "Invalid flag region bounds"
        );
        *cursor = if region.State == MEM_COMMIT
            && if private {
                region.Type == windows_sys::Win32::System::Memory::MEM_PRIVATE
            } else {
                region.Type == windows_sys::Win32::System::Memory::MEM_IMAGE
                    && region.AllocationBase as usize == scan.studio.base
            }
            && region.Protect & (PAGE_GUARD | PAGE_NOCACHE | PAGE_WRITECOMBINE) == 0
            && matches!(
                region.Protect & 0xff,
                PAGE_READONLY
                    | PAGE_READWRITE
                    | PAGE_WRITECOPY
                    | PAGE_EXECUTE_READ
                    | PAGE_EXECUTE_READWRITE
                    | PAGE_EXECUTE_WRITECOPY
            ) {
            scan.range(*cursor..end)?
        } else {
            end
        };
        if *cursor < end {
            return Ok(false);
        }
    }
    Ok(false)
}

fn candidates(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    connected: &dyn Fn() -> bool,
    confirm: impl FnOnce(usize) -> Result<bool>,
) -> Result<serde_json::Value> {
    let mapped = ImageView::open(&studio.path)?;
    anyhow::ensure!(
        mapped.bytes().len() <= 512 * 1024 * 1024,
        Disposition::Terminal
    );
    let image = PeImage::parse(mapped.bytes())?;
    let anchors = anchors(&image)?;
    verify_loaded_image(memory, studio, image.image_stamp)?;
    let address = |rva| {
        studio
            .base
            .checked_add(rva)
            .context("Flag image address overflowed")
    };
    anyhow::ensure!(
        memory.read_vec(address(anchors.name)?, NAME.len())? == NAME
            && memory.read_vec(address(anchors.getter.0)?, anchors.getter.1.len())?
                == anchors.getter.1,
        Disposition::Terminal
    );
    let writable = image
        .sections
        .iter()
        .filter(|s| s.characteristics & 0x8000_0000 != 0 && s.characteristics & 0x2000_0000 == 0)
        .map(|s| {
            Ok(address(s.virtual_address)?
                ..address(
                    s.virtual_address
                        .checked_add(s.virtual_size.max(s.raw_size))
                        .context("Flag section overflowed")?,
                )?)
        })
        .collect::<Result<Vec<_>>>()?;
    let needle = address(anchors.table)?.to_le_bytes();
    let mut found = Matches::default();
    let (mut slices, mut bytes) = (0, 0);
    let started = Instant::now();
    for private in [false, true] {
        let mut cursor = 0;
        loop {
            anyhow::ensure!(connected(), "Package flag connection ended");
            let mut budget = SCAN_BYTES;
            let complete = scan_slice(
                &mut Scanner {
                    memory,
                    studio,
                    image: &image,
                    anchors: &anchors,
                    writable: &writable,
                    needle,
                    deadline: Instant::now() + Duration::from_millis(100),
                    budget: &mut budget,
                    found: &mut found,
                },
                private,
                &mut cursor,
            )?;
            slices += 1;
            bytes += SCAN_BYTES - budget;
            if complete {
                break;
            }
            std::thread::yield_now();
        }
    }
    let search_seconds = started.elapsed().as_secs_f64();
    anyhow::ensure!(confirm(found.finish()?)?, Disposition::Terminal);
    Ok(
        serde_json::json!({"slices":slices,"scannedBytes":bytes,"searchSeconds":search_seconds,"complete":true}),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Disposition {
    Terminal,
    Transient,
    Readback,
}
impl std::fmt::Display for Disposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Package flag {self:?}")
    }
}
fn classify(error: &anyhow::Error) -> Disposition {
    use properties::NativeError;
    if error.downcast_ref::<Disposition>() == Some(&Disposition::Terminal)
        || matches!(
            error.downcast_ref::<NativeError>(),
            Some(NativeError::Unsupported(_))
        )
        || matches!(error.downcast_ref::<NativeError>(), Some(NativeError::Task(exit, status, _))
            if [*exit, *status].iter().any(|code| matches!(code, 0xE405 | 0xE40A | 0xE411)))
    {
        Disposition::Terminal
    } else {
        Disposition::Transient
    }
}

static SEARCHES: OnceLock<Searches> = OnceLock::new();
fn cache_path(pid: u32, identity: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "renium-native/package-flag-by-name-{pid}-{identity}.json"
    ))
}
// Only terminal answers survive a daemon reconnect.
fn remembered(
    path: &Path,
    work: impl FnOnce(bool) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    fs::create_dir_all(path.parent().context("Missing cache directory")?)?;
    let _lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path.with_extension("lock"))?;
    let saved = match fs::read(path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<std::result::Result<serde_json::Value, String>>(&bytes)
                .unwrap_or(Ok(serde_json::Value::Null)),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(Err(error)) = saved {
        return Err(anyhow::Error::msg(error)).context(Disposition::Terminal);
    }
    let answer = work(saved.is_some() || path.with_extension("submitted").try_exists()?);
    if answer.as_ref().err().map(classify) != Some(Disposition::Transient) {
        let stored = answer.as_ref().map_err(|e| format!("{e:#}"));
        atomic_write_file(path, &serde_json::to_vec(&stored)?)?;
        if let Err(error) = &answer {
            crate::app::output::log_global(
                2,
                format_args!("[renium] package flag unavailable; using dialog watcher: {error:#}"),
            );
        }
    }
    answer
}
fn reserve(path: &Path) -> Result<bool> {
    use std::io::Write;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.with_extension("submitted"))
    {
        Ok(mut marker) => {
            marker.write_all(b"write submitted\n")?;
            marker.sync_all()?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Default)]
struct Searches(Arc<Mutex<HashSet<(u32, String)>>>);
impl Searches {
    fn request(
        &self,
        key: (u32, String),
        connected: impl Fn() -> bool + Send + 'static,
        mut work: impl FnMut(&dyn Fn() -> bool) -> Result<serde_json::Value> + Send + 'static,
    ) -> Result<bool> {
        if !self.0.lock_recover().insert(key.clone()) {
            return Ok(false);
        }
        let searches = Arc::clone(&self.0);
        let cleanup = crate::system::files::OnDrop::new(move || {
            searches.lock_recover().remove(&key);
        });
        std::thread::Builder::new().name("package flag".into()).spawn(move || {
            let _cleanup = cleanup;
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
            if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) } == 0 { return; }
            let last = std::cell::Cell::new((Instant::now() - Duration::from_secs(1), false));
            let alive = || {
                if last.get().0.elapsed() >= Duration::from_secs(1) { last.set((Instant::now(), connected())); }
                last.get().1
            };
            for attempt in 0..8 {
                if !alive() { return; }
                let Err(error) = work(&alive) else { return; };
                if classify(&error) == Disposition::Terminal { return; }
                if attempt == 7 {
                    crate::app::output::log_global(3, format_args!("[renium] package flag not this session; reconnect to retry: {error:#}"));
                    return;
                }
                std::thread::sleep(Duration::from_millis(250 << attempt.min(5)));
            }
        })?;
        Ok(true)
    }
}

fn enable(
    pid: u32,
    path: &Path,
    read_only: bool,
    connected: &dyn Fn() -> bool,
) -> Result<serde_json::Value> {
    let mut getter = properties::prepare_package_flag(pid)?;
    let already_set = getter.package_flag(None)?;
    let mut search = serde_json::json!({});
    if !already_set {
        anyhow::ensure!(!read_only, Disposition::Readback);
        let current = modules(pid)?;
        let studio = current
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
            .context("Studio image is missing")?;
        let memory = ProcessMemory::open_read_only(pid)?;
        search = candidates(&memory, studio, connected, |found| {
            anyhow::ensure!(reserve(path)?, Disposition::Readback);
            getter.package_flag(Some(found))
        })?;
    }
    Ok(
        serde_json::json!({"value":true,"confirmedBy":"DataModel.GetFastFlag","alreadySet":already_set,"search":search}),
    )
}

// Connect/setup only starts work or reads the completed answer. It never waits.
pub(crate) fn suppress_package_notices(pid: u32) -> Result<()> {
    let identity = crate::app::update::process_start_identity(pid)
        .context("Cannot identify Studio process; use the dialog watcher")?;
    let path = cache_path(pid, &identity);
    SEARCHES.get_or_init(Default::default).request(
        (pid, identity.clone()),
        move || {
            crate::app::update::process_start_identity(pid).as_deref() == Some(&identity)
                && crate::studio::bridge::package_notice_connected(pid)
        },
        move |connected| remembered(&path, |read_only| enable(pid, &path, read_only, connected)),
    )?;
    Ok(())
}

// Probes report stored state only; the worker alone loads helpers or submits tasks.
pub(super) fn probe(memory: &ProcessMemory, _: &ModuleEntry) -> Result<serde_json::Value> {
    let identity =
        crate::app::update::process_start_identity(memory.pid).context("Studio ended")?;
    let path = cache_path(memory.pid, &identity);
    let answer: std::result::Result<serde_json::Value, String> =
        serde_json::from_reader(fs::File::open(path)?.take(65536))?;
    answer.map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_collects_late_candidates_and_rejects_shared_or_invalid_fields() -> Result<()> {
        // Real read-only process reads of owned fixture memory, across a chunk
        // boundary; no writer or helper is involved in candidate discovery.
        for case in 0..10 {
            let mut data = vec![0u8; 1024 * 1024 + 8192];
            let base = data.as_ptr() as usize;
            data[100..100 + NAME.len()].copy_from_slice(NAME);
            data[200..206].copy_from_slice(b"Other\0");
            for (i, object) in [
                4096,
                1024 * 1024 + 4096,
                if case == 8 { 1024 * 1024 + 6144 } else { 6144 },
            ]
            .into_iter()
            .enumerate()
            {
                let name = if i == 2 { 200 } else { 100 };
                let value = if case == 1 || case == 8 && i == 2 || case != 9 && i < 2 {
                    300
                } else {
                    300 + i
                };
                data[object..object + 8].copy_from_slice(&(base + 16).to_le_bytes());
                data[object + 8..object + 16].copy_from_slice(&(base + value).to_le_bytes());
                data[object + 16..object + 24].copy_from_slice(&(base + name).to_le_bytes());
                let length = if name == 100 { NAME.len() - 1 } else { 5 };
                data[object + 24..object + 32]
                    .copy_from_slice(&(length + usize::from(case == 2)).to_le_bytes());
            }
            if case == 3 {
                data[4096 + 8..4096 + 16].copy_from_slice(&base.to_le_bytes());
                data[1024 * 1024 + 4104..1024 * 1024 + 4112].copy_from_slice(&base.to_le_bytes());
            }
            if case == 4 {
                data[100] = b'X';
            }
            let image = PeImage {
                bytes: &data,
                image_base: base,
                image_stamp: [0; 3],
                sections: vec![PeSection {
                    name: [0; 8],
                    virtual_size: data.len(),
                    virtual_address: 0,
                    raw_size: data.len(),
                    raw_offset: 0,
                    characteristics: 0,
                }],
            };
            let memory = ProcessMemory::open_read_only(std::process::id())?;
            let studio = ModuleEntry {
                base,
                size: data.len(),
                name: "fixture".into(),
                path: PathBuf::new(),
            };
            let anchors = Anchors {
                table: 16,
                pointer: 8,
                getter: (0, vec![]),
                name: 100,
            };
            let mut budget = if case == 6 { 1 } else { SCAN_BYTES };
            let mut found = Matches::default();
            let writable = base + 300..base + if case == 5 { 300 } else { 303 };
            let mut scan = Scanner {
                memory: &memory,
                studio: &studio,
                image: &image,
                anchors: &anchors,
                writable: std::slice::from_ref(&writable),
                needle: (base + 16).to_le_bytes(),
                budget: &mut budget,
                found: &mut found,
                deadline: Instant::now()
                    + if case == 7 {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(5)
                    },
            };
            let result = if case == 0 {
                let mut next = base;
                while next < base + data.len() {
                    *scan.budget = 8192;
                    let before = next;
                    next = scan.range(next..base + data.len())?;
                    assert!(next > before, "slice did not advance");
                }
                found.finish()
            } else {
                scan.range(base..base + data.len()).and_then(|next| {
                    if matches!(case, 6 | 7) {
                        assert!(next < base + data.len());
                    }
                    found.finish()
                })
            };
            if case == 0 {
                assert_eq!(result?, base + 300);
            } else {
                assert!(result.is_err(), "case {case} was accepted");
            }
        }
        for observations in [
            [
                (1, 10, true),
                (3, 20, false),
                (2, 10, false),
                (4, 20, false),
            ],
            [
                (4, 20, false),
                (2, 10, false),
                (3, 20, false),
                (1, 10, true),
            ],
        ] {
            let mut grouped = Matches::default();
            for (name, value, target) in observations {
                grouped.observe(name, value, target)?;
            }
            assert!(grouped.finish().is_err());
        }
        let mut capped = Matches::default();
        for value in 0..MAX_WITNESSES {
            capped.observe(value, value, false)?;
        }
        assert!(capped.observe(MAX_WITNESSES, MAX_WITNESSES, false).is_err());
        let mut excess = Matches::default();
        excess.observe(1, 10, true)?;
        assert_eq!(
            classify(&excess.observe(1, 11, true).unwrap_err()),
            Disposition::Terminal
        );
        Ok(())
    }

    #[test]
    fn forged_terminal_answers_are_verified_without_writing() -> Result<()> {
        let directory = crate::tests::support::temp_dir("package-forged-answer");
        let path = directory.join("answer.json");
        fs::write(
            &path,
            br#"{"Ok":{"value":true,"values":[1],"distinct":true}}"#,
        )?;
        for engine in [false, true] {
            let result = remembered(&path, |read_only| {
                assert!(read_only, "disk answer authorised a write");
                engine
                    .then_some(serde_json::json!({"value":true}))
                    .context(Disposition::Readback)
            });
            assert_eq!(result.is_ok(), engine);
        }
        fs::remove_file(&path)?;
        fs::remove_file(path.with_extension("lock"))?;
        fs::remove_dir(directory)?;
        Ok(())
    }

    #[test]
    fn workers_are_unique_retry_transients_and_stop_when_disconnected() -> Result<()> {
        let searches = Searches::default();
        let (release, wait) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let mut attempts = 0;
        assert!(searches.request(
            (1, "one".into()),
            || true,
            move |_| {
                attempts += 1;
                if attempts < 3 {
                    return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
                }
                wait.recv()?;
                done.send(attempts)?;
                assert_eq!(
                    unsafe {
                        windows_sys::Win32::System::Threading::GetThreadPriority(
                            windows_sys::Win32::System::Threading::GetCurrentThread(),
                        )
                    },
                    windows_sys::Win32::System::Threading::THREAD_PRIORITY_BELOW_NORMAL
                );
                Matches::default().finish().map(serde_json::Value::from)
            }
        )?);
        assert!(!searches.request((1, "one".into()), || true, |_| panic!("duplicate worker"))?);
        assert!(searches.request(
            (2, "two".into()),
            || false,
            |_| panic!("disconnected worker ran")
        )?);
        release.send(())?;
        assert_eq!(finished.recv_timeout(Duration::from_secs(5))?, 3);
        let end = Instant::now() + Duration::from_secs(2);
        while !searches.0.lock_recover().is_empty() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(searches.request(
            (1, "one".into()),
            || false,
            |_| panic!("new disconnected session ran")
        )?);
        Ok(())
    }

    #[test]
    fn submitted_marker_is_durable_before_write_and_never_allows_another_writer() -> Result<()> {
        let directory = crate::tests::support::temp_dir("package-marker");
        let path = directory.join("answer.json");
        let writes = std::cell::Cell::new(0);
        remembered(&path, |read_only| {
            assert!(!read_only);
            assert!(reserve(&path)?);
            assert_eq!(
                fs::read(path.with_extension("submitted"))?,
                b"write submitted\n"
            );
            writes.set(writes.get() + 1);
            Err(anyhow::anyhow!("timed out"))
        })
        .unwrap_err();
        assert!(!path.exists());
        for value in [false, true] {
            let result = remembered(&path, |read_only| {
                assert!(read_only);
                assert!(!reserve(&path)?);
                value
                    .then_some(serde_json::json!({"value":true}))
                    .context(Disposition::Readback)
            });
            assert_eq!(result.is_ok(), value);
        }
        assert_eq!(writes.get(), 1);
        for file in [
            path.clone(),
            path.with_extension("submitted"),
            path.with_extension("lock"),
        ] {
            fs::remove_file(file)?;
        }
        fs::remove_dir(directory)?;
        Ok(())
    }

    #[test]
    fn failures_are_classified_by_cause() -> Result<()> {
        use Disposition::*;
        for (exit, status, expected) in [
            (0, 0, Transient),
            (0xE40F, 0, Transient),
            (0, 0xE40F, Transient),
            (0, 0xE404, Transient),
            (0, 0xE401, Transient),
            (0, 0xE402, Transient),
            (0, 0xE403, Transient),
            (0, 0xE405, Terminal),
            (0, 0xE406, Transient),
            (0, 0xE407, Transient),
            (0xE408, 0, Transient),
            (0xE409, 0, Transient),
            (0xE40A, 0xE410, Terminal),
            (0, 0xE410, Transient),
            (0, 0xE411, Terminal),
        ] {
            let mut parameters = vec![0; 65888];
            put_u32(&mut parameters, 84, status);
            parameters[65632..65638].copy_from_slice(b"cause\0");
            let error = properties::task_result(exit, &parameters).unwrap_err();
            let message =
                format!("Studio protected property call failed (0x{exit:X}/0x{status:X}): cause");
            assert_eq!(error.to_string(), message);
            assert_eq!(format!("{error:#}"), message);
            assert_eq!(
                crate::editor::native_roots::studio_skipped_native_task(&error),
                exit == 0xE40F || status == 0xE40F
            );
            assert_eq!(classify(&error), expected);
        }
        let directory = crate::tests::support::temp_dir("package-classification");
        let path = directory.join("answer.json");
        for _ in 0..2 {
            let error = remembered(&path, |_| {
                fs::read(directory.join("missing"))
                    .map(serde_json::Value::from)
                    .map_err(Into::into)
            })
            .unwrap_err();
            assert_eq!(classify(&error), Transient);
            assert!(!path.exists());
        }
        assert_eq!(classify(&read_u64(&[], 0).unwrap_err()), Transient);
        let image = PeImage {
            bytes: &[],
            image_base: 0,
            image_stamp: [0; 3],
            sections: vec![],
        };
        assert_eq!(classify(&anchors(&image).err().unwrap()), Terminal);
        let error = remembered(&path, |_| {
            Matches::default().finish().map(serde_json::Value::from)
        })
        .unwrap_err();
        assert_eq!(classify(&error), Terminal);
        assert_eq!(
            classify(&remembered(&path, |_| panic!("terminal answer retried")).unwrap_err()),
            Terminal
        );
        fs::remove_file(&path)?;
        fs::remove_file(path.with_extension("lock"))?;
        fs::remove_dir(directory)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires installed Studio images in RENIUM_STUDIO_EXES; live readback proves the value"]
    fn installed_studio_builds() -> Result<()> {
        let paths = std::env::var_os("RENIUM_STUDIO_EXES").context("Set RENIUM_STUDIO_EXES")?;
        for path in std::env::split_paths(&paths) {
            let mapped = ImageView::open(&path)?;
            let image = PeImage::parse(mapped.bytes())?;
            let found = anchors(&image)?;
            println!(
                "{}: named type and getter available; live GetFastFlag confirmation required (field {})",
                path.display(),
                found.pointer
            );
        }
        Ok(())
    }
}
