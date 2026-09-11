//! Export-scoped attribute observation. One engine subscription, no Instance walk.
use super::*;
use std::io::{self, Seek, SeekFrom};
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{
    DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, SetEvent};
#[path = "windows_observation_signal.rs"]
pub(super) mod signal;

struct ImageView {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    _mapping: Handle,
    _file: fs::File,
    len: usize,
}
impl ImageView {
    fn open(path: &Path) -> Result<Self> {
        // Deny writes/deletion while slices exist. Only the PE headers, unwind
        // table and selected functions are touched, instead of copying 226 MB.
        let file = fs::OpenOptions::new().read(true).share_mode(1).open(path)?;
        let len = usize::try_from(file.metadata()?.len())?;
        anyhow::ensure!(
            len > 0 && len <= isize::MAX as usize,
            "Invalid Studio image size"
        );
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle() as HANDLE,
                null(),
                PAGE_READONLY,
                0,
                0,
                null(),
            )
        };
        anyhow::ensure!(
            !mapping.is_null(),
            "Cannot map Studio image: {}",
            io::Error::last_os_error()
        );
        let mapping = Handle(mapping);
        let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, len) };
        anyhow::ensure!(
            !view.Value.is_null(),
            "Cannot read Studio image mapping: {}",
            io::Error::last_os_error()
        );
        Ok(Self {
            view,
            _mapping: mapping,
            _file: file,
            len,
        })
    }
    fn bytes(&self) -> &[u8] {
        // The read-only mapping and write-denying file handle outlive this slice.
        unsafe { std::slice::from_raw_parts(self.view.Value.cast(), self.len) }
    }
}
impl Drop for ImageView {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view);
        }
    }
}

#[derive(Clone)]
struct Prepared {
    trace: signal::ObservationTrace,
    code: Arc<Vec<(usize, Vec<u8>)>>,
}
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    result: std::result::Result<Prepared, String>,
}
static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();

fn prepared(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<Prepared> {
    let metadata = fs::metadata(&studio.path)?;
    let modified = metadata.modified().ok();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&studio.path)
        .filter(|entry| entry.len == metadata.len() && entry.modified == modified)
    {
        return cached.result.clone().map_err(anyhow::Error::msg);
    }
    // A disappearing/loading session is not an unsupported executable layout.
    // Only cache the file-backed discovery result, never a transient live read.
    let history = model
        .roots
        .iter()
        .find(|root| {
            read_instance_class(memory, root.instance, model.layout).as_deref()
                == Some("ChangeHistoryService")
        })
        .context("Studio history service is unavailable for signal discovery")?;
    let descriptor =
        find_class_member_descriptor(memory, history.instance, model.layout, "SetEnabled")?;
    anyhow::ensure!(
        read_rtti_type(memory, descriptor, studio.base, studio.size).as_deref()
            == Some(
                ".?AV?$BoundFuncDesc@VChangeHistoryService@RBX@@$$A6AX_N@Z$0A@$00@Reflection@RBX@@"
            ),
        "Studio SetEnabled signature changed"
    );
    let members = memory.read_vec(descriptor + 0x50, 0x58)?;
    let mapped = ImageView::open(&studio.path)?;
    let bytes = mapped.bytes();
    let result = (|| {
        let image = PeImage::parse(bytes)?;
        let mut candidates = Vec::new();
        for offset in (0..0x50).step_by(8) {
            let address = read_u64(&members, offset)? as usize;
            let Some(method) = address.checked_sub(studio.base) else {
                continue;
            };
            if let Ok(trace) = signal::discover(&image, method) {
                anyhow::ensure!(
                    read_u32(&members, offset + 8)? == 0,
                    "Signal method this-adjustment changed"
                );
                candidates.push(trace);
            }
        }
        anyhow::ensure!(
            candidates.len() == 1,
            "Studio attribute signal layout is unrecognized; Renium's detector needs updating"
        );
        let trace = candidates.remove(0);
        let code = trace
            .functions
            .iter()
            .map(|&rva| {
                let offset = image.rva_to_offset(rva)?;
                let (_, end) = image.function_bounds(offset)?;
                Ok((rva, bytes[offset..end].to_vec()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Prepared {
            trace,
            code: Arc::new(code),
        })
    })()
    .map_err(|error: anyhow::Error| format!("{error:#}"));
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            studio.path.clone(),
            Cached {
                len: metadata.len(),
                modified,
                result: result.clone(),
            },
        );
    result.map_err(anyhow::Error::msg)
}
pub(super) fn prepare(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<()> {
    prepared(memory, studio, model).map(|_| ())
}

struct Handle(HANDLE);
// Event/thread handles have no thread affinity.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn event() -> Result<Handle> {
    let handle = unsafe { CreateEventW(null(), 1, 0, null()) };
    anyhow::ensure!(
        !handle.is_null(),
        "Cannot create attribute observation event: {}",
        io::Error::last_os_error()
    );
    Ok(Handle(handle))
}
struct RemoteHandles<'a> {
    memory: &'a ProcessMemory,
    values: Vec<HANDLE>,
}
impl RemoteHandles<'_> {
    fn duplicate(&mut self, local: &Handle) -> Result<usize> {
        let mut target = null_mut();
        anyhow::ensure!(
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    local.0,
                    self.memory.handle,
                    &mut target,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            } != 0,
            "Cannot share observation event: {}",
            io::Error::last_os_error()
        );
        self.values.push(target);
        Ok(target as usize)
    }
}
impl Drop for RemoteHandles<'_> {
    fn drop(&mut self) {
        for handle in &self.values {
            let mut local = null_mut();
            if unsafe {
                DuplicateHandle(
                    self.memory.handle,
                    *handle,
                    GetCurrentProcess(),
                    &mut local,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
                )
            } != 0
            {
                unsafe {
                    CloseHandle(local);
                }
            }
        }
    }
}

