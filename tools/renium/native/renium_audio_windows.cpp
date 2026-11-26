#define CINTERFACE
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <mmreg.h>
#include <ksmedia.h>
#include <initguid.h>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <stdexcept>
#include <string>
#include "renium_audio_policy.h"

DEFINE_GUID(IID_IAudioClient, 0x1cb9ad4c, 0xdbfa, 0x4c32, 0xb1,0x78,0xc2,0xf5,0x68,0xa7,0x03,0xb2);
DEFINE_GUID(IID_IAudioRenderClient, 0xf294acfc, 0x3146, 0x4483, 0xa7,0xbf,0xad,0xdc,0xa7,0xc2,0x60,0xe2);
DEFINE_GUID(IID_IMMDeviceEnumerator, 0xa95664d2, 0x9614, 0x4f35, 0xa7,0x46,0xde,0x8d,0xb6,0x36,0x17,0xe6);

namespace {
constexpr uint32_t kTapMagic = 0x52415544u;
constexpr uint32_t kTapVersion = 4u;
constexpr uint8_t kFormatI16 = 1u, kFormatI24 = 2u, kFormatI32 = 3u, kFormatF32 = 4u;
constexpr uint8_t kPacketSilent = 1u;
constexpr std::size_t kTableCapacity = 256, kMaxTaps = 8;
constexpr ULONGLONG kSlotRecycleMs = 10000;

struct TapRingHeader {
    uint32_t magic, version, capacity;
    std::atomic<uint32_t> active;
    std::atomic<uint64_t> write_pos, read_pos, dropped, skipped_unknown, read_faults;
    uint64_t reserved;
};
static_assert(sizeof(TapRingHeader) == 64);

struct TapPacketHeader {
    uint32_t packet_bytes, payload_bytes;
    int64_t qpc_ticks;
    uint64_t stream_id;
    uint32_t sample_rate, frames;
    uint16_t channels;
    uint8_t format, flags;
    uint32_t ready;
};
static_assert(sizeof(TapPacketHeader) == 40);
static_assert(offsetof(TapPacketHeader, ready) == 36);

struct AudioFormat {
    uint32_t sample_rate = 0;
    uint16_t channels = 0, block_align = 0;
    uint8_t format = 0;
};

template<class T> struct Com {
    T* value = nullptr;
    ~Com() { if (value) value->lpVtbl->Release(value); }
};

template<class Vtbl, class Fn>
struct SlotHook {
    const Vtbl* table;
    Fn original;
    std::atomic<Fn> system_original;
    SlotHook* next;
};
using ReleaseHook = SlotHook<IAudioRenderClientVtbl, decltype(IAudioRenderClientVtbl::ReleaseBuffer)>;
using GetBufferHook = SlotHook<IAudioRenderClientVtbl, decltype(IAudioRenderClientVtbl::GetBuffer)>;

struct PendingSlot {
    std::atomic<uintptr_t> client{0};
    std::atomic<BYTE*> data{nullptr};
    std::atomic<UINT32> requested_frames{0};
    std::atomic<ULONGLONG> last_used{0};
};

struct StreamFormatLayout {
    std::atomic<bool> valid{false};
    const void* rc_vtbl = nullptr;
    const void* ac_vtbl = nullptr;
    std::size_t rc_to_ac_offset = 0, ac_to_wf_offset = 0;
    intptr_t rc_to_ac_delta = 0;
    bool wf_is_pointer = false;
};

struct TapSlot {
    std::atomic<uint64_t> ref_state{0};
    uint64_t id = 0;
    HANDLE mapping = nullptr, owner_process = nullptr;
    TapRingHeader* ring = nullptr;
};

struct State {
    std::mutex mutex;
    std::atomic<ReleaseHook*> release_hooks{nullptr};
    std::atomic<GetBufferHook*> get_hooks{nullptr};
    std::atomic<unsigned> mode{0};
    std::atomic<ULONGLONG> deadline{0}, calls{0}, silenced{0};
    std::atomic<uint32_t> active_taps{0};
    StreamFormatLayout format_layout{};
    PendingSlot pending_buffers[kTableCapacity]{};
    TapSlot taps[kMaxTaps]{};
    HANDLE watcher_thread = nullptr, watcher_wake = nullptr;
    bool tap_hooks_installed = false;
    unsigned count = 0;
};

State& GetState() {
    static auto* state = new State;
    return *state;
}

template<class HookList, class Vtbl>
HookList* FindHook(const std::atomic<HookList*>& head, const Vtbl* table) {
    auto* hook = head.load(std::memory_order_acquire);
    while (hook && hook->table != table) hook = hook->next;
    return hook;
}

AudioFormat ParseWaveFormat(const WAVEFORMATEX* wf) {
    AudioFormat out{};
    if (!wf || wf->nSamplesPerSec < 8000 || wf->nSamplesPerSec > 384000 ||
        wf->nChannels == 0 || wf->nChannels > 16 || wf->nBlockAlign == 0 ||
        wf->nBlockAlign % wf->nChannels != 0) return out;
    uint16_t tag = wf->wFormatTag;
    if (tag == WAVE_FORMAT_EXTENSIBLE) {
        if (wf->cbSize < 22) return out;
        const auto* ext = reinterpret_cast<const WAVEFORMATEXTENSIBLE*>(wf);
        if (IsEqualGUID(ext->SubFormat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)) tag = WAVE_FORMAT_IEEE_FLOAT;
        else if (IsEqualGUID(ext->SubFormat, KSDATAFORMAT_SUBTYPE_PCM)) tag = WAVE_FORMAT_PCM;
        else return out;
    }
    const uint16_t bps = wf->nBlockAlign / wf->nChannels;
    if (tag == WAVE_FORMAT_IEEE_FLOAT && bps == 4) out.format = kFormatF32;
    else if (tag == WAVE_FORMAT_PCM && bps == 2) out.format = kFormatI16;
    else if (tag == WAVE_FORMAT_PCM && bps == 3) out.format = kFormatI24;
    else if (tag == WAVE_FORMAT_PCM && bps == 4) out.format = kFormatI32;
    else return out;
    out.sample_rate = wf->nSamplesPerSec;
    out.channels = wf->nChannels;
    out.block_align = wf->nBlockAlign;
    return out;
}

std::size_t HashPtr(uintptr_t key) {
    key = (key ^ (key >> 33)) * 0xff51afd7ed558ccdULL;
    return static_cast<std::size_t>(key ^ (key >> 33)) & (kTableCapacity - 1);
}

void StorePendingBuffer(PendingSlot* table, uintptr_t client, BYTE* data, UINT32 frames) {
    if (!client) return;
    const ULONGLONG now = GetTickCount64();
    const std::size_t start = HashPtr(client);
    for (int pass = 0; pass < 2; ++pass) {
        for (std::size_t probe = 0; probe < kTableCapacity; ++probe) {
            auto& slot = table[(start + probe) & (kTableCapacity - 1)];
            uintptr_t cur = slot.client.load(std::memory_order_acquire);
            const bool recycle = pass == 1 && slot.data.load(std::memory_order_acquire) == nullptr &&
                (slot.last_used.load(std::memory_order_relaxed) == 0 ||
                 now - slot.last_used.load(std::memory_order_relaxed) > kSlotRecycleMs);
            if (cur == client || ((cur == 0 || recycle) &&
                    slot.client.compare_exchange_strong(cur, client, std::memory_order_acq_rel))) {
                slot.data.store(nullptr, std::memory_order_release);
                slot.requested_frames.store(frames, std::memory_order_relaxed);
                slot.last_used.store(now, std::memory_order_relaxed);
                slot.data.store(data, std::memory_order_release);
                return;
            }
        }
    }
}

bool TakePendingBuffer(PendingSlot* table, uintptr_t client, BYTE*& out_data, UINT32& out_frames) {
    if (!client) return false;
    const std::size_t start = HashPtr(client);
    for (std::size_t probe = 0; probe < kTableCapacity; ++probe) {
        auto& slot = table[(start + probe) & (kTableCapacity - 1)];
        const uintptr_t cur = slot.client.load(std::memory_order_acquire);
        if (cur == client) {
            BYTE* ptr = slot.data.exchange(nullptr, std::memory_order_acq_rel);
            if (!ptr) return false;
            out_frames = slot.requested_frames.load(std::memory_order_relaxed);
            slot.last_used.store(GetTickCount64(), std::memory_order_relaxed);
            out_data = ptr;
            return true;
        }
        if (cur == 0) return false;
    }
    return false;
}

bool ResolveCalibratedFormatRaw(const StreamFormatLayout* l, IAudioRenderClient* client,
                                AudioFormat* out_fmt, uint64_t* out_stream_id) {
    if (!l->valid.load(std::memory_order_acquire) || !client || client->lpVtbl != l->rc_vtbl) return false;
    const intptr_t ac_addr = *reinterpret_cast<const intptr_t*>(
        reinterpret_cast<const char*>(client) + l->rc_to_ac_offset) - l->rc_to_ac_delta;
    if (ac_addr <= 0x10000) return false;
    const auto* ac = reinterpret_cast<const IAudioClient*>(ac_addr);
    if (ac->lpVtbl != l->ac_vtbl) return false;
    const auto* raw_ac = reinterpret_cast<const char*>(ac) + l->ac_to_wf_offset;
    const WAVEFORMATEX* wf = l->wf_is_pointer
        ? *reinterpret_cast<const WAVEFORMATEX* const*>(raw_ac)
        : reinterpret_cast<const WAVEFORMATEX*>(raw_ac);
    const AudioFormat parsed = ParseWaveFormat(wf);
    if (parsed.format == 0) return false;
    *out_fmt = parsed;
    *out_stream_id = static_cast<uint64_t>(reinterpret_cast<uintptr_t>(client));
    return true;
}

DWORD TapReadExceptionFilter(DWORD code) {
    return (code == EXCEPTION_ACCESS_VIOLATION || code == EXCEPTION_IN_PAGE_ERROR)
        ? EXCEPTION_EXECUTE_HANDLER
        : EXCEPTION_CONTINUE_SEARCH;
}

bool ResolveCalibratedFormatSeh(const StreamFormatLayout* l, IAudioRenderClient* client,
                                AudioFormat* out_fmt, uint64_t* out_stream_id, bool* out_faulted) {
    *out_faulted = false;
    __try {
        return ResolveCalibratedFormatRaw(l, client, out_fmt, out_stream_id);
    } __except (TapReadExceptionFilter(GetExceptionCode())) {
        *out_faulted = true;
        return false;
    }
}

void RingWrite(uint8_t* data, uint32_t capacity, uint64_t pos, const void* src, std::size_t len) {
    if (!len) return;
    const uint32_t start = static_cast<uint32_t>(pos) & (capacity - 1);
    const std::size_t first = len < capacity - start ? len : capacity - start;
    std::memcpy(data + start, src, first);
    if (first < len) std::memcpy(data, static_cast<const uint8_t*>(src) + first, len - first);
}

bool CopyPayloadToRingSeh(uint8_t* ring_data, uint32_t cap, uint64_t pos,
                          const BYTE* src, uint32_t len) {
    __try {
        RingWrite(ring_data, cap, pos, src, len);
        return true;
    } __except (TapReadExceptionFilter(GetExceptionCode())) {
        return false;
    }
}

bool WriteTapPacketToRing(TapRingHeader* ring, uint64_t stream_id, const AudioFormat& fmt,
                          const BYTE* data, UINT32 frames, bool silent) {
    if (!ring || ring->active.load(std::memory_order_acquire) == 0) return true;
    const uint32_t cap = ring->capacity;
    if (cap < 64 || (cap & (cap - 1)) != 0) return true;
    const uint64_t raw_payload = silent ? 0ULL : static_cast<uint64_t>(frames) * fmt.block_align;
    if (raw_payload > cap / 4) {
        ring->dropped.fetch_add(1, std::memory_order_relaxed);
        return true;
    }
    const uint32_t payload_bytes = static_cast<uint32_t>(raw_payload);
    const uint32_t packet_bytes = (static_cast<uint32_t>(sizeof(TapPacketHeader)) + payload_bytes + 7u) & ~7u;
    uint64_t write_pos = ring->write_pos.load(std::memory_order_relaxed);
    for (;;) {
        const uint64_t read_pos = ring->read_pos.load(std::memory_order_acquire);
        const uint64_t used = write_pos >= read_pos ? (write_pos - read_pos) : cap;
        if (used + packet_bytes > cap) {
            ring->dropped.fetch_add(1, std::memory_order_relaxed);
            return true;
        }
        if (ring->write_pos.compare_exchange_weak(write_pos, write_pos + packet_bytes,
                std::memory_order_acq_rel, std::memory_order_relaxed)) break;
    }
    LARGE_INTEGER qpc{};
    QueryPerformanceCounter(&qpc);
    TapPacketHeader hdr{packet_bytes, payload_bytes, qpc.QuadPart, stream_id,
        fmt.sample_rate, frames, fmt.channels, fmt.format, silent ? kPacketSilent : uint8_t{0}, 0};
    auto* ring_data = reinterpret_cast<uint8_t*>(ring + 1);
    RingWrite(ring_data, cap, write_pos, &hdr, sizeof(hdr) - sizeof(uint32_t));
    if (payload_bytes && !CopyPayloadToRingSeh(ring_data, cap, write_pos + sizeof(hdr), data, payload_bytes)) {
        return false;
    }
    const uint32_t ready_off = static_cast<uint32_t>(write_pos + offsetof(TapPacketHeader, ready)) & (cap - 1);
    reinterpret_cast<std::atomic<uint32_t>*>(ring_data + ready_off)->store(1u, std::memory_order_release);
    return true;
}

template<class Fn>
void ForEachActiveTap(State& state, Fn&& fn) {
    for (std::size_t i = 0; i < kMaxTaps; ++i) {
        auto& slot = state.taps[i];
        uint64_t s = slot.ref_state.load(std::memory_order_acquire);
        while ((s & 1u) != 0) {
            if (slot.ref_state.compare_exchange_weak(s, s + 2u,
                    std::memory_order_acquire, std::memory_order_relaxed)) {
                fn(slot.ring);
                slot.ref_state.fetch_sub(2u, std::memory_order_release);
                break;
            }
        }
    }
}

void AbortTapsOnFault(State& state) {
    state.format_layout.valid.store(false, std::memory_order_release);
    ForEachActiveTap(state, [](TapRingHeader* ring) {
        if (ring) {
            ring->read_faults.fetch_add(1, std::memory_order_relaxed);
            ring->active.store(0, std::memory_order_release);
        }
    });
}

bool Focused() {
    DWORD pid = 0;
    GetWindowThreadProcessId(GetForegroundWindow(), &pid);
    return pid == GetCurrentProcessId();
}

struct BoolReset {
    bool& flag;
    explicit BoolReset(bool& f) : flag(f) { flag = true; }
    ~BoolReset() { flag = false; }
};

HRESULT STDMETHODCALLTYPE ReleaseBuffer(IAudioRenderClient* client, UINT32 frames, DWORD flags);

HRESULT STDMETHODCALLTYPE GetBuffer(IAudioRenderClient* client, UINT32 frames, BYTE** data) {
    auto& state = GetState();
    auto* hook = FindHook(state.get_hooks, client->lpVtbl);
    if (!hook) return E_UNEXPECTED;
    static thread_local bool in_get = false;
    if (in_get) {
        auto sys = hook->system_original.load(std::memory_order_acquire);
        return sys ? sys(client, frames, data) : E_FAIL;
    }
    BoolReset guard(in_get);
    const HRESULT hr = hook->original(client, frames, data);
    if (SUCCEEDED(hr) && data && *data) {
        StorePendingBuffer(state.pending_buffers, reinterpret_cast<uintptr_t>(client), *data, frames);
    }
    return hr;
}

HRESULT STDMETHODCALLTYPE ReleaseBuffer(IAudioRenderClient* client, UINT32 frames, DWORD flags) {
    auto& state = GetState();
    auto* hook = FindHook(state.release_hooks, client->lpVtbl);
    if (!hook) return E_UNEXPECTED;
    static thread_local bool in_release = false;
    if (in_release) {
        auto sys = hook->system_original.load(std::memory_order_acquire);
        return sys ? sys(client, frames, flags) : E_FAIL;
    }
    BoolReset guard(in_release);
    state.calls.fetch_add(1, std::memory_order_relaxed);

    BYTE* data = nullptr;
    UINT32 requested_frames = 0;
    const bool has_pending = TakePendingBuffer(
        state.pending_buffers, reinterpret_cast<uintptr_t>(client), data, requested_frames);
    const bool hook_above = client->lpVtbl->ReleaseBuffer != ReleaseBuffer;

    if (frames > 0 && state.active_taps.load(std::memory_order_acquire) > 0) {
        AudioFormat fmt{};
        uint64_t stream_id = 0;
        bool faulted = false;
        if (!ResolveCalibratedFormatSeh(&state.format_layout, client, &fmt, &stream_id, &faulted)) {
            if (faulted) {
                AbortTapsOnFault(state);
            } else {
                ForEachActiveTap(state, [](TapRingHeader* ring) {
                    if (ring && ring->active.load(std::memory_order_acquire) != 0)
                        ring->skipped_unknown.fetch_add(1, std::memory_order_relaxed);
                });
            }
        } else {
            const bool silent = (!hook_above && (flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0)
                || !has_pending || !data || frames > requested_frames;
            bool copy_faulted = false;
            ForEachActiveTap(state, [&](TapRingHeader* ring) {
                if (!WriteTapPacketToRing(ring, stream_id, fmt, data, frames, silent)) {
                    copy_faulted = true;
                }
            });
            if (copy_faulted) AbortTapsOnFault(state);
        }
    }

    const auto mode = state.mode.load(std::memory_order_acquire);
    if (renium_audio::Muted(mode, state.deadline.load(std::memory_order_acquire),
            GetTickCount64(), mode == 4 && Focused())) {
        flags |= AUDCLNT_BUFFERFLAGS_SILENT;
        state.silenced.fetch_add(1, std::memory_order_relaxed);
    }
    return hook->original(client, frames, flags);
}

void Check(HRESULT result, const char* operation) {
    if (FAILED(result)) {
        char code[24]{};
        std::snprintf(code, sizeof(code), " (0x%08lX)", static_cast<unsigned long>(result));
        throw std::runtime_error(std::string(operation) + code);
    }
}

[[noreturn]] void ThrowWinError(const char* operation, DWORD err = GetLastError()) {
    char buf[96]{};
    std::snprintf(buf, sizeof(buf), "%s (%lu)", operation, err);
    throw std::runtime_error(buf);
}

void Probe(IAudioRenderClient* client) {
    if (!client || !client->lpVtbl) throw std::runtime_error("Audio render interface is missing");
    const auto entries = reinterpret_cast<const void* const*>(client->lpVtbl);
    for (std::size_t i = 0; i < sizeof(IAudioRenderClientVtbl) / sizeof(void*); ++i) {
        MEMORY_BASIC_INFORMATION page{};
        if (!VirtualQuery(entries[i], &page, sizeof(page)) || page.State != MEM_COMMIT ||
            (page.Protect & (PAGE_GUARD | PAGE_NOACCESS)) ||
            !(page.Protect & (PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)))
            throw std::runtime_error("Audio render vtable failed its read-only executable-page probe");
    }
}

void PinFunction(const void* fn, const char* message) {
    HMODULE module = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            reinterpret_cast<LPCWSTR>(fn), &module))
        throw std::runtime_error(message);
}

