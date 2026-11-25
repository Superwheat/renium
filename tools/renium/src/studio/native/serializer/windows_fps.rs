//! Runtime structural discovery and in-process control for Studio focus-loss
//! render throttling.
//!
//! Zero addresses, offsets, vtable indices, or executable hashes are hardcoded:
//! every field, function, and vtable slot is derived from stable FLog anchors
//! in the Studio PE image and verified against live memory before any change is
//! made. Studio's own frame rate cap is left untouched.
use super::*;
use crate::studio::fps::Action;
use crate::system::LockRecover;
use iced_x86::{Decoder, DecoderOptions, Instruction, MemorySize, Mnemonic, OpKind, Register};
use std::collections::VecDeque;

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-fps.dll"));
const PARAM_SIZE: usize = 336;

const PUSH_THROTTLE_ANCHOR: &[u8] =
    b"[FLog::RenderSchedulerState] Pushing throttle render state (count {})\0";
const POP_THROTTLE_ANCHOR: &[u8] =
    b"[FLog::RenderSchedulerState] Popping throttle render state (count {})\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RenderThrottleLayout {
    pub(super) push_rva: usize,
    pub(super) pop_rva: usize,
    pub(super) vtable_slot_rva: usize,
    pub(super) should_skip_rva: usize,
    pub(super) frame_counter_offset: u32,
    pub(super) throttle_count_offset: u32,
    pub(super) reset_counter_value: i32,
    pub(super) image_stamp: [u32; 3],
}

struct CachedFpsLayout {
    len: u64,
    modified: Option<SystemTime>,
    layout: RenderThrottleLayout,
}

static FPS_LAYOUTS: OnceLock<Mutex<HashMap<PathBuf, CachedFpsLayout>>> = OnceLock::new();

pub(crate) struct FpsState {
    pub(crate) unlocked: bool,
    pub(crate) hook_installed: bool,
    pub(crate) last_throttle_count: i32,
    pub(crate) render_checks: u64,
    pub(crate) unthrottled_frames: u64,
}

pub(crate) struct FpsController {
    pid: u32,
    process: ProcessMemory,
    active_entry: usize,
    active_is_current: bool,
    owns_unlock: bool,
    pending_thread: Option<(HANDLE, usize)>,
    studio_base: usize,
    layout: RenderThrottleLayout,
}

pub(crate) fn helper_loaded(pid: u32) -> Result<bool> {
    Ok(modules(pid)?
        .iter()
        .any(|module| is_fps_helper_module(&module.name)))
}

fn is_studio_exe_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("RobloxStudioBeta.exe")
}

fn is_fps_helper_module(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("renium-fps-")
}

fn current_helper_name() -> String {
    format!("renium-fps-{:016x}.dll", fnv1a(HELPER))
}

fn helper_owning_slot(loaded: &[ModuleEntry], slot_value: usize) -> Option<&ModuleEntry> {
    loaded.iter().find(|module| {
        is_fps_helper_module(&module.name)
            && slot_value >= module.base
            && slot_value < module.base.saturating_add(module.size)
    })
}

fn remote_export_address(
    process: &ProcessMemory,
    module: &ModuleEntry,
    export_name: &str,
) -> Result<usize> {
    let pe_offset = process.read_u32(module.base + 0x3c)? as usize;
    anyhow::ensure!(
        pe_offset
            .checked_add(0x108)
            .is_some_and(|end| end <= module.size),
        "Helper module {} has an invalid PE header",
        module.name
    );
    let header = process.read_vec(module.base + pe_offset, 0x108)?;
    anyhow::ensure!(
        read_u32(&header, 0)? == 0x4550 && u16::from_le_bytes([header[24], header[25]]) == 0x20b,
        "Helper module {} is not a valid 64-bit PE image",
        module.name
    );
    let exp_rva = read_u32(&header, 24 + 0x70)? as usize;
    let exp_size = read_u32(&header, 24 + 0x74)? as usize;
    anyhow::ensure!(
        exp_rva != 0 && exp_size >= 40 && exp_rva.saturating_add(exp_size) <= module.size,
        "Helper module {} has no export directory",
        module.name
    );
    let dir = process.read_vec(module.base + exp_rva, 40)?;
    let num_names = read_u32(&dir, 24)? as usize;
    let funcs_rva = read_u32(&dir, 28)? as usize;
    let names_rva = read_u32(&dir, 32)? as usize;
    let ords_rva = read_u32(&dir, 36)? as usize;
    anyhow::ensure!(
        num_names <= 4096,
        "Helper module {} has an unexpected export count ({num_names})",
        module.name
    );
    for idx in 0..num_names {
        let name_rva = process.read_u32(module.base + names_rva + idx * 4)? as usize;
        if read_c_string(process, module.base + name_rva, 128).as_deref() == Some(export_name) {
            let mut ord_bytes = [0u8; 2];
            process.read(module.base + ords_rva + idx * 2, &mut ord_bytes)?;
            let ord = u16::from_le_bytes(ord_bytes) as usize;
            let fn_rva = process.read_u32(module.base + funcs_rva + ord * 4)? as usize;
            anyhow::ensure!(
                fn_rva < exp_rva || fn_rva >= exp_rva.saturating_add(exp_size),
                "Helper export {export_name} in {} is unexpectedly forwarded",
                module.name
            );
            return Ok(module.base + fn_rva);
        }
    }
    bail!(
        "Helper module {} does not export {export_name}",
        module.name
    )
}

fn load_current_helper(pid: u32, process: &ProcessMemory, loaded: &[ModuleEntry]) -> Result<usize> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{DONT_RESOLVE_DLL_REFERENCES, LoadLibraryExW};
    let directory = crate::app::update::user_data_dir()?.join("native");
    fs::create_dir_all(&directory)?;
    let path = directory.join(current_helper_name());
    if fs::read(&path).ok().as_deref() != Some(HELPER) {
        atomic_write_file(&path, HELPER)?;
    }
    let local = unsafe {
        LoadLibraryExW(
            wide(path.as_os_str()).as_ptr(),
            null_mut(),
            DONT_RESOLVE_DLL_REFERENCES,
        )
    };
    anyhow::ensure!(!local.is_null(), "Could not read the Studio FPS helper");
    let entry = unsafe { GetProcAddress(local, c"ReniumFpsStep".as_ptr().cast()) }
        .map(|address| address as usize - local as usize);
    unsafe {
        FreeLibrary(local);
    }
    let entry = entry.context("Studio FPS helper has no entry point")?;
    let base = ensure_library_loaded(pid, process, loaded, 5_000, &path)?;
    Ok(base + entry)
}

impl FpsController {
    pub(crate) fn open(pid: u32, allow_load_new: bool) -> Result<Self> {
        let loaded = modules(pid)?;
        let studio = loaded
            .iter()
            .find(|module| is_studio_exe_name(&module.name))
            .context("Studio executable module was not found for FPS control")?;
        let layout = cached_layout(&studio.path)?;
        let process = ProcessMemory::open_for_windows_fps(pid)?;
        verify_loaded_image(&process, studio, layout.image_stamp)?;

        let current_name = current_helper_name();
        let slot_value = process.read_u64(studio.base + layout.vtable_slot_rva)? as usize;
        let expected = studio.base + layout.should_skip_rva;
        let slot_owner = (slot_value != expected)
            .then(|| helper_owning_slot(&loaded, slot_value))
            .flatten();

        let (active_entry, active_is_current) = if let Some(owner) = slot_owner {
            let is_current = owner.name.eq_ignore_ascii_case(&current_name);
            let entry = remote_export_address(&process, owner, "ReniumFpsStep")?;
            (entry, is_current)
        } else if !allow_load_new {
            let existing = loaded
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(&current_name))
                .or_else(|| loaded.iter().find(|m| is_fps_helper_module(&m.name)))
                .context("No Studio FPS helper is loaded in this process")?;
            let is_current = existing.name.eq_ignore_ascii_case(&current_name);
            let entry = remote_export_address(&process, existing, "ReniumFpsStep")?;
            (entry, is_current)
        } else {
            let entry = load_current_helper(pid, &process, &loaded)?;
            (entry, true)
        };

