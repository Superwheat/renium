#pragma once
#include <cstdint>
#include <utility>

// Only the newly created, suspended StartServer process receives this input.
// The original controller remains the source of its test arguments and Stop.
struct TestServerInputConfiguration {
    std::uint32_t version, size;
    std::uint64_t slot, callback, capture, path;
    std::uint64_t rootCreation;
    std::uint32_t rootPid, reserved;
    wchar_t file[32768];
};
static_assert(sizeof(TestServerInputConfiguration) == 65592);
namespace renium_test_server {
using Callback = void(*)(void*, void**);
using StringFromWide = void*(*)(void*, const wchar_t*, int);
using StringDestroy = void(*)(void*);
inline TestServerInputConfiguration configuration{};
inline Callback original = nullptr;
inline StringFromWide fromWide = nullptr;
inline StringDestroy destroy = nullptr;
inline void** slot = nullptr;
inline volatile LONG phase = 0;

inline bool Exchange(void* expected, void* replacement) {
    DWORD protection = 0;
    if (!VirtualProtect(slot, sizeof(void*), PAGE_READWRITE, &protection)) return false;
    const bool changed = InterlockedCompareExchangePointer(slot, replacement, expected) == expected;
    DWORD ignored = 0;
    return VirtualProtect(slot, sizeof(void*), protection, &ignored) && changed;
}

inline void Load(void* capture, void** model) {
    if (InterlockedCompareExchange(&phase, 2, 1) != 1) {
        original(capture, model);
        return;
    }
    // This is the engine's std::function<void(DataModel*)> input callback.
    // Its native file reader consumes a copy of this QString synchronously.
    void* replacement = nullptr;
    fromWide(&replacement, configuration.file, -1);
    auto payload = *reinterpret_cast<char**>(static_cast<char*>(capture) + configuration.capture);
    auto& path = *reinterpret_cast<void**>(payload + configuration.path);
    std::swap(path, replacement);
    try {
        original(capture, model);
    } catch (...) {
        std::swap(path, replacement);
        destroy(&replacement);
        Exchange(reinterpret_cast<void*>(&Load), reinterpret_cast<void*>(original));
        InterlockedExchange(&phase, 4);
        throw;
    }
    std::swap(path, replacement);
    destroy(&replacement);
    const bool restored = Exchange(reinterpret_cast<void*>(&Load), reinterpret_cast<void*>(original));
    InterlockedExchange(&phase, restored ? 3 : 4);
}
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumInitializeTestServer(void* argument) {
    using namespace renium_test_server;
    const auto& input = *static_cast<const TestServerInputConfiguration*>(argument);
    if (input.version != 2 || input.size != sizeof(input) || !input.slot || !input.callback
        || !input.rootPid || !input.rootCreation || input.reserved
        || input.slot % 8 || input.capture > 65536 || input.path > 65536
        || input.capture % 8 || input.path % 8 || !input.file[0] || input.file[32767]) return ERROR_INVALID_PARAMETER;
    if (InterlockedCompareExchange(&phase, -1, 0) != 0) return ERROR_ALREADY_INITIALIZED;
    const auto qt = GetModuleHandleW(L"Qt5Core.dll");
    fromWide = reinterpret_cast<StringFromWide>(GetProcAddress(qt, "?fromWCharArray@QString@@SA?AV1@PEB_WH@Z"));
    destroy = reinterpret_cast<StringDestroy>(GetProcAddress(qt, "??1QString@@QEAA@XZ"));
    if (!fromWide || !destroy) return ERROR_PROC_NOT_FOUND;
    const auto base = reinterpret_cast<std::uintptr_t>(GetModuleHandleW(nullptr));
    slot = reinterpret_cast<void**>(base + input.slot);
    original = reinterpret_cast<Callback>(base + input.callback);
    configuration = input;
    HANDLE root = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, input.rootPid);
    FILETIME created{}, exited{}, kernel{}, user{};
    if (!root || !GetProcessTimes(root, &created, &exited, &kernel, &user)
        || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != input.rootCreation) {
        if (root) CloseHandle(root);
        return ERROR_INVALID_STATE;
    }
    HANDLE lifetime = CreateThread(nullptr, 0, [](void* value) -> DWORD {
        const auto root = static_cast<HANDLE>(value);
        WaitForSingleObject(root, INFINITE);
        CloseHandle(root);
        // This process contains only the owned destination test. It must never
        // survive root Stop, including during loading or after daemon exit.
        // ExitProcess runs DLL detach after killing the other threads, which
        // can deadlock on a lock they held. This disposable test process has
        // no Edit document and must exit even during native loading/teardown.
        TerminateProcess(GetCurrentProcess(), 0);
        return 0;
    }, root, 0, nullptr);
    if (!lifetime) { CloseHandle(root); return GetLastError(); }
    CloseHandle(lifetime);
    if (!Exchange(reinterpret_cast<void*>(original), reinterpret_cast<void*>(&Load))) return ERROR_INVALID_DATA;
    InterlockedExchange(&phase, 1);
    return ERROR_SUCCESS;
}