template<class Vtbl, class Fn>
bool InstallSlot(std::atomic<SlotHook<Vtbl, Fn>*>& head, const Vtbl* table, Fn* slot,
                 Fn replacement, Fn system_fn, const char* protect_err, const char* race_err) {
    if (auto* existing = FindHook(head, table)) {
        if (system_fn && existing->system_original.load(std::memory_order_acquire) == nullptr) {
            PinFunction(reinterpret_cast<const void*>(system_fn), "Could not retain the system audio implementation");
            existing->system_original.store(system_fn, std::memory_order_release);
        }
        return false;
    }
    Fn original = *slot;
    if (original == replacement) return false;
    PinFunction(reinterpret_cast<const void*>(original), "Could not retain the audio implementation");
    if (system_fn && system_fn != original) {
        PinFunction(reinterpret_cast<const void*>(system_fn), "Could not retain the system audio implementation");
    }
    DWORD prot = 0, unused = 0;
    if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &prot)) throw std::runtime_error(protect_err);
    auto* hook = new SlotHook<Vtbl, Fn>{table, original, {system_fn}, head.load(std::memory_order_relaxed)};
    head.store(hook, std::memory_order_release);
    auto replaced = InterlockedCompareExchangePointer(reinterpret_cast<void* volatile*>(slot),
        reinterpret_cast<void*>(replacement), reinterpret_cast<void*>(original));
    VirtualProtect(slot, sizeof(*slot), prot, &unused);
    if (replaced != reinterpret_cast<void*>(original)) {
        head.store(hook->next, std::memory_order_release);
        throw std::runtime_error(race_err);
    }
    return true;
}