        let mut controller = Self {
            pid,
            process,
            active_entry,
            active_is_current,
            owns_unlock: false,
            pending_thread: None,
            studio_base: studio.base,
            layout,
        };
        if allow_load_new && !controller.active_is_current {
            controller.upgrade_active_helper()?;
        }
        Ok(controller)
    }

    fn upgrade_active_helper(&mut self) -> Result<()> {
        if self.active_is_current {
            return Ok(());
        }
        let _ = self.run_step_on(self.active_entry, Action::Default)?;
        let slot_value =
            self.process
                .read_u64(self.studio_base + self.layout.vtable_slot_rva)? as usize;
        let expected = self.studio_base + self.layout.should_skip_rva;
        anyhow::ensure!(
            slot_value == expected,
            "Previous Studio FPS helper did not restore RenderSchedulerState vtable slot"
        );
        let loaded = modules(self.pid)?;
        let entry = load_current_helper(self.pid, &self.process, &loaded)?;
        self.active_entry = entry;
        self.active_is_current = true;
        Ok(())
    }

    fn reap_pending_thread(&mut self) -> Result<()> {
        let Some((thread, addr)) = self.pending_thread else {
            return Ok(());
        };
        let waited = unsafe { WaitForSingleObject(thread, 0) };
        if waited != WAIT_OBJECT_0 {
            bail!("Studio process is not responding (suspended or busy)");
        }
        self.pending_thread = None;
        unsafe {
            CloseHandle(thread);
            if addr != 0 {
                VirtualFreeEx(self.process.handle, addr as *mut c_void, 0, MEM_RELEASE);
            }
        }
        Ok(())
    }

    fn run_step_on(&mut self, entry: usize, action: Action) -> Result<FpsState> {
        self.reap_pending_thread()?;
        let mut bytes = [0u8; PARAM_SIZE];
        put_u32(&mut bytes, 0, PARAM_SIZE as u32);
        put_u32(&mut bytes, 4, 1);
        put_u32(
            &mut bytes,
            8,
            match action {
                Action::Status => 0,
                Action::Default => 1,
                Action::Unlock => 2,
            },
        );
        put_u32(&mut bytes, 12, self.layout.frame_counter_offset);
        put_u32(&mut bytes, 16, self.layout.throttle_count_offset);
        put_u32(&mut bytes, 20, self.layout.reset_counter_value as u32);
        put_u64(
            &mut bytes,
            24,
            self.studio_base + self.layout.vtable_slot_rva,
        );
        put_u64(
            &mut bytes,
            32,
            self.studio_base + self.layout.should_skip_rva,
        );

        let mut remote = self.process.allocate(PARAM_SIZE)?;
        self.process.write(remote.address, &bytes)?;
        let thread = self.process.start_thread(entry, remote.address)?;
        let waited = unsafe { WaitForSingleObject(thread, 3_000) };
        if waited != WAIT_OBJECT_0 {
            if waited == WAIT_TIMEOUT {
                let addr = remote.address;
                remote.address = 0;
                self.pending_thread = Some((thread, addr));
                bail!("Studio FPS helper exceeded its 3000ms deadline (process may be suspended)");
            }
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(thread);
            }
            remote.address = 0;
            bail!("Could not wait for the Studio FPS helper: {error}");
        }
        let mut exit_code = 0;
        let ok = unsafe { GetExitCodeThread(thread, &mut exit_code) };
        unsafe {
            CloseHandle(thread);
        }
        anyhow::ensure!(
            ok != 0,
            "Could not read the Studio FPS helper result: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            exit_code == 0,
            "Studio FPS helper rejected its request ({exit_code})"
        );
        self.process.read(remote.address, &mut bytes)?;
        anyhow::ensure!(
            read_u32(&bytes, 40)? == 1,
            "{}",
            String::from_utf8_lossy(&bytes[80..]).trim_end_matches('\0')
        );
        Ok(FpsState {
            unlocked: read_u32(&bytes, 44)? != 0,
            hook_installed: read_u32(&bytes, 52)? != 0,
            last_throttle_count: read_i32(&bytes, 56)?,
            render_checks: read_u64(&bytes, 64)?,
            unthrottled_frames: read_u64(&bytes, 72)?,
        })
    }

    pub(crate) fn step(&mut self, action: Action) -> Result<FpsState> {
        if action == Action::Unlock && !self.active_is_current {
            self.upgrade_active_helper()?;
        }
        let state = self.run_step_on(self.active_entry, action)?;
        match action {
            Action::Unlock if state.unlocked => self.owns_unlock = true,
            Action::Default => self.owns_unlock = false,
            _ => {}
        }
        Ok(state)
    }
}

impl Drop for FpsController {
    fn drop(&mut self) {
        if self.owns_unlock && self.pending_thread.is_none() {
            let _ = self.step(Action::Default);
        }
        if let Some((thread, addr)) = self.pending_thread.take() {
            unsafe {
                if WaitForSingleObject(thread, 0) == WAIT_OBJECT_0 && addr != 0 {
                    VirtualFreeEx(self.process.handle, addr as *mut c_void, 0, MEM_RELEASE);
                }
                CloseHandle(thread);
            }
        }
    }
}

pub(super) fn probe(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<serde_json::Value> {
    let layout = cached_layout(&studio.path)?;
    verify_loaded_image(memory, studio, layout.image_stamp)?;

    let slot_value = memory.read_u64(studio.base + layout.vtable_slot_rva)? as usize;
    let expected = studio.base + layout.should_skip_rva;
    if slot_value != expected {
        let loaded = modules(memory.pid)?;
        let owner = helper_owning_slot(&loaded, slot_value).with_context(|| {
            format!(
                "Live RenderSchedulerState vtable slot 0x{slot_value:X} does not match expected 0x{expected:X}"
            )
        })?;
        remote_export_address(memory, owner, "ReniumFpsStep")?;
    }

    Ok(serde_json::json!({
        "pushRenderStateRva": layout.push_rva,
        "popRenderStateRva": layout.pop_rva,
        "vtableSlotRva": layout.vtable_slot_rva,
        "shouldSkipRenderRva": layout.should_skip_rva,
        "frameCounterOffset": layout.frame_counter_offset,
        "throttleCountOffset": layout.throttle_count_offset,
        "resetCounterValue": layout.reset_counter_value,
    }))
}

fn cached_layout(path: &Path) -> Result<RenderThrottleLayout> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().ok();
    let cache = FPS_LAYOUTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock_recover()
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
    {
        return Ok(cached.layout);
    }
    let mapped = ImageView::open(path)?;
    let layout = discover_fps_layout(&PeImage::parse(mapped.bytes())?)?;
    cache.lock_recover().insert(
        path.to_owned(),
        CachedFpsLayout {
            len: metadata.len(),
            modified,
            layout,
        },
    );
    Ok(layout)
}

