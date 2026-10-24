#pragma once
#include <Shellapi.h>
#pragma comment(lib, "shell32.lib")
#include <cstdint>

// AddPlayers supplies Studio's current transport endpoint and registers the
// arriving client with the destination controller. Reuse its existing process
// instead of creating another window. This hook is armed for one owned client.
struct TestRouteRequest {
    std::uint32_t version, size, action, clientPid;
    std::uint64_t clientCreation, window, token;
    std::uint32_t state, error;
    wchar_t command[32768];
};
static_assert(sizeof(TestRouteRequest) == 65584);
namespace renium_test_route {
inline SRWLOCK lock = SRWLOCK_INIT;
inline TestRouteRequest current{};
inline ULONGLONG deadline = 0;
inline bool Capture(LPCWSTR command, PROCESS_INFORMATION* process) {
    AcquireSRWLockExclusive(&lock);
    if (current.state != 1 || GetTickCount64() >= deadline) { ReleaseSRWLockExclusive(&lock); return false; }
    int count = 0;
    auto args = CommandLineToArgvW(command, &count);
    bool client = false;
    for (int i = 1; args && i + 1 < count; ++i)
        if (!_wcsicmp(args[i], L"-task") && !_wcsicmp(args[i + 1], L"StartClient")) client = true;
    if (args) LocalFree(args);
    if (!client) { ReleaseSRWLockExclusive(&lock); return false; }
    DWORD pid = 0;
    const DWORD threadId = GetWindowThreadProcessId(reinterpret_cast<HWND>(current.window), &pid);
    HANDLE handle = OpenProcess(PROCESS_ALL_ACCESS, FALSE, current.clientPid);
    FILETIME created{}, exited{}, kernel{}, user{};
    bool owned = handle && pid == current.clientPid && threadId
        && GetProcessTimes(handle, &created, &exited, &kernel, &user)
        && (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) == current.clientCreation
        && WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
    HANDLE thread = owned ? OpenThread(THREAD_ALL_ACCESS, FALSE, threadId) : nullptr;
    if (!thread || !owned || wcslen(command) >= ARRAYSIZE(current.command)) {
        if (thread) CloseHandle(thread);
        if (handle) CloseHandle(handle);
        current.state = 3; current.error = ERROR_INVALID_STATE;
        *process = {};
    } else {
        wcscpy_s(current.command, command);
        *process = {handle, thread, current.clientPid, threadId};
        current.state = 2;
    }
    ReleaseSRWLockExclusive(&lock);
    return true;
}
}
extern "C" __declspec(dllexport) DWORD WINAPI ReniumHandleTestRoute(void* argument) {
    using namespace renium_test_route;
    auto& r = *static_cast<TestRouteRequest*>(argument);
    if (r.version != 1 || r.size != sizeof(r) || !r.token) return ERROR_INVALID_PARAMETER;
    AcquireSRWLockExclusive(&lock);
    DWORD error = ERROR_SUCCESS;
    if (r.action == 1) {
        if (current.state == 1 && GetTickCount64() < deadline) error = ERROR_BUSY;
        else { current = r; current.state = 1; current.error = 0; current.command[0] = 0; deadline = GetTickCount64() + 15000; }
    } else if (current.token != r.token) error = ERROR_INVALID_STATE;
    else if (r.action == 2) { r = current; }
    else if (r.action == 3) { current = {}; }
    else error = ERROR_INVALID_PARAMETER;
    ReleaseSRWLockExclusive(&lock);
    return error;
}
