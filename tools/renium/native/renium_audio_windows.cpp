#define CINTERFACE
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <initguid.h>
#include <atomic>
#include <cstdio>
#include <mutex>
#include <stdexcept>
#include <string>
#include "renium_audio_policy.h"

DEFINE_GUID(IID_IAudioClient, 0x1cb9ad4c, 0xdbfa, 0x4c32, 0xb1,0x78,0xc2,0xf5,0x68,0xa7,0x03,0xb2);
DEFINE_GUID(IID_IAudioRenderClient, 0xf294acfc, 0x3146, 0x4483, 0xa7,0xbf,0xad,0xdc,0xa7,0xc2,0x60,0xe2);
DEFINE_GUID(IID_IMMDeviceEnumerator, 0xa95664d2, 0x9614, 0x4f35, 0xa7,0x46,0xde,0x8d,0xb6,0x36,0x17,0xe6);

namespace {
template<class T> struct Com {
    T* value = nullptr;
    ~Com() { if (value) value->lpVtbl->Release(value); }
};

struct Hook {
    const IAudioRenderClientVtbl* table;
    decltype(IAudioRenderClientVtbl::ReleaseBuffer) original;
    Hook* next;
};

struct State {
    std::mutex mutex;
    std::atomic<Hook*> hooks{nullptr};
    std::atomic<unsigned> mode{0};
    std::atomic<ULONGLONG> deadline{0};
    std::atomic<ULONGLONG> calls{0};
    std::atomic<ULONGLONG> silenced{0};
    unsigned count = 0;
};

State& GetState() {
    static auto* state = new State;
    return *state;
}

bool Focused() {
    DWORD pid = 0;
    GetWindowThreadProcessId(GetForegroundWindow(), &pid);
    return pid == GetCurrentProcessId();
}

HRESULT STDMETHODCALLTYPE ReleaseBuffer(IAudioRenderClient* client, UINT32 frames, DWORD flags) {
    auto& state = GetState();
    auto* hook = state.hooks.load(std::memory_order_acquire);
    while (hook && hook->table != client->lpVtbl) hook = hook->next;
    if (!hook) return E_UNEXPECTED;
    state.calls.fetch_add(1, std::memory_order_relaxed);
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

void Install(IAudioRenderClient* client) {
    auto& state = GetState();
    auto* slot = const_cast<decltype(IAudioRenderClientVtbl::ReleaseBuffer)*>(&client->lpVtbl->ReleaseBuffer);
    auto original = *slot;
    if (original == ReleaseBuffer) return;
    HMODULE module = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            reinterpret_cast<LPCWSTR>(original), &module))
        throw std::runtime_error("Could not retain the audio output implementation");
    DWORD protection = 0;
    if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &protection))
        throw std::runtime_error("Could not attach Studio output suppression");
    auto* hook = new Hook{client->lpVtbl, original, state.hooks.load(std::memory_order_relaxed)};
    state.hooks.store(hook, std::memory_order_release);
    auto replaced = InterlockedCompareExchangePointer(reinterpret_cast<void* volatile*>(slot),
        reinterpret_cast<void*>(ReleaseBuffer), reinterpret_cast<void*>(original));
    DWORD unused = 0;
    VirtualProtect(slot, sizeof(*slot), protection, &unused);
    if (replaced != reinterpret_cast<void*>(original))
        throw std::runtime_error("Audio output implementation changed during attachment");
    ++state.count;
}

void Discover() {
    Com<IMMDeviceEnumerator> enumerator;
    Check(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
        IID_IMMDeviceEnumerator, reinterpret_cast<void**>(&enumerator.value)), "Could not open audio devices");
    Com<IMMDeviceCollection> devices;
    Check(IMMDeviceEnumerator_EnumAudioEndpoints(enumerator.value, eRender, DEVICE_STATE_ACTIVE,
        &devices.value), "Could not list audio outputs");
    UINT count = 0;
    Check(IMMDeviceCollection_GetCount(devices.value, &count), "Could not count audio outputs");
    for (UINT index = 0; index < count; ++index) {
        Com<IMMDevice> device;
        Check(IMMDeviceCollection_Item(devices.value, index, &device.value), "Could not open audio output");
        Com<IAudioClient> client;
        Check(IMMDevice_Activate(device.value, IID_IAudioClient, CLSCTX_ALL, nullptr,
            reinterpret_cast<void**>(&client.value)), "Could not inspect audio output");
        WAVEFORMATEX* format = nullptr;
        Check(IAudioClient_GetMixFormat(client.value, &format), "Could not read output format");
        const GUID session = {0x8d9e03d9, 0x0a12, 0x4793, {0x92,0x0c,0xf1,0xbd,0x53,0x29,0x03,0xd9}};
        const auto initialized = IAudioClient_Initialize(client.value, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_NOPERSIST, 1000000, 0, format, &session);
        CoTaskMemFree(format);
        if (initialized == AUDCLNT_E_DEVICE_INVALIDATED || initialized == AUDCLNT_E_RESOURCES_INVALIDATED) continue;
        Check(initialized, "Could not inspect audio rendering");
        Com<IAudioRenderClient> render;
        Check(IAudioClient_GetService(client.value, IID_IAudioRenderClient,
            reinterpret_cast<void**>(&render.value)), "Could not resolve audio rendering");
        Install(render.value);
    }
}

struct Parameters {
    unsigned size, version, action, refresh;
    unsigned focused, muted, hooks, status;
    ULONGLONG calls, silenced;
    char error[256];
};
static_assert(sizeof(Parameters) == 304);
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumAudioStep(void* input) {
    auto* params = static_cast<Parameters*>(input);
    if (!params || params->size != sizeof(Parameters) || params->version != 1 || params->action > 4)
        return ERROR_INVALID_PARAMETER;
    auto& state = GetState();
    std::lock_guard<std::mutex> lock(state.mutex);
    const auto com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    try {
        Check(com, "Could not initialize audio control");
        HMODULE module = nullptr;
        if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                reinterpret_cast<LPCWSTR>(ReniumAudioStep), &module))
            throw std::runtime_error("Could not retain Studio audio control");
        if (params->action != 0) {
            if (params->action == 2 || params->action == 4) {
                if (params->refresh || !state.count) Discover();
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

BOOL WINAPI DllMain(HINSTANCE, DWORD, LPVOID) { return TRUE; }