fn find_unique_rdata_exact(image: &PeImage<'_>, needle: &[u8]) -> Result<usize> {
    let rdata = image.section(b".rdata")?;
    let bytes = slice(image.bytes, rdata.raw_offset, rdata.raw_size)?;
    let hits = memmem::find_iter(bytes, needle)
        .filter(|&pos| pos == 0 || bytes[pos - 1] == 0)
        .map(|pos| image.offset_to_rva(rdata.raw_offset + pos))
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        hits.len() == 1,
        "Expected 1 Studio .rdata string {:?}, found {}",
        String::from_utf8_lossy(needle),
        hits.len()
    );
    Ok(hits[0])
}

fn rip_lea_target(image: &PeImage<'_>, offset: usize) -> Option<usize> {
    let bytes = image.bytes.get(offset..offset + 7)?;
    if !matches!(bytes[0], 0x48 | 0x4c) || bytes[1] != 0x8d || bytes[2] & 0xc7 != 0x05 {
        return None;
    }
    let disp = i32::from_le_bytes(bytes[3..7].try_into().ok()?) as isize;
    let next_rva = image.offset_to_rva(offset + 7).ok()? as isize;
    usize::try_from(next_rva.checked_add(disp)?).ok()
}

fn find_unique_function_referencing_string(
    image: &PeImage<'_>,
    string_rva: usize,
    label: &str,
) -> Result<(usize, Vec<Instruction>)> {
    let text = image.section(b".text")?;
    let code = slice(image.bytes, text.raw_offset, text.raw_size)?;
    let mut functions = Vec::new();
    for index in memchr::memchr_iter(0x8d, code) {
        let Some(offset) = index.checked_sub(1).map(|i| text.raw_offset + i) else {
            continue;
        };
        if rip_lea_target(image, offset) != Some(string_rva) {
            continue;
        }
        let Ok((start_off, end_off)) = image.function_bounds(offset) else {
            continue;
        };
        let fn_rva = image.offset_to_rva(start_off)?;
        let insns = decode_range(image, start_off, end_off)?;
        if insns.iter().any(|insn| {
            insn.mnemonic() == Mnemonic::Lea
                && insn.is_ip_rel_memory_operand()
                && insn.ip_rel_memory_address() as usize == string_rva
        }) {
            functions.push((fn_rva, insns));
        }
    }
    functions.sort_by_key(|(rva, _)| *rva);
    functions.dedup_by_key(|(rva, _)| *rva);
    anyhow::ensure!(
        functions.len() == 1,
        "Expected 1 Studio function referencing {label}, found {}",
        functions.len()
    );
    Ok(functions.remove(0))
}

fn decode_range(image: &PeImage<'_>, start_off: usize, end_off: usize) -> Result<Vec<Instruction>> {
    let begin_rva = image.offset_to_rva(start_off)?;
    let bytes = slice(image.bytes, start_off, end_off.saturating_sub(start_off))?;
    let decoder = Decoder::with_ip(64, bytes, begin_rva as u64, DecoderOptions::NONE);
    let mut insns = Vec::new();
    for insn in decoder {
        if insn.is_invalid() {
            bail!(
                "Invalid x86-64 instruction at RVA 0x{:X} inside Studio function",
                insn.ip()
            );
        }
        insns.push(insn);
    }
    Ok(insns)
}

fn decode_function(image: &PeImage<'_>, fn_rva: usize) -> Result<Vec<Instruction>> {
    let offset = image.rva_to_offset(fn_rva)?;
    let (start_off, end_off) = image.function_bounds(offset)?;
    anyhow::ensure!(
        image.offset_to_rva(start_off)? == fn_rva,
        "Studio target 0x{fn_rva:X} is not the start of an x64 unwind function"
    );
    decode_range(image, start_off, end_off)
}

fn find_throttle_xadd_offset(insns: &[Instruction], anchor_rva: usize) -> Result<u32> {
    let anchor_idx = insns
        .iter()
        .position(|insn| {
            insn.mnemonic() == Mnemonic::Lea
                && insn.is_ip_rel_memory_operand()
                && insn.ip_rel_memory_address() as usize == anchor_rva
        })
        .context("RenderSchedulerState method missing throttle FLog LEA")?;
    let mut this_regs = HashSet::from([Register::RCX]);
    let mut last_offset = None;
    for insn in &insns[..anchor_idx] {
        if insn.mnemonic() == Mnemonic::Mov
            && insn.op0_kind() == OpKind::Register
            && insn.op1_kind() == OpKind::Register
            && this_regs.contains(&insn.op1_register())
        {
            this_regs.insert(insn.op0_register());
        } else if insn.mnemonic() == Mnemonic::Xadd
            && insn.has_lock_prefix()
            && insn.op0_kind() == OpKind::Memory
            && insn.memory_size() == MemorySize::UInt32
            && this_regs.contains(&insn.memory_base())
            && insn.memory_index() == Register::None
        {
            last_offset = Some(
                u32::try_from(insn.memory_displacement64())
                    .context("RenderSchedulerState throttle counter offset is out of range")?,
            );
        }
    }
    last_offset.context("RenderSchedulerState throttle atomic update not found before FLog anchor")
}

pub(super) fn discover_fps_layout(image: &PeImage<'_>) -> Result<RenderThrottleLayout> {
    let push_str = find_unique_rdata_exact(image, PUSH_THROTTLE_ANCHOR)?;
    let pop_str = find_unique_rdata_exact(image, POP_THROTTLE_ANCHOR)?;
    let (push_rva, push_insns) = find_unique_function_referencing_string(
        image,
        push_str,
        "RenderSchedulerState::pushRenderState",
    )?;
    let (pop_rva, pop_insns) = find_unique_function_referencing_string(
        image,
        pop_str,
        "RenderSchedulerState::popRenderState",
    )?;

    let push_offset = find_throttle_xadd_offset(&push_insns, push_str)?;
    let pop_offset = find_throttle_xadd_offset(&pop_insns, pop_str)?;
    anyhow::ensure!(
        push_offset == pop_offset,
        "RenderSchedulerState push ({push_offset:#x}) and pop ({pop_offset:#x}) disagree on throttle counter offset"
    );
    let throttle_count_offset = push_offset;

    let vtable_entries = find_vtable_around_pair(image, push_rva, pop_rva)?;
    let mut matches = Vec::new();
    for &(slot_rva, fn_rva) in &vtable_entries {
        if fn_rva == push_rva || fn_rva == pop_rva {
            continue;
        }
        if let Some((frame_counter_offset, reset_counter_value)) =
            match_should_skip_render(image, fn_rva, throttle_count_offset)
        {
            matches.push((slot_rva, fn_rva, frame_counter_offset, reset_counter_value));
        }
    }
    anyhow::ensure!(
        matches.len() == 1,
        "RenderSchedulerState vtable resolved {} shouldSkipRender candidates",
        matches.len()
    );
    let (vtable_slot_rva, should_skip_rva, frame_counter_offset, reset_counter_value) = matches[0];
    Ok(RenderThrottleLayout {
        push_rva,
        pop_rva,
        vtable_slot_rva,
        should_skip_rva,
        frame_counter_offset,
        throttle_count_offset,
        reset_counter_value,
        image_stamp: image.image_stamp,
    })
}

