#include "../renium_audio_windows.cpp"
#include <cassert>
#include <cmath>
#include <cstring>

DEFINE_GUID(IID_ISimpleAudioVolume, 0x87ce5498, 0x68d6, 0x44e5, 0x92,0x15,0x6d,0xa4,0x7e,0xf8,0x83,0xd8);

namespace legacy_gate {
struct Hook {
    const IAudioRenderClientVtbl* table;
    decltype(IAudioRenderClientVtbl::ReleaseBuffer) original;
    Hook* next;
};
std::atomic<Hook*> hooks{nullptr};
std::atomic<bool> active{false};
std::atomic<ULONGLONG> silenced{0};

HRESULT STDMETHODCALLTYPE ReleaseBuffer(IAudioRenderClient* client, UINT32 frames, DWORD flags) {
    auto* hook = hooks.load(std::memory_order_acquire);
    while (hook && hook->table != client->lpVtbl) hook = hook->next;
    if (!hook) return E_UNEXPECTED;
    if (active.load(std::memory_order_acquire)) {
        flags |= AUDCLNT_BUFFERFLAGS_SILENT;
        silenced.fetch_add(1, std::memory_order_relaxed);
    }
    return hook->original(client, frames, flags);
}

void Install(IAudioRenderClient* client) {
    auto* slot = const_cast<decltype(IAudioRenderClientVtbl::ReleaseBuffer)*>(&client->lpVtbl->ReleaseBuffer);
    auto original = *slot;
    if (original == ReleaseBuffer) return;
    DWORD prot = 0;
    assert(VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &prot));
    auto* hook = new Hook{client->lpVtbl, original, hooks.load(std::memory_order_relaxed)};
    hooks.store(hook, std::memory_order_release);
    assert(InterlockedCompareExchangePointer(reinterpret_cast<void* volatile*>(slot),
        reinterpret_cast<void*>(ReleaseBuffer), reinterpret_cast<void*>(original)) == original);
    DWORD unused = 0;
    VirtualProtect(slot, sizeof(*slot), prot, &unused);
}
}

namespace {
IAudioRenderClient* received = nullptr;
UINT32 frameCount = 0;
DWORD receivedFlags = 0;
float sampleBuffer[512]{};
BYTE* bufferToReturn = reinterpret_cast<BYTE*>(sampleBuffer);

HRESULT STDMETHODCALLTYPE Query(IAudioRenderClient*, REFIID, void**) { return E_NOINTERFACE; }
ULONG STDMETHODCALLTYPE Reference(IAudioRenderClient*) { return 1; }
HRESULT STDMETHODCALLTYPE Buffer(IAudioRenderClient*, UINT32 frames, BYTE** data) {
    if (frames > 256 || !data) return E_INVALIDARG;
    *data = bufferToReturn;
    return S_OK;
}
HRESULT STDMETHODCALLTYPE Original(IAudioRenderClient* client, UINT32 frames, DWORD flags) {
    received = client;
    frameCount = frames;
    receivedFlags = flags;
    return S_FALSE;
}

struct FakeClient {
    IAudioRenderClient rc;
    intptr_t ac_ptr;
};

struct FakeAudioClient {
    const void* vtbl;
    WAVEFORMATEX* wf;
};

struct LocalRing {
    std::wstring name;
    HANDLE mapping = nullptr;
    TapRingHeader* view = nullptr;
    LocalRing(const wchar_t* n, uint32_t capacity = 262144) : name(n) {
        mapping = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0,
            static_cast<DWORD>(sizeof(TapRingHeader) + capacity), name.c_str());
        assert(mapping != nullptr);
        view = static_cast<TapRingHeader*>(MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0));
        assert(view != nullptr);
        std::memset(view, 0, sizeof(TapRingHeader));
        view->magic = kTapMagic;
        view->version = kTapVersion;
        view->capacity = capacity;
        view->active.store(1);
    }
    void ReleaseLocalHandles() {
        if (view) { UnmapViewOfFile(view); view = nullptr; }
        if (mapping) { CloseHandle(mapping); mapping = nullptr; }
    }
    ~LocalRing() { ReleaseLocalHandles(); }
};