void Install(IAudioRenderClient* client, decltype(IAudioRenderClientVtbl::ReleaseBuffer) system_release = nullptr) {
    Probe(client);
    auto& state = GetState();
    auto* slot = const_cast<decltype(IAudioRenderClientVtbl::ReleaseBuffer)*>(&client->lpVtbl->ReleaseBuffer);
    if (InstallSlot(state.release_hooks, client->lpVtbl, slot, ReleaseBuffer, system_release,
            "Could not attach Studio output suppression",
            "Audio output implementation changed during attachment")) {
        ++state.count;
    }
}

void InstallTapRender(IAudioRenderClient* client,
                      decltype(IAudioRenderClientVtbl::GetBuffer) system_get = nullptr,
                      decltype(IAudioRenderClientVtbl::ReleaseBuffer) system_release = nullptr) {
    Install(client, system_release);
    auto* slot = const_cast<decltype(IAudioRenderClientVtbl::GetBuffer)*>(&client->lpVtbl->GetBuffer);
    InstallSlot(GetState().get_hooks, client->lpVtbl, slot, GetBuffer, system_get,
        "Could not attach Studio output capture",
        "Audio output implementation changed during attachment");
}

bool IsReadable(const void* ptr, std::size_t len) {
    MEMORY_BASIC_INFORMATION mbi{};
    if (!ptr || !VirtualQuery(ptr, &mbi, sizeof(mbi)) || mbi.State != MEM_COMMIT ||
        (mbi.Protect & (PAGE_GUARD | PAGE_NOACCESS))) return false;
    const uintptr_t start = reinterpret_cast<uintptr_t>(ptr);
    const uintptr_t end = reinterpret_cast<uintptr_t>(mbi.BaseAddress) + mbi.RegionSize;
    return start <= end && len <= end - start;
}