fn find_vtable_around_pair(
    image: &PeImage<'_>,
    push_rva: usize,
    pop_rva: usize,
) -> Result<Vec<(usize, usize)>> {
    let rdata = image.section(b".rdata")?;
    let table = slice(image.bytes, rdata.raw_offset, rdata.raw_size)?;
    let mut pair_bytes = [0u8; 16];
    pair_bytes[..8].copy_from_slice(&((image.image_base + push_rva) as u64).to_le_bytes());
    pair_bytes[8..].copy_from_slice(&((image.image_base + pop_rva) as u64).to_le_bytes());

    let pairs: Vec<usize> = memmem::find_iter(table, &pair_bytes)
        .filter(|&pos| pos % 8 == 0)
        .collect();
    anyhow::ensure!(
        pairs.len() == 1,
        "Expected 1 RenderSchedulerState vtable pair in .rdata, found {}",
        pairs.len()
    );
    let anchor = pairs[0];
    let read_exec_fn = |pos: usize| -> Option<usize> {
        let va = read_u64(table, pos).ok()? as usize;
        let rva = va.checked_sub(image.image_base)?;
        image.require_executable_rva(rva).ok()?;
        Some(rva)
    };

    let mut start = anchor;
    while start >= 8 && read_exec_fn(start - 8).is_some() {
        start -= 8;
    }
    let mut end = anchor + 16;
    while end + 8 <= table.len() && read_exec_fn(end).is_some() {
        end += 8;
    }

    let mut entries = Vec::new();
    for pos in (start..end).step_by(8) {
        if let Some(fn_rva) = read_exec_fn(pos) {
            entries.push((image.offset_to_rva(rdata.raw_offset + pos)?, fn_rva));
        }
    }
    Ok(entries)
}

fn is_ax_reg(reg: Register) -> bool {
    matches!(
        reg,
        Register::AL | Register::AX | Register::EAX | Register::RAX
    )
}

fn is_setcc(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Seta
            | Mnemonic::Setae
            | Mnemonic::Setb
            | Mnemonic::Setbe
            | Mnemonic::Sete
            | Mnemonic::Setg
            | Mnemonic::Setge
            | Mnemonic::Setl
            | Mnemonic::Setle
            | Mnemonic::Setne
    )
}

fn is_conditional_jump(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Ja
            | Mnemonic::Jae
            | Mnemonic::Jb
            | Mnemonic::Jbe
            | Mnemonic::Je
            | Mnemonic::Jg
            | Mnemonic::Jge
            | Mnemonic::Jl
            | Mnemonic::Jle
            | Mnemonic::Jne
            | Mnemonic::Jno
            | Mnemonic::Jnp
            | Mnemonic::Jns
            | Mnemonic::Jo
            | Mnemonic::Jp
            | Mnemonic::Js
    )
}

fn eval_cmp_condition(mnemonic: Mnemonic, lhs: i64, rhs: i64) -> Option<bool> {
    let ul = lhs as u64;
    let ur = rhs as u64;
    Some(match mnemonic {
        Mnemonic::Je | Mnemonic::Sete => lhs == rhs,
        Mnemonic::Jne | Mnemonic::Setne => lhs != rhs,
        Mnemonic::Jl | Mnemonic::Setl => lhs < rhs,
        Mnemonic::Jle | Mnemonic::Setle => lhs <= rhs,
        Mnemonic::Jg | Mnemonic::Setg => lhs > rhs,
        Mnemonic::Jge | Mnemonic::Setge => lhs >= rhs,
        Mnemonic::Jb | Mnemonic::Setb => ul < ur,
        Mnemonic::Jbe | Mnemonic::Setbe => ul <= ur,
        Mnemonic::Ja | Mnemonic::Seta => ul > ur,
        Mnemonic::Jae | Mnemonic::Setae => ul >= ur,
        _ => return None,
    })
}

fn const_bool_write_to_ax(insn: &Instruction) -> Option<Option<bool>> {
    if insn.op0_kind() != OpKind::Register || !is_ax_reg(insn.op0_register()) {
        return None;
    }
    if matches!(insn.mnemonic(), Mnemonic::Xor | Mnemonic::Sub)
        && insn.op1_kind() == OpKind::Register
        && insn.op0_register() == insn.op1_register()
    {
        return Some(Some(false));
    }
    if insn.mnemonic() == Mnemonic::Mov
        && insn.op1_kind() != OpKind::Register
        && insn.op1_kind() != OpKind::Memory
    {
        return match insn.immediate(1) {
            0 => Some(Some(false)),
            1 => Some(Some(true)),
            _ => Some(None),
        };
    }
    if insn.mnemonic() == Mnemonic::Cmp || insn.mnemonic() == Mnemonic::Test {
        return None;
    }
    Some(None)
}

fn trace_bool_return(
    insns: &[Instruction],
    ip_to_idx: &HashMap<u64, usize>,
    mut idx: usize,
    mut current_bool: Option<bool>,
) -> Option<bool> {
    let mut visited = HashSet::new();
    for _ in 0..64 {
        if !visited.insert(idx) {
            return None;
        }
        let insn = insns.get(idx)?;
        match insn.mnemonic() {
            Mnemonic::Ret => return current_bool,
            Mnemonic::Jmp => {
                if insn.op0_kind() != OpKind::NearBranch64 {
                    return None;
                }
                idx = *ip_to_idx.get(&insn.near_branch64())?;
                continue;
            }
            Mnemonic::Call => return None,
            m if is_conditional_jump(m) => return None,
            _ => {
                if let Some(written) = const_bool_write_to_ax(insn) {
                    current_bool = written;
                }
                idx += 1;
            }
        }
    }
    None
}

fn preserves_flags(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Mov
            | Mnemonic::Movzx
            | Mnemonic::Movsx
            | Mnemonic::Movsxd
            | Mnemonic::Lea
            | Mnemonic::Nop
            | Mnemonic::Push
            | Mnemonic::Pop
    )
}

fn verify_render_on_zero_branch(
    insns: &[Instruction],
    ip_to_idx: &HashMap<u64, usize>,
    cmp_idx: usize,
    threshold: i64,
    mask: u32,
) -> bool {
    let mut branch_idx = None;
    for offset in 1..=4 {
        let Some(insn) = insns.get(cmp_idx + offset) else {
            break;
        };
        if is_conditional_jump(insn.mnemonic()) || is_setcc(insn.mnemonic()) {
            branch_idx = Some(cmp_idx + offset);
            break;
        }
        if !preserves_flags(insn.mnemonic()) {
            break;
        }
    }
    let Some(branch_idx) = branch_idx else {
        return false;
    };
    let branch = &insns[branch_idx];
    let nonzero_val = threshold.max(1).min(i64::from(mask));
    let Some(on_zero) = eval_cmp_condition(branch.mnemonic(), 0, threshold) else {
        return false;
    };
    let Some(on_nonzero) = eval_cmp_condition(branch.mnemonic(), nonzero_val, threshold) else {
        return false;
    };

    if is_setcc(branch.mnemonic()) {
        if branch.op0_kind() != OpKind::Register || branch.op0_register() != Register::AL {
            return false;
        }
        let ret_zero = trace_bool_return(insns, ip_to_idx, branch_idx + 1, Some(on_zero));
        let ret_nonzero = trace_bool_return(insns, ip_to_idx, branch_idx + 1, Some(on_nonzero));
        return ret_zero == Some(false) && ret_nonzero == Some(true);
    }

    if branch.op0_kind() != OpKind::NearBranch64 {
        return false;
    }
    let Some(&target_idx) = ip_to_idx.get(&branch.near_branch64()) else {
        return false;
    };
    let fallthrough_idx = branch_idx + 1;
    let zero_idx = if on_zero { target_idx } else { fallthrough_idx };
    let nonzero_idx = if on_nonzero {
        target_idx
    } else {
        fallthrough_idx
    };
    let ret_zero = trace_bool_return(insns, ip_to_idx, zero_idx, None);
    let ret_nonzero = trace_bool_return(insns, ip_to_idx, nonzero_idx, None);
    ret_zero == Some(false) && ret_nonzero == Some(true)
}

