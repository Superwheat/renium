use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::core::{GUID, Interface};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::{Action, Status};
use crate::studio::native::serializer::AudioOutputGate;
use crate::system::files::atomic_write_file;

const CONTEXT: GUID = GUID::from_u128(0x2b86eb38_3951_4f73_81ec_426d72b85a6b);

struct Session {
    control: IAudioSessionControl2,
    volume: ISimpleAudioVolume,
}

struct RestoreJournal {
    path: PathBuf,
    owned: BTreeSet<String>,
}

impl RestoreJournal {
    fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("mute-ownership.json");
        let owned = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("Could not read Studio audio restoration state")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => {
                return Err(error).context("Could not read Studio audio restoration state");
            }
        };
        Ok(Self { path, owned })
    }

    fn set(&mut self, key: &str, owned: bool) -> Result<()> {
        if self.owned.contains(key) == owned {
            return Ok(());
        }
        let mut updated = self.owned.clone();
        if owned {
            updated.insert(key.to_owned());
        } else {
            updated.remove(key);
        }
        atomic_write_file(&self.path, &serde_json::to_vec(&updated)?)
            .context("Could not save Studio audio restoration state")?;
        self.owned = updated;
        Ok(())
    }
}

pub(super) struct Backend {
    pid: u32,
    process: windows_sys::Win32::Foundation::HANDLE,
    sessions: HashMap<String, Session>,
    journal: RestoreJournal,
    gate: Option<AudioOutputGate>,
    gate_sessions: HashSet<String>,
}