bool MatchesWaveFormat(const void* ptr, const WAVEFORMATEXTENSIBLE& want, bool extensible) {
    if (!IsReadable(ptr, sizeof(WAVEFORMATEX))) return false;
    const auto* got = reinterpret_cast<const WAVEFORMATEX*>(ptr);
    if (got->nChannels != want.Format.nChannels || got->nSamplesPerSec != want.Format.nSamplesPerSec ||
        got->wBitsPerSample != want.Format.wBitsPerSample || got->nBlockAlign != want.Format.nBlockAlign ||
        got->nAvgBytesPerSec != want.Format.nAvgBytesPerSec) return false;
    if (!extensible) return got->wFormatTag == want.Format.wFormatTag;
    return got->wFormatTag == WAVE_FORMAT_EXTENSIBLE && got->cbSize >= 22 &&
        IsReadable(ptr, sizeof(WAVEFORMATEXTENSIBLE)) &&
        IsEqualGUID(reinterpret_cast<const WAVEFORMATEXTENSIBLE*>(ptr)->SubFormat, want.SubFormat) != 0;
}

struct LayoutProbe {
    Com<IAudioClient> ac;
    Com<IAudioRenderClient> rc;
    WAVEFORMATEXTENSIBLE fmt{};
    bool extensible = false;
};