fn watched_arg_bit(reg: Register) -> Option<u16> {
    Some(match reg {
        Register::R8 | Register::R8D | Register::R8W | Register::R8L => 1 << 0,
        Register::R9 | Register::R9D | Register::R9W | Register::R9L => 1 << 1,
        Register::XMM0 | Register::YMM0 | Register::ZMM0 => 1 << 2,
        Register::XMM1 | Register::YMM1 | Register::ZMM1 => 1 << 3,
        Register::XMM2 | Register::YMM2 | Register::ZMM2 => 1 << 4,
        Register::XMM3 | Register::YMM3 | Register::ZMM3 => 1 << 5,
        Register::XMM4 | Register::YMM4 | Register::ZMM4 => 1 << 6,
        Register::XMM5 | Register::YMM5 | Register::ZMM5 => 1 << 7,
        _ => return None,
    })
}

fn is_self_zeroing(insn: &Instruction) -> bool {
    matches!(
        insn.mnemonic(),
        Mnemonic::Xor | Mnemonic::Sub | Mnemonic::Pxor | Mnemonic::Xorps | Mnemonic::Xorpd
    ) && insn.op0_kind() == OpKind::Register
        && insn.op1_kind() == OpKind::Register
        && insn.op0_register() == insn.op1_register()
}

fn op0_reads_register(mnemonic: Mnemonic) -> bool {
    !matches!(
        mnemonic,
        Mnemonic::Mov
            | Mnemonic::Movzx
            | Mnemonic::Movsx
            | Mnemonic::Movsxd
            | Mnemonic::Lea
            | Mnemonic::Pop
            | Mnemonic::Movaps
            | Mnemonic::Movups
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movss
            | Mnemonic::Movsd
    ) && !is_setcc(mnemonic)
}

fn op0_writes_register(mnemonic: Mnemonic) -> bool {
    !matches!(mnemonic, Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push)
}

