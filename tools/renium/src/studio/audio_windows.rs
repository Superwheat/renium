use std::collections::{HashMap, HashSet};

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

use super::{Action, MuteOwnership, Status};

const CONTEXT: GUID = GUID::from_u128(0x2b86eb38_3951_4f73_81ec_426d72b85a6b);

struct Session {
    control: IAudioSessionControl2,
    volume: ISimpleAudioVolume,
    ownership: MuteOwnership,
}

pub(super) struct Backend {
    pid: u32,
    process: windows_sys::Win32::Foundation::HANDLE,
    sessions: HashMap<String, Session>,
}

impl Backend {
    pub(super) fn is_running(&self) -> bool {
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(self.process, 0)
                == windows_sys::Win32::Foundation::WAIT_TIMEOUT
        }
    }

    pub(super) fn new(pid: u32) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
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
                    let id = control.GetSessionIdentifier()?;
                    let key = id.to_string();
                    CoTaskMemFree(Some(id.0.cast()));
                    let key = key.context("Invalid audio session identity")?;
                    found.insert(key.clone());
                    let volume = control.cast()?;
                    let previous = self.sessions.remove(&key);
                    let ownership =
                        previous.map_or_else(MuteOwnership::default, |session| session.ownership);
                    self.sessions.insert(
                        key,
                        Session {
                            control,
                            volume,
                            ownership,
                        },
                    );
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
        let mute = action == Action::Mute || action == Action::Auto && !focused;
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
        let mut retire = Vec::new();
        for (key, session) in &mut self.sessions {
            let result = unsafe {
                (|| -> Result<()> {
                    let expired = session.control.GetState()? == AudioSessionStateExpired;
                    let current = session.volume.GetMute()?.as_bool();
                    let wanted = mute && !expired;
                    let desired =
                        session
                            .ownership
                            .desired(current, wanted, action == Action::Unmute);
                    if let Some(value) = desired {
                        session.volume.SetMute(value, &CONTEXT)?;
                        anyhow::ensure!(
                            session.volume.GetMute()?.as_bool() == value,
                            "Audio session did not retain mute state"
                        );
                    }
                    session.ownership.accepted(wanted, desired == Some(true));
                    if !expired && found.contains(key) {
                        status.sessions += 1;
                        status.muted_sessions += usize::from(desired.unwrap_or(current));
                    } else if !session.ownership.changed {
                        retire.push(key.clone());
                    }
                    Ok(())
                })()
            };
            if let Err(error) = result {
                status.error = Some(format!(
                    "Audio session unavailable; control will retry: {error:#}"
                ));
                if !session.ownership.changed {
                    retire.push(key.clone());
                }
            }
            status.pending_restores += usize::from(session.ownership.changed);
        }
        for key in retire {
            self.sessions.remove(&key);
        }
        Ok(status)
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        for session in self.sessions.values_mut() {
            if session.ownership.changed {
                unsafe {
                    let _ = session.volume.SetMute(false, &CONTEXT);
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
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_NOPERSIST,
        IAudioClient, IAudioRenderClient, eConsole,
    };

    unsafe fn silent_session(id: u128) -> Result<(IAudioClient, ISimpleAudioVolume)> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let initialized = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_NOPERSIST,
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
    #[ignore = "requires a real Windows audio output; creates only silent sessions in the test process"]
    fn audio_real_sessions_restore_focus_and_late_sessions() -> Result<()> {
        let mut backend = Backend::new(std::process::id())?;
        unsafe {
            let (first, first_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f101)?;
            let (second, second_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f102)?;
            first_volume.SetMute(false, &CONTEXT)?;
            first_volume.SetMasterVolume(0.23, &CONTEXT)?;
            second_volume.SetMute(true, &CONTEXT)?;
            for _ in 0..4 {
                let status = backend.step_with_focus(Action::Auto, false)?;
                assert!(status.error.is_none(), "{:?}", status.error);
                assert!(first_volume.GetMute()?.as_bool());
                backend.step_with_focus(Action::Auto, true)?;
                assert!(!first_volume.GetMute()?.as_bool());
                assert!(second_volume.GetMute()?.as_bool());
            }
            backend.step_with_focus(Action::Mute, false)?;
            let (late, late_volume) = silent_session(0x464e88f9_54c4_48aa_82f6_d5566c29f103)?;
            late_volume.SetMute(false, &CONTEXT)?;
            backend.step_with_focus(Action::Mute, false)?;
            assert!(late_volume.GetMute()?.as_bool());
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
        Ok(())
    }
}