pub(crate) struct AttributeGuard {
    stop: Handle,
    thread: Option<Handle>,
    transport: fs::File,
}
impl AttributeGuard {
    fn header(&mut self) -> Result<[u8; CAPTURE_HEADER_SIZE]> {
        self.transport.seek(SeekFrom::Start(0))?;
        let mut header = [0; CAPTURE_HEADER_SIZE];
        self.transport.read_exact(&mut header)?;
        anyhow::ensure!(
            read_u32(&header, 0)? == CAPTURE_HEADER_MAGIC && read_u32(&header, 4)? == 1,
            "Invalid attribute observation response"
        );
        Ok(header)
    }
    pub(crate) fn finish(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        unsafe {
            SetEvent(self.stop.0);
        }
        anyhow::ensure!(
            unsafe { WaitForSingleObject(thread.0, 2500) } == WAIT_OBJECT_0,
            "Studio attribute observation did not finish; export was not accepted"
        );
        let mut exit = 0;
        anyhow::ensure!(
            unsafe { GetExitCodeThread(thread.0, &mut exit) } != 0,
            "Cannot read attribute observation completion"
        );
        let header = self.header()?;
        anyhow::ensure!(
            exit == 0 && read_u32(&header, 8)? == 4 && read_u32(&header, 12)? == 0,
            "{}",
            String::from_utf8_lossy(&header[64..]).trim_end_matches('\0')
        );
        Ok(())
    }
}
impl Drop for AttributeGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub(crate) fn begin_attribute_guard(
    pid: u32,
    title: &str,
    services: &[String],
    lifetime: Duration,
) -> Result<AttributeGuard> {
    begin_observation(pid, title, services, lifetime, None)
}

pub(crate) fn begin_attribute_relay(
    pid: u32,
    title: &str,
    services: &[String],
    lifetime: Duration,
    relay_path: &[String],
) -> Result<AttributeGuard> {
    begin_observation(pid, title, services, lifetime, Some(relay_path))
}