bool MakeLayoutProbe(IMMDevice* device, uint32_t rate, uint16_t ch, uint16_t bits,
                     bool is_float, bool extensible, LayoutProbe& out) {
    if (FAILED(IMMDevice_Activate(device, IID_IAudioClient, CLSCTX_ALL, nullptr,
            reinterpret_cast<void**>(&out.ac.value))) || !out.ac.value) return false;
    out.extensible = extensible;
    WAVEFORMATEX* wf = &out.fmt.Format;
    wf->nChannels = ch;
    wf->nSamplesPerSec = rate;
    wf->wBitsPerSample = bits;
    wf->nBlockAlign = static_cast<WORD>(ch * (bits / 8));
    wf->nAvgBytesPerSec = rate * wf->nBlockAlign;
    wf->wFormatTag = extensible ? WAVE_FORMAT_EXTENSIBLE : (is_float ? WAVE_FORMAT_IEEE_FLOAT : WAVE_FORMAT_PCM);
    wf->cbSize = extensible ? 22 : 0;
    if (extensible) {
        out.fmt.Samples.wValidBitsPerSample = bits;
        out.fmt.dwChannelMask = (ch == 1) ? 4u : 3u;
        out.fmt.SubFormat = is_float ? KSDATAFORMAT_SUBTYPE_IEEE_FLOAT : KSDATAFORMAT_SUBTYPE_PCM;
    }
    const DWORD flags = AUDCLNT_STREAMFLAGS_NOPERSIST | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    return SUCCEEDED(IAudioClient_Initialize(out.ac.value, AUDCLNT_SHAREMODE_SHARED, flags, 1000000, 0, wf, nullptr))
        && SUCCEEDED(IAudioClient_GetService(out.ac.value, IID_IAudioRenderClient,
            reinterpret_cast<void**>(&out.rc.value))) && out.rc.value != nullptr;
}