extern "C" __declspec(dllexport) DWORD ReniumPrepareTestServer(DWORD pid, const TestServerInputConfiguration* input) {
    HANDLE process = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION
        | PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE, FALSE, pid);
    if (!process) return GetLastError();
    HMODULE self = nullptr;
    DWORD error = ERROR_SUCCESS;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        reinterpret_cast<LPCWSTR>(&ReniumInitializeTestServer), &self)) error = GetLastError();
    wchar_t path[32768]{};
    if (!error && !GetModuleFileNameW(self, path, ARRAYSIZE(path))) error = GetLastError();
    const auto remote = error ? nullptr : remoteLaunchModule(pid, path);
    if (!error && !remote) error = ERROR_MOD_NOT_FOUND;
    if (!error) {
        const auto offset = reinterpret_cast<ULONG_PTR>(&ReniumInitializeTestServer) - reinterpret_cast<ULONG_PTR>(self);
        error = launchRemoteCall(process,
            reinterpret_cast<LPTHREAD_START_ROUTINE>(reinterpret_cast<ULONG_PTR>(remote) + offset), input, sizeof(*input));
    }
    CloseHandle(process);
    return error;
}

extern "C" __declspec(dllexport) DWORD ReniumTestRoute(DWORD pid, TestRouteRequest* input) {
    HANDLE process = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION
        | PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE, FALSE, pid);
    if (!process) return GetLastError();
    HMODULE self = nullptr;
    DWORD error = ERROR_SUCCESS;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        reinterpret_cast<LPCWSTR>(&ReniumHandleTestRoute), &self)) error = GetLastError();
    wchar_t path[32768]{};
    if (!error && !GetModuleFileNameW(self, path, ARRAYSIZE(path))) error = GetLastError();
    const auto module = error ? nullptr : remoteLaunchModule(pid, path);
    if (!error && !module) error = ERROR_MOD_NOT_FOUND;
    void* remote = nullptr;
    if (!error) {
        remote = VirtualAllocEx(process, nullptr, sizeof(*input), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if (!remote || !WriteProcessMemory(process, remote, input, sizeof(*input), nullptr)) error = GetLastError();
    }
    if (!error) {
        const auto offset = reinterpret_cast<ULONG_PTR>(&ReniumHandleTestRoute) - reinterpret_cast<ULONG_PTR>(self);
        HANDLE thread = CreateRemoteThread(process, nullptr, 0,
            reinterpret_cast<LPTHREAD_START_ROUTINE>(reinterpret_cast<ULONG_PTR>(module) + offset), remote, 0, nullptr);
        if (!thread) error = GetLastError();
        else {
            if (WaitForSingleObject(thread, 3000) == WAIT_OBJECT_0) {
                if (!GetExitCodeThread(thread, &error)) error = GetLastError();
                if (!error && !ReadProcessMemory(process, remote, input, sizeof(*input), nullptr)) error = GetLastError();
            } else { error = ERROR_TIMEOUT; remote = nullptr; }
            CloseHandle(thread);
        }
    }
    if (remote) VirtualFreeEx(process, remote, 0, MEM_RELEASE);
    CloseHandle(process);
    return error;
}