fn begin_observation(
    pid: u32,
    title: &str,
    services: &[String],
    lifetime: Duration,
    relay_path: Option<&[String]>,
) -> Result<AttributeGuard> {
    let _trace = crate::app::timing::trace_scope("native.capture", "arm native attributes");
    let memory = ProcessMemory::open_with_access(pid, PROCESS_DUP_HANDLE)?;
    let modules = modules(pid)?;
    let studio = modules.first().context("No Studio module")?;
    anyhow::ensure!(
        studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"),
        "Attribute observation target is not Studio"
    );
    let window = capture_window(pid, title)?;
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    let mut model = active_data_model(pid, &memory, studio, layout.data, title)?;
    let prepared = prepared(&memory, studio, &model)?;
    for (rva, code) in prepared.code.iter() {
        anyhow::ensure!(
            memory.read_vec(studio.base + rva, code.len())? == *code,
            "Loaded Studio attribute signal code changed"
        );
    }
    let descriptor = find_class_member_descriptor(
        &memory,
        model.outer + model.layout.data_model_instance,
        model.layout,
        "Attributes",
    )?;
    let context = data_model_task_context(&memory, studio, &layout, &model)?;
    properties::verified_code(
        &memory,
        studio,
        &layout,
        studio.base + layout.submit_task,
        64,
    )?;
    let parent = properties::parent_offset(&memory, &model)?;
    let roots = model
        .roots
        .iter()
        .map(|entry| {
            Ok((
                *entry,
                read_instance_class(&memory, entry.instance, model.layout)
                    .context("Service class changed")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let relay = relay_path.map(|path| -> Result<_> {
        anyhow::ensure!(path.len() == 2 && path[0] == "CoreGui"
            && path[1].starts_with("ReniumAttributeJournal_"), "Invalid native attribute relay path");
        let ancestors = properties::resolve_path(&memory, &model, path, &[1, 1])?;
        let target = *ancestors.last().context("Native attribute relay is missing")?;
        anyhow::ensure!(read_instance_class(&memory, target.instance, model.layout).as_deref()
            == Some("ObjectValue"), "Native attribute relay changed class");
        let changed = find_class_member_descriptor(&memory, target.instance, model.layout, "Changed")?;
        anyhow::ensure!(read_rtti_type(&memory, changed, studio.base, studio.size).as_deref() == Some(
            ".?AV?$EventDesc@VObjectValue@RBX@@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@ZV?$signal@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@Z@rbx@@PEQ12@V56@@Reflection@RBX@@"
        ), "Native attribute relay signature changed");
        let vtable = memory.read_u64(changed)? as usize;
        let invoker = (memory.read_u64(vtable + 0x30)? as usize).checked_sub(studio.base)
            .context("Native attribute relay invoker is outside Studio")?;
        let offset = memory.read_u32(changed + 0x78)? as usize;
        anyhow::ensure!((8..0x1000).contains(&offset) && offset.is_multiple_of(8),
            "Native attribute relay member changed");
        let mapped = ImageView::open(&studio.path)?;
        let bytes = mapped.bytes();
        let image = PeImage::parse(bytes)?;
        let fire = signal::relay_dispatch(&image, invoker)?;
        for rva in [invoker, fire] {
            let offset = image.rva_to_offset(rva)?;
            let (start, end) = image.function_bounds(offset)?;
            anyhow::ensure!(start == offset && memory.read_vec(studio.base + rva, end - start)? == bytes[start..end],
                "Loaded native attribute relay code changed");
        }
        Ok((target, offset, studio.base + fire))
    }).transpose()?;
    model.roots = select_capture_roots(&roots, services)?;
    let (_, serializer, _) = studio_layout(&studio.path)?;
    let helper = ensure_helper_loaded(pid, &memory, &modules)?;
    let entry = helper + helper_export_rva("ReniumObserveAttributes")?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("Cannot create observation nonce: {error}"))?;
    let directory = std::env::temp_dir().join("renium-native");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "attributes-{pid}-{:032x}.tmp",
        u128::from_le_bytes(nonce)
    ));
    let transport = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(7)
        .custom_flags(0x04000000)
        .open(&path)?;
    let stop = event()?;
    let ready = event()?;
    let host = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, std::process::id()) };
    anyhow::ensure!(
        !host.is_null(),
        "Cannot observe native host lifetime: {}",
        io::Error::last_os_error()
    );
    let host = Handle(host);
    let mut duplicates = RemoteHandles {
        memory: &memory,
        values: Vec::new(),
    };
    let mut params = build_parameters(studio.base, serializer, &model, &path, true)?;
    params.resize(6016, 0);
    if let Some((target, offset, fire)) = relay {
        for (field, value) in [
            (5976, target.instance),
            (5984, target.owner),
            (5992, offset),
            (6000, fire),
        ] {
            put_u64(&mut params, field, value);
        }
    }
    for (offset, value) in [
        (PARAM_TASK_CONTEXT, context),
        (PARAM_SUBMIT_TASK, studio.base + layout.submit_task),
        (PARAM_WINDOW, window.0),
        (5904, prepared.trace.signal_offset),
        (5912, studio.base + prepared.trace.ensure),
        (5920, studio.base + prepared.trace.allocate),
        (5928, studio.base + prepared.trace.append),
        (5936, studio.base + prepared.trace.disconnect),
        (5944, studio.base + prepared.trace.assign),
        (5952, descriptor),
        (5960, duplicates.duplicate(&stop)?),
        (5968, duplicates.duplicate(&ready)?),
        (6008, duplicates.duplicate(&host)?),
    ] {
        put_u64(&mut params, offset, value);
    }
    put_u32(&mut params, PARAM_PROCESS_ID, pid);
    put_u32(
        &mut params,
        PARAM_TIMEOUT,
        u32::try_from(lifetime.as_millis())?,
    );
    put_u32(&mut params, PARAM_PARENT_OFFSET, u32::try_from(parent)?);
    let mut remote = memory.allocate(params.len())?;
    memory.write(remote.address, &params)?;
    anyhow::ensure!(
        capture_window(pid, title)? == window,
        "Studio window changed before attribute observation"
    );
    let thread = unsafe {
        CreateRemoteThread(
            memory.handle,
            null(),
            0,
            Some(transmute::<
                usize,
                unsafe extern "system" fn(*mut c_void) -> u32,
            >(entry)),
            remote.address as *const c_void,
            0,
            null_mut(),
        )
    };
    anyhow::ensure!(
        !thread.is_null(),
        "Cannot start attribute observation: {}",
        io::Error::last_os_error()
    );
    remote.address = 0;
    duplicates.values.clear(); // Worker owns both duplicates and its input.
    let mut guard = AttributeGuard {
        stop,
        thread: Some(Handle(thread)),
        transport,
    };
    anyhow::ensure!(
        unsafe { WaitForSingleObject(ready.0, 2500) } == WAIT_OBJECT_0,
        "Studio did not arm attribute observation before its deadline"
    );
    let header = guard.header()?;
    anyhow::ensure!(
        read_u32(&header, 8)? == 2,
        "{}",
        String::from_utf8_lossy(&header[64..]).trim_end_matches('\0')
    );
    Ok(guard)
}