void CalibrateStreamFormatLayout(IMMDevice* device, Com<IAudioRenderClient>& out_render) {
    auto& state = GetState();
    if (state.format_layout.valid.load(std::memory_order_acquire) || !device) return;
    LayoutProbe p[3];
    const bool ok = MakeLayoutProbe(device, 44100, 2, 16, false, false, p[0])
        && MakeLayoutProbe(device, 48000, 2, 32, true, true, p[1])
        && MakeLayoutProbe(device, 48000, 1, 16, false, false, p[2]);
    if (p[0].rc.value && !out_render.value) {
        out_render.value = p[0].rc.value;
        out_render.value->lpVtbl->AddRef(out_render.value);
    }
    if (!ok) return;
    const void* rc_vtbl = p[0].rc.value->lpVtbl;
    const void* ac_vtbl = p[0].ac.value->lpVtbl;
    if (p[1].rc.value->lpVtbl != rc_vtbl || p[2].rc.value->lpVtbl != rc_vtbl ||
        p[1].ac.value->lpVtbl != ac_vtbl || p[2].ac.value->lpVtbl != ac_vtbl) return;

    int rc_matches = 0, wf_matches = 0;
    std::size_t rc_off = 0, wf_off = 0;
    intptr_t rc_delta = 0;
    bool wf_ptr = false;
    for (std::size_t off = 0; off < 256; off += sizeof(void*)) {
        const auto* a0 = reinterpret_cast<const char*>(p[0].rc.value) + off;
        const auto* a1 = reinterpret_cast<const char*>(p[1].rc.value) + off;
        const auto* a2 = reinterpret_cast<const char*>(p[2].rc.value) + off;
        if (!IsReadable(a0, sizeof(void*)) || !IsReadable(a1, sizeof(void*)) || !IsReadable(a2, sizeof(void*))) continue;
        const intptr_t d0 = *reinterpret_cast<const intptr_t*>(a0) - reinterpret_cast<intptr_t>(p[0].ac.value);
        const intptr_t d1 = *reinterpret_cast<const intptr_t*>(a1) - reinterpret_cast<intptr_t>(p[1].ac.value);
        const intptr_t d2 = *reinterpret_cast<const intptr_t*>(a2) - reinterpret_cast<intptr_t>(p[2].ac.value);
        if (d0 == d1 && d1 == d2 && d0 >= -128 && d0 <= 128) { rc_off = off; rc_delta = d0; ++rc_matches; }
    }
    if (rc_matches != 1) return;

    for (std::size_t off = 0; off < 1024; off += sizeof(void*)) {
        const auto* a0 = reinterpret_cast<const char*>(p[0].ac.value) + off;
        const auto* a1 = reinterpret_cast<const char*>(p[1].ac.value) + off;
        const auto* a2 = reinterpret_cast<const char*>(p[2].ac.value) + off;
        if (!IsReadable(a0, sizeof(void*)) || !IsReadable(a1, sizeof(void*)) || !IsReadable(a2, sizeof(void*))) continue;
        if (MatchesWaveFormat(*reinterpret_cast<const void* const*>(a0), p[0].fmt, p[0].extensible) &&
            MatchesWaveFormat(*reinterpret_cast<const void* const*>(a1), p[1].fmt, p[1].extensible) &&
            MatchesWaveFormat(*reinterpret_cast<const void* const*>(a2), p[2].fmt, p[2].extensible)) {
            wf_off = off; wf_ptr = true; ++wf_matches;
        }
    }
    if (wf_matches == 0) {
        for (std::size_t off = 0; off < 1024; off += 4) {
            if (MatchesWaveFormat(reinterpret_cast<const char*>(p[0].ac.value) + off, p[0].fmt, p[0].extensible) &&
                MatchesWaveFormat(reinterpret_cast<const char*>(p[1].ac.value) + off, p[1].fmt, p[1].extensible) &&
                MatchesWaveFormat(reinterpret_cast<const char*>(p[2].ac.value) + off, p[2].fmt, p[2].extensible)) {
                wf_off = off; wf_ptr = false; ++wf_matches;
            }
        }
    }
    if (wf_matches != 1) return;

    Com<IAudioClient> verify_ac;
    Com<IAudioRenderClient> verify_rc;
    WAVEFORMATEX* mix = nullptr;
    if (FAILED(IMMDevice_Activate(device, IID_IAudioClient, CLSCTX_ALL, nullptr,
            reinterpret_cast<void**>(&verify_ac.value))) || !verify_ac.value ||
        FAILED(IAudioClient_GetMixFormat(verify_ac.value, &mix)) || !mix) return;
    const AudioFormat expected = ParseWaveFormat(mix);
    const HRESULT init_hr = IAudioClient_Initialize(verify_ac.value, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_NOPERSIST, 1000000, 0, mix, nullptr);
    CoTaskMemFree(mix);
    if (FAILED(init_hr) || expected.format == 0 ||
        FAILED(IAudioClient_GetService(verify_ac.value, IID_IAudioRenderClient,
            reinterpret_cast<void**>(&verify_rc.value))) || !verify_rc.value) return;

    state.format_layout.rc_vtbl = rc_vtbl;
    state.format_layout.ac_vtbl = ac_vtbl;
    state.format_layout.rc_to_ac_offset = rc_off;
    state.format_layout.rc_to_ac_delta = rc_delta;
    state.format_layout.ac_to_wf_offset = wf_off;
    state.format_layout.wf_is_pointer = wf_ptr;
    state.format_layout.valid.store(true, std::memory_order_release);

    AudioFormat verified{};
    uint64_t sid = 0;
    bool faulted = false;
    if (!ResolveCalibratedFormatSeh(&state.format_layout, verify_rc.value, &verified, &sid, &faulted) ||
        faulted || verified.sample_rate != expected.sample_rate || verified.channels != expected.channels ||
        verified.block_align != expected.block_align || verified.format != expected.format) {
        state.format_layout.valid.store(false, std::memory_order_release);
    }
}

bool OpenRenderProbe(IMMDevice* device, Com<IAudioClient>& client, Com<IAudioRenderClient>& render,
                     bool skip_invalidated) {
    Check(IMMDevice_Activate(device, IID_IAudioClient, CLSCTX_ALL, nullptr,
        reinterpret_cast<void**>(&client.value)), "Could not inspect audio output");
    WAVEFORMATEX* format = nullptr;
    Check(IAudioClient_GetMixFormat(client.value, &format), "Could not read output format");
    const GUID session = {0x8d9e03d9, 0x0a12, 0x4793, {0x92,0x0c,0xf1,0xbd,0x53,0x29,0x03,0xd9}};
    const auto initialized = IAudioClient_Initialize(client.value, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_NOPERSIST, 1000000, 0, format, &session);
    CoTaskMemFree(format);
    if (skip_invalidated &&
        (initialized == AUDCLNT_E_DEVICE_INVALIDATED || initialized == AUDCLNT_E_RESOURCES_INVALIDATED)) return false;
    Check(initialized, "Could not inspect audio rendering");
    Check(IAudioClient_GetService(client.value, IID_IAudioRenderClient,
        reinterpret_cast<void**>(&render.value)), "Could not resolve audio rendering");
    return true;
}