void CallTap(unsigned action, ULONGLONG tap_id, DWORD recorder_pid, const wchar_t* name,
             ULONGLONG vtbl = 0, ULONGLONG sys_get = 0, ULONGLONG sys_rel = 0) {
    TapParameters p{};
    p.size = sizeof(p);
    p.version = kTapVersion;
    p.action = action;
    p.recorder_pid = recorder_pid;
    p.tap_id = tap_id;
    p.render_vtbl = vtbl;
    p.system_get_buffer = sys_get;
    p.system_release_buffer = sys_rel;
    if (name) std::wcsncpy(p.mapping_name, name, 127);
    assert(ReniumAudioTap(&p) == 0);
    assert(p.status == 1);
}

void WriteToneFrames(IAudioClient* ac, IAudioRenderClient* rc, UINT32 buf_frames,
                     uint32_t rate, uint16_t ch, double& phase, int iterations) {
    const double step = 2.0 * 3.14159265358979323846 * 440.0 / rate;
    for (int it = 0; it < iterations; ++it) {
        UINT32 pad = 0;
        assert(SUCCEEDED(IAudioClient_GetCurrentPadding(ac, &pad)));
        const UINT32 avail = buf_frames > pad ? buf_frames - pad : 0;
        if (avail > 0) {
            BYTE* ptr = nullptr;
            assert(SUCCEEDED(IAudioRenderClient_GetBuffer(rc, avail, &ptr)));
            auto* out = reinterpret_cast<float*>(ptr);
            for (UINT32 f = 0; f < avail; ++f) {
                const float s = static_cast<float>(0.5 * std::sin(phase));
                phase = std::fmod(phase + step, 2.0 * 3.14159265358979323846);
                for (uint16_t c = 0; c < ch; ++c) out[f * ch + c] = s;
            }
            assert(SUCCEEDED(IAudioRenderClient_ReleaseBuffer(rc, avail, 0)));
        }
        Sleep(10);
    }
}
}

