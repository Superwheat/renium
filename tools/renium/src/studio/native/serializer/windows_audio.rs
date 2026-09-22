use super::*;
use crate::studio::audio::Action;

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-audio.dll"));
const SIZE: usize = 304;

pub(crate) struct AudioOutputGate {
    process: ProcessMemory,
    entry: usize,
}

pub(crate) struct OutputState {
    pub(crate) focused: bool,
    pub(crate) muted: bool,
    pub(crate) hooks: u32,
    pub(crate) buffers: u64,
    pub(crate) silenced: u64,
}

impl AudioOutputGate {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        use windows_sys::Win32::Foundation::FreeLibrary;
        use windows_sys::Win32::System::LibraryLoader::{
            DONT_RESOLVE_DLL_REFERENCES, LoadLibraryExW,
        };
        let directory = crate::app::update::user_data_dir()?.join("native");
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("renium-audio-{:016x}.dll", fnv1a(HELPER)));
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
        anyhow::ensure!(!local.is_null(), "Could not read the Studio audio helper");
        let entry = unsafe { GetProcAddress(local, c"ReniumAudioStep".as_ptr().cast()) }
            .map(|address| address as usize - local as usize);
        unsafe {
            FreeLibrary(local);
        }
        let entry = entry.context("Studio audio helper has no entry point")?;
        let process = ProcessMemory::open(pid)?;
        let base = ensure_library_loaded(pid, &process, &modules(pid)?, 5_000, &path)?;
        Ok(Self {
            process,
            entry: base + entry,
        })
    }

    pub(crate) fn step(&self, action: Action, refresh: bool) -> Result<OutputState> {
        let mut bytes = [0; SIZE];
        put_u32(&mut bytes, 0, SIZE as u32);
        put_u32(&mut bytes, 4, 1);
        put_u32(
            &mut bytes,
            8,
            match action {
                Action::Status => 0,
                Action::Off => 1,
                Action::Mute => 2,
                Action::Unmute => 3,
                Action::Auto => 4,
            },
        );
        put_u32(&mut bytes, 12, u32::from(refresh));
        let mut remote = self.process.allocate(SIZE)?;
        self.process.write(remote.address, &bytes)?;
        let result = remote.run(self.entry, 3_000)?;
        anyhow::ensure!(
            result == 0,
            "Studio audio helper rejected its request ({result})"
        );
        self.process.read(remote.address, &mut bytes)?;
        anyhow::ensure!(
            read_u32(&bytes, 28)? == 1,
            "{}",
            String::from_utf8_lossy(&bytes[48..]).trim_end_matches('\0')
        );
        Ok(OutputState {
            focused: read_u32(&bytes, 16)? != 0,
            muted: read_u32(&bytes, 20)? != 0,
            hooks: read_u32(&bytes, 24)?,
            buffers: read_u64(&bytes, 32)?,
            silenced: read_u64(&bytes, 40)?,
        })
    }
}

impl Drop for AudioOutputGate {
    fn drop(&mut self) {
        let _ = self.step(Action::Off, false);
    }
}

#[test]
fn output_gate_preserves_forwarding_focus_and_crash_expiry() {
    assert!(
        std::process::Command::new(Path::new(env!("OUT_DIR")).join("renium-audio-test.exe"))
            .status()
            .expect("run audio output regression")
            .success()
    );
}