void DiscoverGate(ULONGLONG expected_vtbl, ULONGLONG system_release) {
    Com<IMMDeviceEnumerator> enumerator;
    Check(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
        IID_IMMDeviceEnumerator, reinterpret_cast<void**>(&enumerator.value)), "Could not open audio devices");
    Com<IMMDeviceCollection> devices;
    Check(IMMDeviceEnumerator_EnumAudioEndpoints(enumerator.value, eRender, DEVICE_STATE_ACTIVE,
        &devices.value), "Could not list audio outputs");
    UINT count = 0;
    Check(IMMDeviceCollection_GetCount(devices.value, &count), "Could not count audio outputs");
    for (UINT i = 0; i < count; ++i) {
        Com<IMMDevice> device;
        Check(IMMDeviceCollection_Item(devices.value, i, &device.value), "Could not open audio output");
        Com<IAudioClient> client;
        Com<IAudioRenderClient> render;
        if (OpenRenderProbe(device.value, client, render, true)) {
            const bool match = expected_vtbl && reinterpret_cast<uintptr_t>(render.value->lpVtbl) == expected_vtbl;
            Install(render.value,
                match ? reinterpret_cast<decltype(IAudioRenderClientVtbl::ReleaseBuffer)>(system_release) : nullptr);
        }
    }
}

void DiscoverForTap(ULONGLONG expected_vtbl, ULONGLONG system_get, ULONGLONG system_release) {
    auto& state = GetState();
    const bool need_cal = !state.format_layout.valid.load(std::memory_order_acquire);
    if (state.tap_hooks_installed && !need_cal) return;
    Com<IMMDeviceEnumerator> enumerator;
    Check(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
        IID_IMMDeviceEnumerator, reinterpret_cast<void**>(&enumerator.value)), "Could not open audio devices");
    Com<IMMDevice> device;
    if (FAILED(IMMDeviceEnumerator_GetDefaultAudioEndpoint(enumerator.value, eRender, eConsole, &device.value)) || !device.value) {
        Com<IMMDeviceCollection> devices;
        Check(IMMDeviceEnumerator_EnumAudioEndpoints(enumerator.value, eRender, DEVICE_STATE_ACTIVE,
            &devices.value), "Could not list audio outputs");
        UINT count = 0;
        Check(IMMDeviceCollection_GetCount(devices.value, &count), "Could not count audio outputs");
        if (count == 0) throw std::runtime_error("No active audio output device is available");
        Check(IMMDeviceCollection_Item(devices.value, 0, &device.value), "Could not open audio output");
    }
    Com<IAudioRenderClient> render;
    if (need_cal) CalibrateStreamFormatLayout(device.value, render);
    if (!state.tap_hooks_installed) {
        Com<IAudioClient> fallback_client;
        if (!render.value) OpenRenderProbe(device.value, fallback_client, render, false);
        const bool match = expected_vtbl && reinterpret_cast<uintptr_t>(render.value->lpVtbl) == expected_vtbl;
        InstallTapRender(render.value,
            match ? reinterpret_cast<decltype(IAudioRenderClientVtbl::GetBuffer)>(system_get) : nullptr,
            match ? reinterpret_cast<decltype(IAudioRenderClientVtbl::ReleaseBuffer)>(system_release) : nullptr);
        state.tap_hooks_installed = true;
    }
}

void PreFaultRing(TapRingHeader* view, std::size_t total_bytes) {
    auto* bytes = reinterpret_cast<volatile uint8_t*>(view);
    for (std::size_t off = 0; off < total_bytes; off += 4096) {
        const uint8_t v = bytes[off];
        if (off >= sizeof(TapRingHeader)) bytes[off] = v;
    }
}

void CloseTapSlotLocked(State& state, TapSlot& slot) {
    if ((slot.ref_state.fetch_and(~1ULL, std::memory_order_acq_rel) & 1u) != 0)
        state.active_taps.fetch_sub(1, std::memory_order_release);
    while ((slot.ref_state.load(std::memory_order_acquire) >> 1) != 0) SwitchToThread();
    if (slot.ring) {
        slot.ring->active.store(0, std::memory_order_release);
        UnmapViewOfFile(slot.ring);
        slot.ring = nullptr;
    }
    if (slot.mapping) { CloseHandle(slot.mapping); slot.mapping = nullptr; }
    if (slot.owner_process) { CloseHandle(slot.owner_process); slot.owner_process = nullptr; }
    slot.id = 0;
}

void ReapDeadTapsLocked(State& state) {
    for (std::size_t i = 0; i < kMaxTaps; ++i) {
        auto& slot = state.taps[i];
        if ((slot.ref_state.load(std::memory_order_acquire) & 1u) == 0) continue;
        const bool dead = slot.owner_process && WaitForSingleObject(slot.owner_process, 0) == WAIT_OBJECT_0;
        const bool off = slot.ring && slot.ring->active.load(std::memory_order_acquire) == 0;
        if (dead || off) CloseTapSlotLocked(state, slot);
    }
}

DWORD WINAPI TapWatcherThread(LPVOID) {
    auto& state = GetState();
    for (;;) {
        HANDLE handles[kMaxTaps + 1]{};
        DWORD count = 0;
        {
            std::lock_guard<std::mutex> lock(state.mutex);
            ReapDeadTapsLocked(state);
            if (state.active_taps.load(std::memory_order_acquire) == 0) {
                if (state.watcher_thread) { CloseHandle(state.watcher_thread); state.watcher_thread = nullptr; }
                return 0;
            }
            if (state.watcher_wake) handles[count++] = state.watcher_wake;
            for (auto& slot : state.taps) {
                if ((slot.ref_state.load(std::memory_order_acquire) & 1u) != 0 && slot.owner_process)
                    handles[count++] = slot.owner_process;
            }
        }
        if (count) WaitForMultipleObjects(count, handles, FALSE, 500);
        else Sleep(500);
    }
}

struct Parameters {
    unsigned size, version, action, refresh, focused, muted, hooks, status;
    ULONGLONG calls, silenced;
    char error[256];
};
static_assert(sizeof(Parameters) == 304);

struct ParametersV2 {
    Parameters base;
    ULONGLONG render_vtbl, system_release_buffer;
};
static_assert(sizeof(ParametersV2) == 320);