int main(int argc, char** argv) {
    if (argc > 1 && std::strcmp(argv[1], "--sleep") == 0) {
        Sleep(60000);
        return 0;
    }

    assert(!renium_audio::Muted(4, 5000, 1000, true));
    assert(renium_audio::Muted(4, 5000, 1000, false));
    assert(renium_audio::Muted(2, 5000, 1000, true));
    assert(!renium_audio::Muted(1, 5000, 1000, false));
    assert(!renium_audio::Muted(4, 5000, 5000, false));

    IAudioRenderClientVtbl table{};
    table.ReleaseBuffer = Original;
    FakeClient fake{{&table}, 0};
    IAudioRenderClient* client = &fake.rc;
    bool rejected = false;
    try { Probe(client); } catch (const std::runtime_error&) { rejected = true; }
    assert(rejected && GetState().count == 0);
    table.QueryInterface = Query;
    table.AddRef = Reference;
    table.Release = Reference;
    table.GetBuffer = Buffer;

    Install(client);
    Install(client);
    assert(GetState().count == 1);
    assert(table.GetBuffer == Buffer);
    assert(table.ReleaseBuffer == ReleaseBuffer);

    for (unsigned iteration = 0; iteration < 1000; ++iteration) {
        auto& state = GetState();
        state.deadline = GetTickCount64() + 3000;
        state.mode = 2;
        assert(IAudioRenderClient_ReleaseBuffer(client, 128, 0) == S_FALSE);
        assert(received == client && frameCount == 128 && receivedFlags == AUDCLNT_BUFFERFLAGS_SILENT);
        state.mode = 1;
        assert(IAudioRenderClient_ReleaseBuffer(client, 96, 0) == S_FALSE);
        assert(frameCount == 96 && receivedFlags == 0);
        state.mode = 2;
        state.deadline = 0;
        assert(IAudioRenderClient_ReleaseBuffer(client, 48, 0) == S_FALSE);
        assert(frameCount == 48 && receivedFlags == 0);
        assert(IAudioRenderClient_ReleaseBuffer(client, 0, AUDCLNT_BUFFERFLAGS_SILENT) == S_FALSE);
        assert(frameCount == 0 && receivedFlags == AUDCLNT_BUFFERFLAGS_SILENT);
    }
    assert(GetState().silenced == 1000);
    assert(GetState().calls == 4000);

    InstallTapRender(client, nullptr, nullptr);
    auto* rel_hook = FindHook(GetState().release_hooks, &table);
    assert(rel_hook && rel_hook->system_original.load() == nullptr);
    legacy_gate::active.store(true);
    legacy_gate::Install(client);
    rel_hook->original = legacy_gate::ReleaseBuffer;
    assert(IAudioRenderClient_ReleaseBuffer(client, 16, 0) == E_FAIL);

    Install(client, Original);
    assert(rel_hook->system_original.load() == Original);
    const ULONGLONG leg_before = legacy_gate::silenced.load();
    receivedFlags = 0;
    assert(IAudioRenderClient_ReleaseBuffer(client, 24, 0) == S_FALSE);
    assert(frameCount == 24 && (receivedFlags & AUDCLNT_BUFFERFLAGS_SILENT) != 0);
    assert(legacy_gate::silenced.load() > leg_before);

    {
        auto& state = GetState();
        LocalRing seh_ring(L"Local\\renium-audio-seh-test");
        state.taps[0].id = 999;
        state.taps[0].ring = seh_ring.view;
        state.taps[0].ref_state.store(1);
        state.active_taps.store(1);

        const int dummy_ac_vtbl = 0;
        WAVEFORMATEX wf{};
        wf.wFormatTag = WAVE_FORMAT_IEEE_FLOAT;
        wf.nChannels = 2;
        wf.nSamplesPerSec = 48000;
        wf.wBitsPerSample = 32;
        wf.nBlockAlign = 8;
        wf.nAvgBytesPerSec = 48000 * 8;
        FakeAudioClient fake_ac{&dummy_ac_vtbl, &wf};

        state.format_layout.rc_vtbl = &table;
        state.format_layout.ac_vtbl = &dummy_ac_vtbl;
        state.format_layout.rc_to_ac_offset = offsetof(FakeClient, ac_ptr);
        state.format_layout.rc_to_ac_delta = 0;
        state.format_layout.ac_to_wf_offset = offsetof(FakeAudioClient, wf);
        state.format_layout.wf_is_pointer = true;
        state.format_layout.valid.store(true);

        fake.ac_ptr = 0x100000;
        BYTE* ptr = nullptr;
        assert(IAudioRenderClient_GetBuffer(client, 4, &ptr) == S_OK);
        assert(IAudioRenderClient_ReleaseBuffer(client, 4, 0) == S_FALSE);
        assert(!state.format_layout.valid.load());
        assert(seh_ring.view->read_faults.load() == 1);
        assert(seh_ring.view->skipped_unknown.load() == 0);
        assert(seh_ring.view->active.load() == 0);
        assert(state.taps[0].ref_state.load() == 1);

        seh_ring.view->active.store(1);
        state.format_layout.valid.store(true);
        fake.ac_ptr = reinterpret_cast<intptr_t>(&fake_ac);
        bufferToReturn = reinterpret_cast<BYTE*>(0x100000);
        assert(IAudioRenderClient_GetBuffer(client, 4, &ptr) == S_OK);
        assert(IAudioRenderClient_ReleaseBuffer(client, 4, 0) == S_FALSE);
        assert(!state.format_layout.valid.load());
        assert(seh_ring.view->read_faults.load() == 2);
        assert(seh_ring.view->skipped_unknown.load() == 0);
        assert(seh_ring.view->active.load() == 0);
        assert(state.taps[0].ref_state.load() == 1);

        bufferToReturn = reinterpret_cast<BYTE*>(sampleBuffer);
        state.taps[0].ref_state.store(0);
        state.taps[0].ring = nullptr;
        state.taps[0].id = 0;
        state.active_taps.store(0);
    }

    if (SUCCEEDED(CoInitializeEx(nullptr, COINIT_MULTITHREADED))) {
        Com<IMMDeviceEnumerator> enumerator;
        Com<IMMDevice> device;
        if (SUCCEEDED(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                IID_IMMDeviceEnumerator, reinterpret_cast<void**>(&enumerator.value))) &&
            SUCCEEDED(IMMDeviceEnumerator_GetDefaultAudioEndpoint(enumerator.value, eRender, eConsole, &device.value))) {
            Com<IAudioClient> ac;
            assert(SUCCEEDED(IMMDevice_Activate(device.value, IID_IAudioClient, CLSCTX_ALL, nullptr,
                reinterpret_cast<void**>(&ac.value))));
            WAVEFORMATEX* mix = nullptr;
            assert(SUCCEEDED(IAudioClient_GetMixFormat(ac.value, &mix)));
            const uint32_t rate = mix->nSamplesPerSec;
            const uint16_t ch = mix->nChannels;
            const GUID sid = {0x6a9d2211, 0x3b44, 0x4120, {0x91,0x22,0x33,0x44,0x55,0x66,0x77,0x88}};
            assert(SUCCEEDED(IAudioClient_Initialize(ac.value, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_NOPERSIST, 1000000, 0, mix, &sid)));
            CoTaskMemFree(mix);

            Com<ISimpleAudioVolume> vol;
            assert(SUCCEEDED(IAudioClient_GetService(ac.value, IID_ISimpleAudioVolume,
                reinterpret_cast<void**>(&vol.value))));
            assert(SUCCEEDED(vol.value->lpVtbl->SetMute(vol.value, TRUE, nullptr)));
            BOOL muted_now = FALSE;
            assert(SUCCEEDED(vol.value->lpVtbl->GetMute(vol.value, &muted_now)) && muted_now == TRUE);

            Com<IAudioRenderClient> rc;
            assert(SUCCEEDED(IAudioClient_GetService(ac.value, IID_IAudioRenderClient,
                reinterpret_cast<void**>(&rc.value))));
            const auto real_vtbl = reinterpret_cast<ULONGLONG>(rc.value->lpVtbl);
            const auto real_get = reinterpret_cast<ULONGLONG>(rc.value->lpVtbl->GetBuffer);
            const auto real_rel = reinterpret_cast<ULONGLONG>(rc.value->lpVtbl->ReleaseBuffer);

            UINT32 buf_frames = 0;
            assert(SUCCEEDED(IAudioClient_GetBufferSize(ac.value, &buf_frames)));
            assert(SUCCEEDED(IAudioClient_Start(ac.value)));

            double phase = 0.0;
            WriteToneFrames(ac.value, rc.value, buf_frames, rate, ch, phase, 4);

            legacy_gate::active.store(true);
            legacy_gate::Install(rc.value);

            ParametersV2 gp{};
            gp.base.size = sizeof(gp);
            gp.base.version = 2;
            gp.base.action = 2;
            gp.base.refresh = 1;
            gp.render_vtbl = real_vtbl;
            gp.system_release_buffer = real_rel;
            assert(ReniumAudioStep(&gp) == 0 && gp.base.status == 1);
            auto* real_rel_hook = FindHook(GetState().release_hooks, rc.value->lpVtbl);
            assert(real_rel_hook && reinterpret_cast<ULONGLONG>(real_rel_hook->system_original.load()) == real_rel);

            const DWORD self_pid = GetCurrentProcessId();
            wchar_t n1[96]{}, n2[96]{}, n3[96]{};
            std::swprintf(n1, 96, L"Local\\renium-audio-unit-1-%lu", self_pid);
            std::swprintf(n2, 96, L"Local\\renium-audio-unit-2-%lu", self_pid);
            std::swprintf(n3, 96, L"Local\\renium-audio-unit-3-%lu", self_pid);

            LocalRing ring1(n1);
            CallTap(1, 101, self_pid, n1, real_vtbl, real_get, real_rel);

            legacy_gate::Install(rc.value);
            const ULONGLONG leg_sil_before = legacy_gate::silenced.load();
            WriteToneFrames(ac.value, rc.value, buf_frames, rate, ch, phase, 12);
            assert(legacy_gate::silenced.load() > leg_sil_before);
            assert(ring1.view->skipped_unknown.load() == 0);
            assert(ring1.view->write_pos.load() > sizeof(TapPacketHeader));
            const auto* first_pkt = reinterpret_cast<const TapPacketHeader*>(ring1.view + 1);
            assert(first_pkt->ready == 1 && first_pkt->sample_rate == rate && first_pkt->channels == ch);
            assert(first_pkt->payload_bytes > 0);
            bool any_nonzero = false;
            const auto* pcm = reinterpret_cast<const float*>(first_pkt + 1);
            for (uint32_t i = 0; i < first_pkt->payload_bytes / sizeof(float); ++i) {
                if (std::fabs(pcm[i]) > 0.05f) { any_nonzero = true; break; }
            }
            assert(any_nonzero);

            LocalRing ring2(n2);
            CallTap(1, 102, self_pid, n2, real_vtbl, real_get, real_rel);
            const uint64_t r1_mid = ring1.view->write_pos.load();
            WriteToneFrames(ac.value, rc.value, buf_frames, rate, ch, phase, 8);
            assert(ring1.view->write_pos.load() > r1_mid);
            assert(ring2.view->write_pos.load() > sizeof(TapPacketHeader));
            CallTap(2, 101, self_pid, nullptr);
            const uint64_t r1_stopped = ring1.view->write_pos.load();
            const uint64_t r2_before = ring2.view->write_pos.load();
            WriteToneFrames(ac.value, rc.value, buf_frames, rate, ch, phase, 8);
            assert(ring1.view->write_pos.load() == r1_stopped);
            assert(ring2.view->write_pos.load() > r2_before);
            CallTap(2, 102, self_pid, nullptr);

            wchar_t self_exe[MAX_PATH]{};
            assert(GetModuleFileNameW(nullptr, self_exe, MAX_PATH) > 0);
            std::wstring cmd = std::wstring(L"\"") + self_exe + L"\" --sleep";
            STARTUPINFOW si{};
            si.cb = sizeof(si);
            PROCESS_INFORMATION pi{};
            assert(CreateProcessW(nullptr, cmd.data(), nullptr, nullptr, FALSE,
                CREATE_NO_WINDOW, nullptr, nullptr, &si, &pi));
            {
                LocalRing ring3(n3);
                CallTap(1, 103, pi.dwProcessId, n3, real_vtbl, real_get, real_rel);
                ring3.ReleaseLocalHandles();
            }
            HANDLE held = OpenFileMappingW(FILE_MAP_READ, FALSE, n3);
            assert(held != nullptr);
            CloseHandle(held);
            assert(TerminateProcess(pi.hProcess, 1));
            WaitForSingleObject(pi.hProcess, 5000);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            bool reaped = false;
            const ULONGLONG deadline = GetTickCount64() + 3000;
            while (GetTickCount64() < deadline) {
                HANDLE probe = OpenFileMappingW(FILE_MAP_READ, FALSE, n3);
                if (!probe) { reaped = true; break; }
                CloseHandle(probe);
                Sleep(50);
            }
            assert(reaped);

            assert(SUCCEEDED(vol.value->lpVtbl->GetMute(vol.value, &muted_now)) && muted_now == TRUE);
            IAudioClient_Stop(ac.value);
        }
        CoUninitialize();
    }
    return 0;
}