impl Backend {
    pub(super) fn is_running(&self) -> bool {
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(self.process, 0)
                == windows_sys::Win32::Foundation::WAIT_TIMEOUT
        }
    }

    pub(super) fn new(pid: u32, dir: &Path) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        let journal = RestoreJournal::open(dir)?;
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        anyhow::ensure!(
            !process.is_null(),
            "Could not retain Studio process identity: {}",
            std::io::Error::last_os_error()
        );
        unsafe {
            if let Err(error) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                windows_sys::Win32::Foundation::CloseHandle(process);
                return Err(error.into());
            }
        }
        Ok(Self {
            pid,
            process,
            sessions: HashMap::new(),
            journal,
            gate: None,
            gate_sessions: HashSet::new(),
        })
    }

    fn discover(&mut self) -> Result<HashSet<String>> {
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut found = HashSet::new();
            for index in 0..devices.GetCount()? {
                let device = devices.Item(index)?;
                let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                let sessions = manager.GetSessionEnumerator()?;
                for index in 0..sessions.GetCount()? {
                    let control: IAudioSessionControl2 = sessions.GetSession(index)?.cast()?;
                    let mut pid = 0;
                    let result = (Interface::vtable(&control).GetProcessId)(
                        Interface::as_raw(&control),
                        &mut pid,
                    );
                    result.ok()?;
                    if pid != self.pid {
                        continue;
                    }
                    if result.0 != 0 {
                        bail!(
                            "Studio shares an audio session with another process; refusing to mute unrelated audio"
                        );
                    }
                    if control.GetState()? == AudioSessionStateExpired {
                        continue;
                    }
                    let id = control.GetSessionInstanceIdentifier()?;
                    let key = id.to_string();
                    CoTaskMemFree(Some(id.0.cast()));
                    let key = key.context("Invalid audio session identity")?;
                    found.insert(key.clone());
                    let volume = control.cast()?;
                    self.sessions.insert(key, Session { control, volume });
                }
            }
            Ok(found)
        }
    }

    pub(super) fn step(&mut self, action: Action) -> Result<Status> {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        }
        let focused = pid == self.pid;
        self.step_with_focus(action, focused)
    }

    fn step_with_focus(&mut self, action: Action, focused: bool) -> Result<Status> {
        let mut status = Status {
            focused,
            ..Status::default()
        };
        let found = match self.discover() {
            Ok(found) => found,
            Err(error) => {
                status.error = Some(format!("Could not enumerate output sessions: {error:#}"));
                HashSet::new()
            }
        };
        if self.gate.is_none() && matches!(action, Action::Mute | Action::Auto) {
            self.gate = Some(AudioOutputGate::new(self.pid)?);
        }
        let mut suppressed = false;
        if let Some(gate) = &self.gate {
            let output = gate.step(action, self.gate_sessions != found)?;
            self.gate_sessions.clone_from(&found);
            anyhow::ensure!(
                output.hooks != 0 || found.is_empty(),
                "No supported Studio output stream was found"
            );
            status.focused = output.focused;
            status.output_buffers = output.buffers;
            status.suppressed_buffers = output.silenced;
            suppressed = output.muted;
        }
        let mut retire = Vec::new();
        for (key, session) in &mut self.sessions {
            let result = unsafe {
                (|| -> Result<()> {
                    let expired = session.control.GetState()? == AudioSessionStateExpired;
                    let current = session.volume.GetMute()?.as_bool();
                    let unmute = action == Action::Unmute
                        || action == Action::Auto && focused
                        || self.journal.owned.contains(key);
                    if current && unmute {
                        session.volume.SetMute(false, &CONTEXT)?;
                        anyhow::ensure!(
                            !session.volume.GetMute()?.as_bool(),
                            "Audio session did not clear its previous Windows mute"
                        );
                    }
                    self.journal.set(key, false)?;
                    if !expired && found.contains(key) {
                        status.sessions += 1;
                        status.muted_sessions += usize::from(suppressed || current && !unmute);
                    } else {
                        retire.push(key.clone());
                    }
                    Ok(())
                })()
            };
            if let Err(error) = result {
                status.error = Some(format!(
                    "Audio session unavailable; control will retry: {error:#}"
                ));
                if !self.journal.owned.contains(key) {
                    retire.push(key.clone());
                }
            }
        }
        status.pending_restores = self.journal.owned.len();
        for key in retire {
            self.sessions.remove(&key);
        }
        Ok(status)
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        for (key, session) in &self.sessions {
            if self.journal.owned.contains(key) {
                unsafe {
                    if session.volume.SetMute(false, &CONTEXT).is_ok()
                        && session.volume.GetMute().is_ok_and(|mute| !mute.as_bool())
                    {
                        let _ = self.journal.set(key, false);
                    }
                }
            }
        }
        self.sessions.clear();
        unsafe {
            CoUninitialize();
            windows_sys::Win32::Foundation::CloseHandle(self.process);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::temp_dir;
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_NOPERSIST,
        IAudioClient, IAudioRenderClient, eConsole,
    };

    unsafe fn silent_session(id: u128) -> Result<(IAudioClient, ISimpleAudioVolume)> {
        unsafe { silent_session_with_flags(id, AUDCLNT_STREAMFLAGS_NOPERSIST) }
    }

    unsafe fn silent_session_with_flags(
        id: u128,
        flags: u32,
    ) -> Result<(IAudioClient, ISimpleAudioVolume)> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let initialized = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                1_000_000,
                0,
                format,
                Some(&GUID::from_u128(id)),
            );
            CoTaskMemFree(Some(format.cast()));
            initialized?;
            let render: IAudioRenderClient = client.GetService()?;
            let size = client.GetBufferSize()?;
            render.GetBuffer(size)?;
            render.ReleaseBuffer(size, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            let volume: ISimpleAudioVolume = client.GetService()?;
            client.Start()?;
            Ok((client, volume))
        }
    }

    #[test]
    #[ignore = "child process for the real Windows audio lifecycle regression"]
    fn audio_persistent_session_child() -> Result<()> {
        let Ok(id) = std::env::var("RENIUM_AUDIO_TEST_SESSION") else {
            return Ok(());
        };
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let (client, volume) = silent_session_with_flags(id.parse()?, 0)?;
            if std::env::var_os("RENIUM_AUDIO_TEST_RESET").is_some() {
                volume.SetMute(false, &CONTEXT)?;
                volume.SetMasterVolume(0.23, &CONTEXT)?;
            }
            println!("AUDIO-READY {}", volume.GetMute()?.as_bool());
            loop {
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line)? == 0 || line.trim() == "stop" {
                    break;
                }
                if line.trim() == "legacy-mute" {
                    volume.SetMute(true, &CONTEXT)?;
                }
                let render: IAudioRenderClient = client.GetService()?;
                let frames = client.GetBufferSize()? - client.GetCurrentPadding()?;
                let format = client.GetMixFormat()?;
                let stride = (*format).nBlockAlign as usize;
                CoTaskMemFree(Some(format.cast()));
                let buffer = render.GetBuffer(frames)?;
                if frames != 0 {
                    std::ptr::write_bytes(buffer, 0, frames as usize * stride);
                }
                render.ReleaseBuffer(frames, 0)?;
                println!(
                    "AUDIO-TICK {} {}",
                    volume.GetMute()?.as_bool(),
                    volume.GetMasterVolume()?
                );
            }
            client.Stop()?;
            drop(volume);
            drop(client);
            CoUninitialize();
        }
        Ok(())
    }

    struct AudioProcess {
        child: std::process::Child,
        output: std::io::BufReader<std::process::ChildStdout>,
    }

    impl AudioProcess {
        fn start(session: u128, reset: bool) -> Result<(Self, bool)> {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args([
                    "--exact",
                    "studio::audio::platform::tests::audio_persistent_session_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("RENIUM_AUDIO_TEST_SESSION", session.to_string())
                .env_remove("RENIUM_AUDIO_TEST_RESET")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if reset {
                command.env("RENIUM_AUDIO_TEST_RESET", "1");
            }
            let mut child = command.spawn()?;
            let output = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut process = Self { child, output };
            let mut line = String::new();
            while process.output.read_line(&mut line)? != 0 {
                if let Some(muted) = line.trim().strip_prefix("AUDIO-READY ") {
                    let muted = muted.parse()?;
                    return Ok((process, muted));
                }
                line.clear();
            }
            bail!("Audio test child exited before creating its session")
        }

        fn finish(&mut self) -> Result<()> {
            use std::io::Write;
            self.child.stdin.take().unwrap().write_all(b"stop\n")?;
            anyhow::ensure!(self.child.wait()?.success(), "Audio test child failed");
            Ok(())
        }

        fn tick(&mut self, command: &str) -> Result<(bool, f32)> {
            use std::io::{BufRead, Write};
            writeln!(self.child.stdin.as_mut().unwrap(), "{command}")?;
            let mut line = String::new();
            self.output.read_line(&mut line)?;
            let values = line
                .trim()
                .strip_prefix("AUDIO-TICK ")
                .context("Missing audio tick")?;
            let (muted, volume) = values.split_once(' ').context("Invalid audio tick")?;
            Ok((muted.parse()?, volume.parse()?))
        }
    }

    impl Drop for AudioProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    #[ignore = "requires a real Windows audio output; creates silent sessions in isolated test processes"]
    fn audio_real_sessions_do_not_inherit_background_mute_after_reopen() -> Result<()> {
        let dir = temp_dir("audio-process-reopen");
        let session = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let (mut first, initially_muted) = AudioProcess::start(session, true)?;
        assert!(!initially_muted);
        let mut backend = Backend::new(first.child.id(), &dir.join("first"))?;
        let muted = backend.step(Action::Mute)?;
        assert!(muted.error.is_none(), "{:?}", muted.error);
        assert_eq!(muted.muted_sessions, 1);
        assert_eq!(first.tick("tick")?, (false, 0.23));
        first.finish()?;
        backend.sessions.clear();
        drop(backend);
        let (mut second, inherited_mute) = AudioProcess::start(session, false)?;
        assert!(
            !inherited_mute,
            "The process-local mute leaked into a reopened Windows session"
        );
        let mut reopened = Backend::new(second.child.id(), &dir.join("second"))?;
        let focused = reopened.step_with_focus(Action::Auto, true)?;
        reopened.step_with_focus(Action::Unmute, true)?;
        second.finish()?;
        drop(reopened);
        fs::remove_dir_all(dir)?;
        assert!(focused.error.is_none(), "{:?}", focused.error);
        Ok(())
    }

    #[test]
    #[ignore = "requires a real Windows output; uses isolated silent processes without moving focus"]
    fn audio_real_output_gate_preserves_mixer_and_recovers_without_controller() -> Result<()> {
        let dir = temp_dir("audio-output-gate");
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let (mut child, _) = AudioProcess::start(id, true)?;
        let (mut unrelated, _) = AudioProcess::start(id + 1, true)?;
        let mut backend = Backend::new(child.child.id(), &dir)?;
        for _ in 0..8 {
            let before = backend.step(Action::Mute)?;
            assert!(before.error.is_none(), "{:?}", before.error);
            assert_eq!(child.tick("tick")?, (false, 0.23));
            let after = backend.step(Action::Mute)?;
            assert!(after.output_buffers > before.output_buffers);
            assert!(after.suppressed_buffers > before.suppressed_buffers);
            assert_eq!(after.pending_restores, 0);
            let off = backend.step(Action::Off)?;
            assert_eq!(child.tick("tick")?, (false, 0.23));
            let after = backend.step(Action::Off)?;
            assert!(after.output_buffers > off.output_buffers);
            assert_eq!(after.suppressed_buffers, off.suppressed_buffers);
            assert_eq!(after.muted_sessions, 0);
        }
        let muted = backend.step(Action::Mute)?;
        std::thread::sleep(std::time::Duration::from_millis(3200));
        assert_eq!(child.tick("tick")?, (false, 0.23));
        let expired = backend.step(Action::Status)?;
        assert_eq!(expired.muted_sessions, 0);
        assert!(expired.output_buffers > muted.output_buffers);
        assert_eq!(expired.suppressed_buffers, muted.suppressed_buffers);
        assert_eq!(unrelated.tick("tick")?, (false, 0.23));
        assert_eq!(child.tick("legacy-mute")?, (true, 0.23));
        let focused = backend.step_with_focus(Action::Auto, true)?;
        assert!(focused.error.is_none(), "{:?}", focused.error);
        assert_eq!(child.tick("tick")?, (false, 0.23));
        backend.step(Action::Off)?;
        drop(backend);
        child.finish()?;
        unrelated.finish()?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a real Windows audio output; creates only silent sessions in the test process"]
    fn audio_real_sessions_restore_after_worker_restart() -> Result<()> {
        let dir = temp_dir("audio-worker-restart");
        let mut backend = Backend::new(std::process::id(), &dir)?;
        unsafe {
            let (client, volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f201)?;
            let (manual, manual_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f202)?;
            volume.SetMute(false, &CONTEXT)?;
            volume.SetMasterVolume(0.23, &CONTEXT)?;
            manual_volume.SetMute(true, &CONTEXT)?;
            backend.discover()?;
            let key = backend
                .sessions
                .iter()
                .find(|(_, session)| {
                    session
                        .volume
                        .GetMasterVolume()
                        .is_ok_and(|level| (level - 0.23).abs() < 0.0001)
                })
                .unwrap()
                .0
                .clone();
            backend.journal.set(&key, true)?;
            volume.SetMute(true, &CONTEXT)?;
            assert!(volume.GetMute()?.as_bool());
            let mut restarted = Backend::new(std::process::id(), &dir)?;
            backend.sessions.clear();
            drop(backend);
            restarted.step(Action::Off)?;
            let still_muted = volume.GetMute()?.as_bool();
            volume.SetMute(false, &CONTEXT)?;
            assert!(
                !still_muted,
                "Renium lost mute ownership after its worker restarted"
            );
            assert!(manual_volume.GetMute()?.as_bool());
            assert!((volume.GetMasterVolume()? - 0.23).abs() < 0.0001);
            client.Stop()?;
            manual.Stop()?;
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a real Windows audio output; creates only silent sessions in the test process"]
    fn audio_real_sessions_restore_focus_and_late_sessions() -> Result<()> {
        let dir = temp_dir("audio-sessions");
        let mut backend = Backend::new(std::process::id(), &dir)?;
        unsafe {
            let (first, first_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f101)?;
            let (second, second_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f102)?;
            first_volume.SetMute(false, &CONTEXT)?;
            first_volume.SetMasterVolume(0.23, &CONTEXT)?;
            second_volume.SetMute(true, &CONTEXT)?;
            for _ in 0..4 {
                let status = backend.step_with_focus(Action::Auto, false)?;
                assert!(status.error.is_none(), "{:?}", status.error);
                assert!(!first_volume.GetMute()?.as_bool());
                backend.step_with_focus(Action::Auto, true)?;
                assert!(!first_volume.GetMute()?.as_bool());
                assert!(!second_volume.GetMute()?.as_bool());
            }
            second_volume.SetMute(true, &CONTEXT)?;
            backend.step_with_focus(Action::Mute, false)?;
            let (late, late_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f103)?;
            late_volume.SetMute(false, &CONTEXT)?;
            backend.step_with_focus(Action::Mute, false)?;
            assert!(!late_volume.GetMute()?.as_bool());
            backend.step_with_focus(Action::Off, false)?;
            assert!(!first_volume.GetMute()?.as_bool());
            assert!(!late_volume.GetMute()?.as_bool());
            assert!(second_volume.GetMute()?.as_bool());
            assert!((first_volume.GetMasterVolume()? - 0.23).abs() < 0.0001);
            backend.step_with_focus(Action::Unmute, false)?;
            assert!(!second_volume.GetMute()?.as_bool());
            backend.step_with_focus(Action::Mute, false)?;
            drop(late);
            backend.step_with_focus(Action::Off, false)?;
            first.Stop()?;
            second.Stop()?;
        }
        drop(backend);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn audio_ownership_journal_is_durable_and_does_not_adopt_other_sessions() -> Result<()> {
        let dir = temp_dir("audio-ownership");
        let mut journal = RestoreJournal::open(&dir)?;
        journal.set("session-instance-a", true)?;
        let mut restarted = RestoreJournal::open(&dir)?;
        assert!(restarted.owned.contains("session-instance-a"));
        assert!(!restarted.owned.contains("session-instance-b"));
        restarted.set("session-instance-a", false)?;
        assert!(RestoreJournal::open(&dir)?.owned.is_empty());
        fs::write(dir.join("mute-ownership.json"), "invalid")?;
        assert!(RestoreJournal::open(&dir).is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn audio_ownership_is_not_accepted_when_recording_fails() -> Result<()> {
        let dir = temp_dir("audio-ownership-failure");
        let mut journal = RestoreJournal::open(&dir)?;
        let path = journal.path.clone();
        let blocker = dir.join("blocked");
        fs::write(&blocker, "not a directory")?;
        journal.path = blocker.join("mute-ownership.json");
        assert!(journal.set("session", true).is_err());
        assert!(journal.owned.is_empty());
        journal.path = path;
        journal.set("session", true)?;
        journal.path = blocker.join("mute-ownership.json");
        assert!(journal.set("session", false).is_err());
        assert!(journal.owned.contains("session"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