struct TapParameters {
    unsigned size, version, action, status, recorder_pid, reserved;
    ULONGLONG tap_id, render_vtbl, system_get_buffer, system_release_buffer;
    wchar_t mapping_name[128];
    char error[256];
};
static_assert(sizeof(TapParameters) == 568);
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumAudioStep(void* input) {
    auto* params = static_cast<Parameters*>(input);
    if (!params || params->action > 4) return ERROR_INVALID_PARAMETER;
    const bool v1 = params->version == 1 && params->size == sizeof(Parameters);
    const bool v2 = params->version == 2 && params->size == sizeof(ParametersV2);
    if (!v1 && !v2) return ERROR_INVALID_PARAMETER;
    const ULONGLONG expected_vtbl = v2 ? static_cast<ParametersV2*>(input)->render_vtbl : 0;
    const ULONGLONG system_rel = v2 ? static_cast<ParametersV2*>(input)->system_release_buffer : 0;
    auto& state = GetState();
    std::lock_guard<std::mutex> lock(state.mutex);
    const auto com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    try {
        if (com != RPC_E_CHANGED_MODE) Check(com, "Could not initialize audio control");
        PinFunction(reinterpret_cast<const void*>(ReniumAudioStep), "Could not retain Studio audio control");
        if (params->action != 0) {
            if (params->action == 2 || params->action == 4) {
                if (params->refresh || !state.count) DiscoverGate(expected_vtbl, system_rel);
                state.deadline.store(GetTickCount64() + 3000, std::memory_order_release);
                state.mode.store(params->action, std::memory_order_release);
            } else {
                state.mode.store(0, std::memory_order_release);
            }
        }
        params->focused = Focused();
        params->muted = renium_audio::Muted(state.mode.load(), state.deadline.load(), GetTickCount64(), params->focused != 0);
        params->hooks = state.count;
        params->calls = state.calls.load();
        params->silenced = state.silenced.load();
        params->status = 1;
    } catch (const std::exception& error) {
        state.mode.store(0, std::memory_order_release);
        std::snprintf(params->error, sizeof(params->error), "%s", error.what());
    }
    if (SUCCEEDED(com)) CoUninitialize();
    return 0;
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumAudioTap(void* input) {
    auto* params = static_cast<TapParameters*>(input);
    if (!params || params->size != sizeof(TapParameters) || params->version != kTapVersion ||
        (params->action != 1 && params->action != 2)) return ERROR_INVALID_PARAMETER;
    auto& state = GetState();
    std::lock_guard<std::mutex> lock(state.mutex);
    const auto com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    try {
        if (com != RPC_E_CHANGED_MODE) Check(com, "Could not initialize audio capture");
        PinFunction(reinterpret_cast<const void*>(ReniumAudioTap), "Could not retain Studio audio capture");
        ReapDeadTapsLocked(state);
        if (params->action == 2) {
            for (auto& slot : state.taps) {
                if (slot.id == params->tap_id && params->tap_id != 0) { CloseTapSlotLocked(state, slot); break; }
            }
            if (state.watcher_wake) SetEvent(state.watcher_wake);
            params->status = 1;
        } else {
            if (!params->tap_id) throw std::runtime_error("Audio tap id is missing");
            if (!params->recorder_pid) throw std::runtime_error("Recorder PID is missing");
            HANDLE owner = OpenProcess(SYNCHRONIZE, FALSE, params->recorder_pid);
            if (!owner) ThrowWinError("Could not watch recorder process");
            auto close_owner = [&]() { if (owner) { CloseHandle(owner); owner = nullptr; } };
            try {
                DiscoverForTap(params->render_vtbl, params->system_get_buffer, params->system_release_buffer);
                if (state.count == 0) throw std::runtime_error("No active audio output stream could be attached");
                TapSlot* free_slot = nullptr;
                for (auto& slot : state.taps) {
                    if ((slot.ref_state.load(std::memory_order_acquire) & 1u) == 0 && !slot.ring) {
                        free_slot = &slot; break;
                    }
                }
                if (!free_slot) throw std::runtime_error("Too many concurrent audio recordings on one Studio process");
                params->mapping_name[127] = L'\0';
                HANDLE mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, params->mapping_name);
                if (!mapping) ThrowWinError("Could not open audio tap ring");
                auto* view = static_cast<TapRingHeader*>(MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0));
                if (!view) { const DWORD err = GetLastError(); CloseHandle(mapping); ThrowWinError("Could not map audio tap ring", err); }
                if (view->magic != kTapMagic || view->version != kTapVersion ||
                    view->capacity < 64 || (view->capacity & (view->capacity - 1)) != 0) {
                    UnmapViewOfFile(view); CloseHandle(mapping);
                    throw std::runtime_error("Audio tap ring header is invalid");
                }
                PreFaultRing(view, sizeof(TapRingHeader) + view->capacity);
                free_slot->id = params->tap_id;
                free_slot->mapping = mapping;
                free_slot->owner_process = owner;
                owner = nullptr;
                free_slot->ring = view;
                free_slot->ref_state.store(1u, std::memory_order_release);
                state.active_taps.fetch_add(1, std::memory_order_release);
                if (!state.watcher_wake) state.watcher_wake = CreateEventW(nullptr, FALSE, FALSE, nullptr);
                if (!state.watcher_thread) state.watcher_thread = CreateThread(nullptr, 0, TapWatcherThread, nullptr, 0, nullptr);
                else if (state.watcher_wake) SetEvent(state.watcher_wake);
                params->status = 1;
            } catch (...) {
                close_owner();
                throw;
            }
        }
    } catch (const std::exception& error) {
        std::snprintf(params->error, sizeof(params->error), "%s", error.what());
    }
    if (SUCCEEDED(com)) CoUninitialize();
    return 0;
}