fn stack_home_bit(caller_off: i64) -> Option<u8> {
    if (8..=32).contains(&caller_off) && caller_off % 8 == 0 {
        Some(1 << ((caller_off - 8) / 8))
    } else {
        None
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct AbiState {
    written_args: u16,
    home_written: u8,
    rsp_depth: i64,
    al_is_bool: bool,
}

fn check_and_step_abi(insn: &Instruction, mut state: AbiState) -> Option<AbiState> {
    if is_self_zeroing(insn) {
        if let Some(bit) = watched_arg_bit(insn.op0_register()) {
            state.written_args |= bit;
        }
        if is_ax_reg(insn.op0_register()) {
            state.al_is_bool = true;
        }
        return Some(state);
    }

    let copies_stack_pointer = insn.op_count() == 2
        && insn.op0_kind() == OpKind::Register
        && insn.op0_register() != Register::RSP
        && (insn.op1_kind() == OpKind::Register && insn.op1_register() == Register::RSP
            || insn.mnemonic() == Mnemonic::Lea && insn.memory_base() == Register::RSP);
    if copies_stack_pointer {
        return None;
    }

    let op_count = insn.op_count();
    for op in 0..op_count {
        match insn.op_kind(op) {
            OpKind::Register => {
                let is_read = op > 0 || op0_reads_register(insn.mnemonic());
                if is_read
                    && let Some(bit) = watched_arg_bit(insn.op_register(op))
                    && state.written_args & bit == 0
                {
                    return None;
                }
            }
            OpKind::Memory => {
                for reg in [insn.memory_base(), insn.memory_index()] {
                    if let Some(bit) = watched_arg_bit(reg)
                        && state.written_args & bit == 0
                    {
                        return None;
                    }
                }
                if insn.memory_base() == Register::RSP && insn.memory_index() == Register::None {
                    let caller_off = (insn.memory_displacement64() as i64) - state.rsp_depth;
                    let is_read = op > 0 || op0_reads_register(insn.mnemonic());
                    if is_read {
                        if caller_off >= 0x28 {
                            return None;
                        }
                        if let Some(bit) = stack_home_bit(caller_off)
                            && state.home_written & bit == 0
                        {
                            return None;
                        }
                    } else if let Some(bit) = stack_home_bit(caller_off) {
                        state.home_written |= bit;
                    }
                }
            }
            _ => {}
        }
    }

    if insn.op0_kind() == OpKind::Register {
        let dst = insn.op0_register();
        if op0_writes_register(insn.mnemonic())
            && let Some(bit) = watched_arg_bit(dst)
        {
            state.written_args |= bit;
        }
        if is_ax_reg(dst) {
            if is_setcc(insn.mnemonic()) && dst == Register::AL {
                state.al_is_bool = true;
            } else if let Some(written) = const_bool_write_to_ax(insn) {
                state.al_is_bool = written.is_some();
            }
        }
    }

    match insn.mnemonic() {
        Mnemonic::Push => state.rsp_depth += 8,
        Mnemonic::Pop => state.rsp_depth -= 8,
        Mnemonic::Sub
            if insn.op0_kind() == OpKind::Register && insn.op0_register() == Register::RSP =>
        {
            if insn.op1_kind() == OpKind::Register {
                return None;
            }
            state.rsp_depth += insn.immediate(1) as i64;
        }
        Mnemonic::Add
            if insn.op0_kind() == OpKind::Register && insn.op0_register() == Register::RSP =>
        {
            if insn.op1_kind() == OpKind::Register {
                return None;
            }
            state.rsp_depth -= insn.immediate(1) as i64;
        }
        Mnemonic::Call => {
            state.al_is_bool = false;
        }
        _ => {}
    }

    Some(state)
}

fn verify_should_skip_abi(insns: &[Instruction], ip_to_idx: &HashMap<u64, usize>) -> bool {
    if insns.is_empty() {
        return false;
    }
    let mut states: Vec<Option<AbiState>> = vec![None; insns.len()];
    let mut queue = VecDeque::new();
    let entry = AbiState {
        written_args: 0,
        home_written: 0,
        rsp_depth: 0,
        al_is_bool: false,
    };
    states[0] = Some(entry);
    queue.push_back(0usize);

    let mut saw_ret = false;
    while let Some(idx) = queue.pop_front() {
        let Some(in_state) = states[idx] else {
            continue;
        };
        let insn = &insns[idx];
        if insn.mnemonic() == Mnemonic::Ret {
            if !in_state.al_is_bool {
                return false;
            }
            saw_ret = true;
            continue;
        }
        let Some(out_state) = check_and_step_abi(insn, in_state) else {
            return false;
        };

        let mut nexts = Vec::with_capacity(2);
        if insn.mnemonic() == Mnemonic::Jmp {
            if insn.op0_kind() != OpKind::NearBranch64 {
                return false;
            }
            let Some(&target) = ip_to_idx.get(&insn.near_branch64()) else {
                return false;
            };
            nexts.push(target);
        } else if is_conditional_jump(insn.mnemonic()) {
            if insn.op0_kind() != OpKind::NearBranch64 {
                return false;
            }
            let Some(&target) = ip_to_idx.get(&insn.near_branch64()) else {
                return false;
            };
            nexts.push(target);
            if idx + 1 < insns.len() {
                nexts.push(idx + 1);
            }
        } else if idx + 1 < insns.len() {
            nexts.push(idx + 1);
        }

        for next in nexts {
            match states[next] {
                None => {
                    states[next] = Some(out_state);
                    queue.push_back(next);
                }
                Some(prev) => {
                    if prev.rsp_depth != out_state.rsp_depth {
                        return false;
                    }
                    let merged = AbiState {
                        written_args: prev.written_args & out_state.written_args,
                        home_written: prev.home_written & out_state.home_written,
                        rsp_depth: prev.rsp_depth,
                        al_is_bool: prev.al_is_bool && out_state.al_is_bool,
                    };
                    if merged != prev {
                        states[next] = Some(merged);
                        queue.push_back(next);
                    }
                }
            }
        }
    }
    saw_ret
}

fn match_should_skip_render(
    image: &PeImage<'_>,
    fn_rva: usize,
    throttle_count_offset: u32,
) -> Option<(u32, i32)> {
    let insns = decode_function(image, fn_rva).ok()?;
    let ip_to_idx: HashMap<u64, usize> = insns
        .iter()
        .enumerate()
        .map(|(idx, insn)| (insn.ip(), idx))
        .collect();
    if !verify_should_skip_abi(&insns, &ip_to_idx) {
        return None;
    }

    let mut this_regs = HashSet::from([Register::RCX]);
    let mut loaded_counter: Option<(Register, u32)> = None;
    let mut incremented = false;
    let mut modulus_mask: Option<u32> = None;
    let mut stored_counter_offset: Option<u32> = None;
    let mut read_throttle = false;
    let mut verified_branch = false;

    for (idx, insn) in insns.iter().enumerate() {
        if insn.mnemonic() == Mnemonic::Mov
            && insn.op0_kind() == OpKind::Register
            && insn.op1_kind() == OpKind::Register
            && this_regs.contains(&insn.op1_register())
        {
            this_regs.insert(insn.op0_register());
            continue;
        }
        if insn.mnemonic() == Mnemonic::Mov
            && insn.op0_kind() == OpKind::Register
            && insn.op1_kind() == OpKind::Memory
            && insn.memory_size() == MemorySize::UInt32
            && this_regs.contains(&insn.memory_base())
            && insn.memory_index() == Register::None
        {
            let Ok(off) = u32::try_from(insn.memory_displacement64()) else {
                continue;
            };
            if off == throttle_count_offset {
                read_throttle = true;
            } else if stored_counter_offset.is_none() && loaded_counter.is_none() {
                loaded_counter = Some((insn.op0_register(), off));
            }
            continue;
        }
        if let Some((reg, off)) = loaded_counter
            && stored_counter_offset.is_none()
        {
            if (insn.mnemonic() == Mnemonic::Inc && insn.op0_register() == reg)
                || (insn.mnemonic() == Mnemonic::Add
                    && insn.op0_register() == reg
                    && insn.op1_kind() != OpKind::Register
                    && insn.immediate(1) == 1)
            {
                incremented = true;
            } else if incremented
                && insn.mnemonic() == Mnemonic::And
                && insn.op0_register() == reg
                && insn.op1_kind() != OpKind::Register
            {
                let mask = (insn.immediate(1) as u32) & 0x7fff_ffff;
                if mask >= 1 && (mask + 1).is_power_of_two() && mask <= 1023 {
                    modulus_mask = Some(mask);
                }
            } else if modulus_mask.is_some()
                && insn.mnemonic() == Mnemonic::Mov
                && insn.op0_kind() == OpKind::Memory
                && insn.op1_kind() == OpKind::Register
                && insn.op1_register() == reg
                && insn.memory_size() == MemorySize::UInt32
                && this_regs.contains(&insn.memory_base())
                && insn.memory_index() == Register::None
                && insn.memory_displacement64() == u64::from(off)
            {
                stored_counter_offset = Some(off);
            }
        }
        if let (Some(counter_off), Some(mask)) = (stored_counter_offset, modulus_mask)
            && read_throttle
            && insn.mnemonic() == Mnemonic::Cmp
            && insn.op0_kind() == OpKind::Memory
            && insn.memory_size() == MemorySize::UInt32
            && this_regs.contains(&insn.memory_base())
            && insn.memory_index() == Register::None
            && insn.memory_displacement64() == u64::from(counter_off)
            && insn.op1_kind() != OpKind::Register
            && insn.op1_kind() != OpKind::Memory
        {
            let threshold = insn.immediate(1) as i64;
            if threshold >= 1
                && threshold <= i64::from(mask)
                && verify_render_on_zero_branch(&insns, &ip_to_idx, idx, threshold, mask)
            {
                verified_branch = true;
            }
        }
    }

    match (stored_counter_offset, modulus_mask) {
        (Some(counter_off), Some(mask)) if read_throttle && verified_branch => {
            Some((counter_off, mask as i32))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_unlock_does_not_require_datamodel_task_continuation_abi() {
        assert!(
            task_abi::from_report(NativeOperation::FpsUnlock, &serde_json::json!({}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn native_fps_helper_regression_passes() {
        assert!(
            crate::system::tools::hidden_command(
                Path::new(env!("OUT_DIR")).join("renium-fps-test.exe")
            )
            .status()
            .expect("run Windows FPS helper regression")
            .success()
        );
    }

    #[derive(Clone, Copy)]
    enum SyntheticBranch {
        /// `cmp [rbx+0x68], 1; jge skip; xor al, al; ret; skip: mov al, 1; ret` (renders on 0)
        RenderOnZeroJge,
        /// `cmp [rbx+0x68], 1; jl render; mov al, 1; ret; render: xor al, al; ret` (renders on 0)
        RenderOnZeroJl,
        /// `cmp [rbx+0x68], 1; setge al; ret` (renders on 0)
        RenderOnZeroSetge,
        /// Inverted: `cmp [rbx+0x68], 1; jl skip; xor al, al; ret; skip: mov al, 1; ret` (skips on 0!)
        InvertedSkipsOnZero,
        /// No branch after `cmp` at all
        NoJump,
        /// Non-boolean return (`mov eax, 2`)
        NonBooleanReturn,
    }

    #[derive(Clone)]
    struct SyntheticConfig {
        push_anchor: Vec<u8>,
        pop_anchor: Vec<u8>,
        push_throttle_offset: i32,
        pop_throttle_offset: i32,
        should_skip_modulo_mask: u32,
        include_vtable_pair: bool,
        branch: SyntheticBranch,
        prologue_extra: Vec<u8>,
    }

    impl Default for SyntheticConfig {
        fn default() -> Self {
            Self {
                push_anchor: PUSH_THROTTLE_ANCHOR.to_vec(),
                pop_anchor: POP_THROTTLE_ANCHOR.to_vec(),
                push_throttle_offset: 0x74,
                pop_throttle_offset: 0x74,
                should_skip_modulo_mask: 0x8000_0003,
                include_vtable_pair: true,
                branch: SyntheticBranch::RenderOnZeroJge,
                prologue_extra: Vec::new(),
            }
        }
    }

    fn emit_rip_rel(code: &mut Vec<u8>, code_rva: usize, prefix: &[u8], target_rva: usize) {
        code.extend_from_slice(prefix);
        let next_rva = code_rva + code.len() + 4;
        let disp = (target_rva as isize - next_rva as isize) as i32;
        code.extend_from_slice(&disp.to_le_bytes());
    }

    fn build_synthetic_pe(cfg: &SyntheticConfig) -> Vec<u8> {
        const IMAGE_BASE: u64 = 0x1_4000_0000;
        const TEXT_RVA: usize = 0x1000;
        const TEXT_OFF: usize = 0x400;
        const TEXT_SIZE: usize = 0x1000;
        const RDATA_RVA: usize = 0x2000;
        const RDATA_OFF: usize = 0x1400;
        const RDATA_SIZE: usize = 0x1000;
        const PDATA_RVA: usize = 0x3000;
        const PDATA_OFF: usize = 0x2400;
        const PDATA_SIZE: usize = 3 * 12;

        let mut img = vec![0u8; PDATA_OFF + PDATA_SIZE];
        img[0..2].copy_from_slice(b"MZ");
        img[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let pe = 0x80;
        img[pe..pe + 4].copy_from_slice(b"PE\0\0");
        img[pe + 6..pe + 8].copy_from_slice(&3u16.to_le_bytes());
        img[pe + 20..pe + 22].copy_from_slice(&0xF0u16.to_le_bytes());
        let opt = pe + 24;
        img[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        img[opt + 24..opt + 32].copy_from_slice(&IMAGE_BASE.to_le_bytes());

        let sections: [(&[u8; 8], usize, usize, usize, u32); 3] = [
            (b".text\0\0\0", TEXT_RVA, TEXT_OFF, TEXT_SIZE, 0x6000_0020),
            (b".rdata\0\0", RDATA_RVA, RDATA_OFF, RDATA_SIZE, 0x4000_0040),
            (b".pdata\0\0", PDATA_RVA, PDATA_OFF, PDATA_SIZE, 0x4000_0040),
        ];
        for (i, (name, va, raw_off, raw_size, chars)) in sections.iter().enumerate() {
            let off = opt + 0xF0 + i * 40;
            img[off..off + 8].copy_from_slice(*name);
            img[off + 8..off + 12].copy_from_slice(&(*raw_size as u32).to_le_bytes());
            img[off + 12..off + 16].copy_from_slice(&(*va as u32).to_le_bytes());
            img[off + 16..off + 20].copy_from_slice(&(*raw_size as u32).to_le_bytes());
            img[off + 20..off + 24].copy_from_slice(&(*raw_off as u32).to_le_bytes());
            img[off + 36..off + 40].copy_from_slice(&chars.to_le_bytes());
        }

        let mut rdata_cur = 0usize;
        let mut put_rdata = |bytes: &[u8]| -> usize {
            rdata_cur = (rdata_cur + 8) & !7;
            let rva = RDATA_RVA + rdata_cur;
            img[RDATA_OFF + rdata_cur..RDATA_OFF + rdata_cur + bytes.len()].copy_from_slice(bytes);
            rdata_cur += bytes.len();
            rva
        };
        let rva_push_str = put_rdata(&cfg.push_anchor);
        let rva_pop_str = put_rdata(&cfg.pop_anchor);

        let fn_should_skip = TEXT_RVA;
        let fn_push = TEXT_RVA + 0x100;
        let fn_pop = TEXT_RVA + 0x200;

        let mut pdata_entries = Vec::new();
        let mut write_fn = |rva: usize, bytes: &[u8]| {
            let off = TEXT_OFF + (rva - TEXT_RVA);
            img[off..off + bytes.len()].copy_from_slice(bytes);
            pdata_entries.push((rva, rva + bytes.len()));
        };

        let mut code = Vec::new();
        code.extend_from_slice(&cfg.prologue_extra);
        code.extend_from_slice(&[0x8B, 0x41, 0x68, 0xFF, 0xC0, 0x48, 0x8B, 0xD9, 0x25]);
        code.extend_from_slice(&cfg.should_skip_modulo_mask.to_le_bytes());
        code.extend_from_slice(&[0x89, 0x41, 0x68, 0x8B, 0x83]);
        code.extend_from_slice(&cfg.push_throttle_offset.to_le_bytes());
        code.extend_from_slice(&[0x83, 0x7B, 0x68, 0x01]);
        match cfg.branch {
            SyntheticBranch::RenderOnZeroJge => {
                code.extend_from_slice(&[0x7D, 0x03, 0x30, 0xC0, 0xC3, 0xB0, 0x01, 0xC3]);
            }
            SyntheticBranch::RenderOnZeroJl => {
                code.extend_from_slice(&[0x7C, 0x03, 0xB0, 0x01, 0xC3, 0x30, 0xC0, 0xC3]);
            }
            SyntheticBranch::RenderOnZeroSetge => {
                code.extend_from_slice(&[0x0F, 0x9D, 0xC0, 0xC3]);
            }
            SyntheticBranch::InvertedSkipsOnZero => {
                code.extend_from_slice(&[0x7C, 0x03, 0x30, 0xC0, 0xC3, 0xB0, 0x01, 0xC3]);
            }
            SyntheticBranch::NoJump => {
                code.extend_from_slice(&[0x30, 0xC0, 0xC3]);
            }
            SyntheticBranch::NonBooleanReturn => {
                code.extend_from_slice(&[
                    0x7D, 0x03, 0x30, 0xC0, 0xC3, 0xB8, 0x02, 0x00, 0x00, 0x00, 0xC3,
                ]);
            }
        }
        write_fn(fn_should_skip, &code);

        code.clear();
        code.extend_from_slice(&[0xF0, 0x0F, 0xC1, 0x81]);
        code.extend_from_slice(&cfg.push_throttle_offset.to_le_bytes());
        emit_rip_rel(&mut code, fn_push, &[0x48, 0x8D, 0x0D], rva_push_str);
        code.push(0xC3);
        write_fn(fn_push, &code);

        code.clear();
        code.extend_from_slice(&[0xF0, 0x0F, 0xC1, 0x81]);
        code.extend_from_slice(&cfg.pop_throttle_offset.to_le_bytes());
        emit_rip_rel(&mut code, fn_pop, &[0x48, 0x8D, 0x0D], rva_pop_str);
        code.push(0xC3);
        write_fn(fn_pop, &code);

        if cfg.include_vtable_pair {
            rdata_cur = (rdata_cur + 8) & !7;
            for fn_rva in [fn_should_skip, fn_push, fn_pop] {
                let va = IMAGE_BASE + fn_rva as u64;
                img[RDATA_OFF + rdata_cur..RDATA_OFF + rdata_cur + 8]
                    .copy_from_slice(&va.to_le_bytes());
                rdata_cur += 8;
            }
        }

        pdata_entries.sort_by_key(|(begin, _)| *begin);
        for (i, (begin, end)) in pdata_entries.iter().enumerate() {
            let off = PDATA_OFF + i * 12;
            img[off..off + 4].copy_from_slice(&(*begin as u32).to_le_bytes());
            img[off + 4..off + 8].copy_from_slice(&(*end as u32).to_le_bytes());
        }
        img
    }

    #[test]
    fn synthetic_pe_discovery_succeeds_and_refuses_broken_assumptions() {
        for branch in [
            SyntheticBranch::RenderOnZeroJge,
            SyntheticBranch::RenderOnZeroJl,
            SyntheticBranch::RenderOnZeroSetge,
        ] {
            let valid = build_synthetic_pe(&SyntheticConfig {
                branch,
                ..SyntheticConfig::default()
            });
            let layout = discover_fps_layout(&PeImage::parse(&valid).unwrap()).unwrap();
            assert_eq!(layout.frame_counter_offset, 0x68);
            assert_eq!(layout.throttle_count_offset, 0x74);
            assert_eq!(layout.reset_counter_value, 3);
        }

        let scratch_r8 = build_synthetic_pe(&SyntheticConfig {
            prologue_extra: vec![0x45, 0x31, 0xC0, 0x44, 0x89, 0xC0],
            ..SyntheticConfig::default()
        });
        assert!(discover_fps_layout(&PeImage::parse(&scratch_r8).unwrap()).is_ok());

        let broken_cases = [
            SyntheticConfig {
                push_anchor: b"[FLog::Other] unrelated\0".to_vec(),
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                pop_anchor: b"[FLog::Other] unrelated\0".to_vec(),
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                pop_throttle_offset: 0x78,
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                should_skip_modulo_mask: 0x0000_0005,
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                include_vtable_pair: false,
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                branch: SyntheticBranch::InvertedSkipsOnZero,
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                branch: SyntheticBranch::NoJump,
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                branch: SyntheticBranch::NonBooleanReturn,
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                prologue_extra: vec![0x44, 0x89, 0xC0],
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                prologue_extra: vec![0x44, 0x89, 0xC8],
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                prologue_extra: vec![0x66, 0x0F, 0x7E, 0xD0],
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                prologue_extra: vec![0x66, 0x0F, 0x7E, 0xD8],
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                prologue_extra: vec![0x8B, 0x44, 0x24, 0x28],
                ..SyntheticConfig::default()
            },
            SyntheticConfig {
                prologue_extra: vec![0x48, 0x8B, 0xC4],
                ..SyntheticConfig::default()
            },
        ];
        for (i, broken) in broken_cases.iter().enumerate() {
            let bytes = build_synthetic_pe(broken);
            assert!(
                discover_fps_layout(&PeImage::parse(&bytes).unwrap()).is_err(),
                "Expected broken synthetic PE case {i} to be refused"
            );
        }
    }

    #[test]
    fn cross_version_helper_export_resolves_and_hands_off_cleanly() -> Result<()> {
        use windows_sys::Win32::Foundation::FreeLibrary;
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

        let temp = crate::tests::support::temp_dir("fps-cross-version");
        let old_path = temp.join("renium-fps-0000000000000001.dll");
        let new_path = temp.join("renium-fps-0000000000000002.dll");
        fs::write(&old_path, HELPER)?;
        fs::write(&new_path, HELPER)?;

        let old_mod = unsafe { LoadLibraryW(wide(old_path.as_os_str()).as_ptr()) };
        let new_mod = unsafe { LoadLibraryW(wide(new_path.as_os_str()).as_ptr()) };
        anyhow::ensure!(!old_mod.is_null() && !new_mod.is_null());

        let result = (|| -> Result<()> {
            let pid = std::process::id();
            let loaded = modules(pid)?;
            let old_entry_mod = loaded
                .iter()
                .find(|m| {
                    m.name
                        .eq_ignore_ascii_case("renium-fps-0000000000000001.dll")
                })
                .context("old helper module not found")?;
            let new_entry_mod = loaded
                .iter()
                .find(|m| {
                    m.name
                        .eq_ignore_ascii_case("renium-fps-0000000000000002.dll")
                })
                .context("new helper module not found")?;

            let memory = ProcessMemory::open_read_only(pid)?;
            let old_step = remote_export_address(&memory, old_entry_mod, "ReniumFpsStep")?;
            let new_step = remote_export_address(&memory, new_entry_mod, "ReniumFpsStep")?;
            assert_ne!(old_step, new_step);

            #[repr(C)]
            struct FakeState {
                vtable: *mut *mut c_void,
                pad: [u8; 0x60],
                frame_counter: i32,
                throttle_count: i32,
            }
            unsafe extern "system" fn fake_should_skip(
                _self: *mut c_void,
                _a2: *mut c_void,
                _a3: *mut c_void,
                _a4: *mut c_void,
            ) -> bool {
                false
            }
            let mut vtable: [*mut c_void; 2] = [null_mut(), fake_should_skip as *mut c_void];
            let expected = vtable[1] as usize;
            let slot_addr = std::ptr::addr_of_mut!(vtable[1]) as usize;

            type StepFn = unsafe extern "system" fn(*mut c_void) -> u32;
            let call_step = |entry: usize, action: u32| -> Result<(u32, u32, u32)> {
                let mut buf = [0u8; PARAM_SIZE];
                put_u32(&mut buf, 0, PARAM_SIZE as u32);
                put_u32(&mut buf, 4, 1);
                put_u32(&mut buf, 8, action);
                put_u32(
                    &mut buf,
                    12,
                    std::mem::offset_of!(FakeState, frame_counter) as u32,
                );
                put_u32(
                    &mut buf,
                    16,
                    std::mem::offset_of!(FakeState, throttle_count) as u32,
                );
                put_u32(&mut buf, 20, 3);
                put_u64(&mut buf, 24, slot_addr);
                put_u64(&mut buf, 32, expected);
                let func: StepFn = unsafe { transmute(entry) };
                let rc = unsafe { func(buf.as_mut_ptr().cast()) };
                assert_eq!(rc, 0);
                Ok((
                    read_u32(&buf, 40)?,
                    read_u32(&buf, 44)?,
                    read_u32(&buf, 52)?,
                ))
            };

            assert_eq!(call_step(old_step, 2)?, (1, 1, 1));
            let hooked_slot = vtable[1] as usize;
            assert_ne!(hooked_slot, expected);
            assert_eq!(
                helper_owning_slot(&loaded, hooked_slot).map(|m| m.base),
                Some(old_entry_mod.base)
            );
            assert_eq!(call_step(new_step, 0)?.0, 0);
            let owner = helper_owning_slot(&loaded, hooked_slot).unwrap();
            let owner_step = remote_export_address(&memory, owner, "ReniumFpsStep")?;
            assert_eq!(call_step(owner_step, 0)?, (1, 1, 1));
            assert_eq!(call_step(owner_step, 1)?, (1, 0, 0));
            assert_eq!(vtable[1] as usize, expected);
            assert_eq!(call_step(new_step, 2)?, (1, 1, 1));
            assert_eq!(
                helper_owning_slot(&loaded, vtable[1] as usize).map(|m| m.base),
                Some(new_entry_mod.base)
            );
            assert_eq!(call_step(new_step, 1)?, (1, 0, 0));
            assert_eq!(vtable[1] as usize, expected);
            Ok(())
        })();

        unsafe {
            FreeLibrary(new_mod);
            FreeLibrary(old_mod);
        }
        let _ = fs::remove_dir_all(&temp);
        result
    }

    #[test]
    #[ignore = "set RENIUM_STUDIO_EXES to inspect installed Studio executables"]
    fn installed_studio_resolves_fps_layout() -> Result<()> {
        for path in std::env::var("RENIUM_STUDIO_EXES")?.split(';') {
            let mapped = ImageView::open(Path::new(path))?;
            let image = PeImage::parse(mapped.bytes())?;
            let layout = discover_fps_layout(&image)?;
            assert_eq!(layout.reset_counter_value, 3);
            println!("{path}: {layout:#x?}");
        }
        Ok(())
    }
}
