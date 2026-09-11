#include <Windows.h>
#include <TlHelp32.h>
#include <CommCtrl.h>
#include <cstdio>

// Configuration is local to the selected Studio. No desktop-wide hook or
// shared DLL state is installed in another application's process.
static DWORD rootPid = 0;
static wchar_t rootExecutable[32768] = {};
static wchar_t tracePath[32768] = {};
static DWORD launchMonitor = 0;

extern "C" __declspec(dllexport) BOOL ReniumConfigureLaunch(DWORD pid)
{
    if (rootPid)
        return rootPid == pid;
    HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (!process)
        return FALSE;
    DWORD length = ARRAYSIZE(rootExecutable);
    BOOL result = QueryFullProcessImageNameW(process, 0, rootExecutable, &length);
    CloseHandle(process);
    if (result)
    {
        GetEnvironmentVariableW(L"RENIUM_TRACE_LAUNCH", tracePath, ARRAYSIZE(tracePath));
        wchar_t monitor[16]{};
        if (GetEnvironmentVariableW(L"RENIUM_STUDIO_MONITOR", monitor, ARRAYSIZE(monitor)))
            launchMonitor = wcstoul(monitor, nullptr, 10);
        rootPid = pid;
    }
    return result;
}

static bool belongsToLaunch()
{
    return rootPid && rootPid == GetCurrentProcessId();
}

static bool userActivation(const CBTACTIVATESTRUCT* activation)
{
    if (activation && activation->fMouse)
        return true;
    INPUT_MESSAGE_SOURCE source{};
    return GetCurrentInputMessageSource(&source)
        && source.deviceType != IMDT_UNAVAILABLE
        && source.originId != IMO_UNAVAILABLE;
}

static bool shouldStayInBackground()
{
    HWND foreground = GetForegroundWindow();
    DWORD process = 0;
    GetWindowThreadProcessId(foreground, &process);
    // GetForegroundWindow can be NULL during an activation transition. That
    // is not permission for a background launch to acquire the foreground.
    // Only an already active Studio or explicit user input may activate it.
    return process != GetCurrentProcessId();
}

static bool backgroundRequest()
{
    // Late dialogs and Play windows need the same protection as startup.
    // Keep it for this process's lifetime; explicit user activation and an
    // already focused Studio still pass through to the normal Windows APIs.
    return belongsToLaunch() && shouldStayInBackground() && !userActivation(nullptr);
}

static void traceActivation(const char* api, bool blocked)
{
    if (!tracePath[0]) return;
    DWORD foregroundPid = 0;
    GetWindowThreadProcessId(GetForegroundWindow(), &foregroundPid);
    char line[256];
    const int length = std::snprintf(line, sizeof(line),
        "{\"pid\":%lu,\"ms\":%llu,\"api\":\"%s\",\"foregroundPid\":%lu,\"blocked\":%s}\n",
        GetCurrentProcessId(), GetTickCount64(), api, foregroundPid, blocked ? "true" : "false");
    HANDLE file = CreateFileW(tracePath, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file != INVALID_HANDLE_VALUE)
    {
        DWORD written = 0;
        if (length > 0 && length < sizeof(line)) WriteFile(file, line, DWORD(length), &written, nullptr);
        CloseHandle(file);
    }
}

// Qt's requestActivateWindow attaches input queues and calls both foreground
// and keyboard-focus APIs. A CBT veto arrives after Windows has already taken
// the old application's input queue out of the foreground. Intercept these
// calls in Qt's import table, before user32 can affect the other application.
static BOOL WINAPI launchSetForegroundWindow(HWND window)
{
    const bool blocked = backgroundRequest();
    traceActivation("SetForegroundWindow", blocked);
    return blocked ? FALSE : SetForegroundWindow(window);
}

static HWND WINAPI launchSetFocus(HWND window)
{
    const bool blocked = backgroundRequest();
    traceActivation("SetFocus", blocked);
    return blocked ? GetFocus() : SetFocus(window);
}

static BOOL WINAPI launchAttachThreadInput(DWORD from, DWORD to, BOOL attach)
{
    const bool blocked = attach && backgroundRequest();
    traceActivation("AttachThreadInput", blocked);
    if (blocked) { SetLastError(ERROR_ACCESS_DENIED); return FALSE; }
    return AttachThreadInput(from, to, attach);
}

static constexpr wchar_t deferredShowProperty[] = L"Renium.BackgroundLaunch.ShowCommand";
static UINT deferredShowMessage()
{
    static const UINT message = RegisterWindowMessageW(L"Renium.BackgroundLaunch.ApplyWindowState");
    return message;
}

static BOOL WINAPI launchShowWindow(HWND window, int command)
{
    const bool blocked = command != SW_HIDE && backgroundRequest()
        && !(GetWindowLongPtrW(window, GWL_STYLE) & WS_CHILD);
    char operation[40];
    std::snprintf(operation, sizeof(operation), "ShowWindow:%d", command);
    traceActivation(operation, blocked);
    if (!blocked)
    {
        RemovePropW(window, deferredShowProperty);
        return ShowWindow(window, command);
    }
    // WS_EX_NOACTIVATE alone does not prevent an explicit ShowWindow command
    // from attempting activation. Use the API's nonactivating show modes.
    // Maximize/restore is applied only when the user actually activates Studio;
    // until then, keep the window's existing geometry and the user's input.
    if (command == SW_SHOWMAXIMIZED || command == SW_RESTORE)
        SetPropW(window, deferredShowProperty, reinterpret_cast<HANDLE>(INT_PTR(command + 1)));
    else if (command != SW_SHOWNA && command != SW_SHOWNOACTIVATE)
        RemovePropW(window, deferredShowProperty);
    const bool minimized = command == SW_SHOWMINIMIZED || command == SW_MINIMIZE || command == SW_FORCEMINIMIZE;
    return ShowWindow(window, minimized ? SW_SHOWMINNOACTIVE : SW_SHOWNA);
}

static void traceActivationStack(HANDLE file)
{
    void* frames[24]{};
    const USHORT count = CaptureStackBackTrace(0, ARRAYSIZE(frames), frames, nullptr);
    char line[4096];
    int used = std::snprintf(line, sizeof(line), "{\"pid\":%lu,\"ms\":%llu,\"stack\":[",
        GetCurrentProcessId(), GetTickCount64());
    for (USHORT index = 0; index < count && used > 0 && used < int(sizeof(line)) - 300; ++index)
    {
        HMODULE module = nullptr;
        char path[MAX_PATH]{};
        GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            reinterpret_cast<LPCSTR>(frames[index]), &module);
        if (module) GetModuleFileNameA(module, path, ARRAYSIZE(path));
        const char* name = path;
        for (const char* next = path; *next; ++next) if (*next == '\\' || *next == '/') name = next + 1;
        used += std::snprintf(line + used, sizeof(line) - used, "%s\"%s+%llx\"", index ? "," : "", name,
            static_cast<unsigned long long>(reinterpret_cast<ULONG_PTR>(frames[index]) - reinterpret_cast<ULONG_PTR>(module)));
    }
    if (used > 0 && used < int(sizeof(line)) - 4)
    {
        used += std::snprintf(line + used, sizeof(line) - used, "]}\n");
        DWORD written = 0;
        WriteFile(file, line, DWORD(used), &written, nullptr);
    }
}

static BOOL WINAPI launchSetWindowPos(HWND window, HWND after, int x, int y, int width, int height, UINT flags)
{
    if (backgroundRequest())
    {
        flags |= SWP_NOACTIVATE;
        if (!(flags & SWP_NOZORDER) && !(GetWindowLongPtrW(window, GWL_STYLE) & WS_CHILD))
            after = HWND_BOTTOM;
    }
    return SetWindowPos(window, after, x, y, width, height, flags);
}

static HWND WINAPI launchSetParent(HWND window, HWND parent)
{
    const DWORD previousError = GetLastError();
    const bool blocked = backgroundRequest();
    traceActivation("SetParent", blocked);
    // Qt reparents hidden dock windows during shutdown too. SetParent can
    // activate them internally before the CBT veto. Disable activation for
    // the native call without changing visibility, hierarchy or geometry.
    const LONG_PTR extended = GetWindowLongPtrW(window, GWL_EXSTYLE);
    if (blocked) SetWindowLongPtrW(window, GWL_EXSTYLE, extended | WS_EX_NOACTIVATE);
    SetLastError(previousError);
    HWND previous = SetParent(window, parent);
    const DWORD error = GetLastError();
    if (blocked) SetWindowLongPtrW(window, GWL_EXSTYLE, extended);
    SetLastError(error);
    return previous;
}

static BOOL WINAPI launchCreateProcessW(LPCWSTR, LPWSTR, LPSECURITY_ATTRIBUTES,
    LPSECURITY_ATTRIBUTES, BOOL, DWORD, LPVOID, LPCWSTR, LPSTARTUPINFOW, LPPROCESS_INFORMATION);
static HMODULE WINAPI launchLoadLibraryExW(LPCWSTR, HANDLE, DWORD);
extern "C" __declspec(dllexport) LRESULT CALLBACK ReniumLaunchHook(int, WPARAM, LPARAM);

static HWND WINAPI launchCreateWindowExW(DWORD extended, LPCWSTR className, LPCWSTR title, DWORD style,
    int x, int y, int width, int height, HWND parent, HMENU menu, HINSTANCE instance, LPVOID argument)
{
    // Install on the UI thread itself, before its first window exists. A
    // suspended main thread has no GUI queue yet, so remote registration is
    // too early. Passing NULL here uses no global hook-library registration.
    static thread_local HHOOK hook = nullptr;
    if (!hook && belongsToLaunch())
    {
        MSG message{};
        PeekMessageW(&message, nullptr, 0, 0, PM_NOREMOVE);
        hook = SetWindowsHookExW(WH_CBT, ReniumLaunchHook, nullptr, GetCurrentThreadId());
        if (!hook) return nullptr;
    }
    const bool deferShow = (style & WS_VISIBLE) && !(style & WS_CHILD) && backgroundRequest();
    HWND window = CreateWindowExW(extended, className, title, deferShow ? style & ~WS_VISIBLE : style,
        x, y, width, height, parent, menu, instance, argument);
    if (window && deferShow)
        launchShowWindow(window, (style & WS_MINIMIZE) ? SW_SHOWMINIMIZED :
            (style & WS_MAXIMIZE) ? SW_SHOWMAXIMIZED : SW_SHOWNORMAL);
    return window;
}

static bool bindActivationImports(HMODULE module)
{
    auto* image = reinterpret_cast<unsigned char*>(module);
    const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(image);
    if (dos->e_magic != IMAGE_DOS_SIGNATURE) return false;
    const auto* nt = reinterpret_cast<const IMAGE_NT_HEADERS*>(image + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE) return false;
    const auto& directory = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!directory.VirtualAddress) return false;
    struct Binding { void* original; void* replacement; };
    const Binding bindings[] = {
        { reinterpret_cast<void*>(&SetForegroundWindow), reinterpret_cast<void*>(&launchSetForegroundWindow) },
        { reinterpret_cast<void*>(&SetFocus), reinterpret_cast<void*>(&launchSetFocus) },
        { reinterpret_cast<void*>(&AttachThreadInput), reinterpret_cast<void*>(&launchAttachThreadInput) },
        { reinterpret_cast<void*>(&ShowWindow), reinterpret_cast<void*>(&launchShowWindow) },
        { reinterpret_cast<void*>(&SetWindowPos), reinterpret_cast<void*>(&launchSetWindowPos) },
        { reinterpret_cast<void*>(&SetParent), reinterpret_cast<void*>(&launchSetParent) },
        { reinterpret_cast<void*>(&CreateProcessW), reinterpret_cast<void*>(&launchCreateProcessW) },
        { reinterpret_cast<void*>(&LoadLibraryExW), reinterpret_cast<void*>(&launchLoadLibraryExW) },
        { reinterpret_cast<void*>(&CreateWindowExW), reinterpret_cast<void*>(&launchCreateWindowExW) },
    };
    // Only data pointers are changed, never executable system pages. Verify
    // each resolved import still points at the expected unmodified API.
    auto* imports = reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(image + directory.VirtualAddress);
    for (; imports->Name; ++imports)
    {
        auto* thunk = reinterpret_cast<IMAGE_THUNK_DATA*>(image + imports->FirstThunk);
        for (; thunk->u1.Function; ++thunk)
        {
            auto** slot = reinterpret_cast<void**>(&thunk->u1.Function);
            for (const auto& binding : bindings)
            {
                if (*slot != binding.original) continue;
                DWORD protection = 0;
                if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &protection)) return false;
                InterlockedCompareExchangePointer(slot, binding.replacement, binding.original);
                DWORD ignored = 0;
                if (!VirtualProtect(slot, sizeof(*slot), protection, &ignored)) return false;
                break;
            }
        }
    }
    return true;
}

static void protectQtActivation()
{
    static SRWLOCK lock = SRWLOCK_INIT;
    static bool installed = false;
    AcquireSRWLockExclusive(&lock);
    if (!installed)
    {
        // qwindows is loaded before it creates Studio's first native window.
        // Pin both modules because these IAT callbacks outlive the CBT hook.
        HMODULE qt = GetModuleHandleW(L"qwindows.dll"), self = nullptr;
        if (qt && GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            reinterpret_cast<LPCWSTR>(&launchSetForegroundWindow), &self)
            && GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, L"qwindows.dll", &qt))
            installed = bindActivationImports(qt);
    }
    ReleaseSRWLockExclusive(&lock);
}

static void placeOnRequestedMonitor(int& x, int& y, int width, int height)
{
    if (!launchMonitor) return;
    struct Selection { DWORD index = 0; RECT bounds{}; bool found = false; } selection;
    EnumDisplayMonitors(nullptr, nullptr, [](HMONITOR monitor, HDC, LPRECT, LPARAM value) -> BOOL {
        auto& selected = *reinterpret_cast<Selection*>(value);
        if (++selected.index != launchMonitor) return TRUE;
        MONITORINFO info{sizeof(info)};
        if (GetMonitorInfoW(monitor, &info)) { selected.bounds = info.rcMonitor; selected.found = true; }
        return FALSE;
    }, reinterpret_cast<LPARAM>(&selection));
    if (!selection.found) return;
    if (x == CW_USEDEFAULT || y == CW_USEDEFAULT)
    {
        x = selection.bounds.left + 32;
        y = selection.bounds.top + 32;
        return;
    }
    POINT point{x + (width > 0 ? width / 2 : 32), y + (height > 0 ? height / 2 : 32)};
    MONITORINFO source{sizeof(source)};
    if (GetMonitorInfoW(MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST), &source))
    {
        x += selection.bounds.left - source.rcMonitor.left;
        y += selection.bounds.top - source.rcMonitor.top;
    }
}

static LRESULT CALLBACK launchWindowProc(HWND window, UINT message, WPARAM wparam,
    LPARAM lparam, UINT_PTR id, DWORD_PTR)
{
    if (message == WM_ACTIVATE && LOWORD(wparam) != WA_INACTIVE
        && GetPropW(window, deferredShowProperty))
        PostMessageW(window, deferredShowMessage(), 0, 0);
    if (message == deferredShowMessage() && !shouldStayInBackground())
    {
        const HANDLE pending = RemovePropW(window, deferredShowProperty);
        if (pending) ShowWindow(window, int(reinterpret_cast<INT_PTR>(pending) - 1));
        return 0;
    }
    if (message == WM_WINDOWPOSCHANGING && backgroundRequest())
    {
        auto* position = reinterpret_cast<WINDOWPOS*>(lparam);
        position->flags |= SWP_NOACTIVATE;
        // A later Qt show/raise can reorder a window without activating it.
        // Keep the whole Studio window below other applications, including
        // their owned windows, rather than only refusing WM_ACTIVATE.
        if (!(position->flags & SWP_NOZORDER)) position->hwndInsertAfter = HWND_BOTTOM;
        if (!(position->flags & SWP_NOMOVE))
            placeOnRequestedMonitor(position->x, position->y, position->cx, position->cy);
    }
    if (message == WM_NCDESTROY)
    {
        RemovePropW(window, deferredShowProperty);
        RemoveWindowSubclass(window, launchWindowProc, id);
    }
    return DefSubclassProc(window, message, wparam, lparam);
}

extern "C" __declspec(dllexport) LRESULT CALLBACK ReniumLaunchHook(
    int code, WPARAM wparam, LPARAM lparam)
{
    if (code == HCBT_ACTIVATE || code == HCBT_CREATEWND)
    {
        static const bool protectedProcess = belongsToLaunch();
        if (protectedProcess)
        {
            protectQtActivation();
            if (code == HCBT_ACTIVATE)
            {
                const auto* activation = reinterpret_cast<const CBTACTIVATESTRUCT*>(lparam);
                const bool blocked = !userActivation(activation) && shouldStayInBackground();
                if (tracePath[0])
                {
                    INPUT_MESSAGE_SOURCE source{};
                    GetCurrentInputMessageSource(&source);
                    DWORD foregroundPid = 0;
                    GetWindowThreadProcessId(GetForegroundWindow(), &foregroundPid);
                    char line[256];
                    const int length = std::snprintf(line, sizeof(line),
                        "{\"pid\":%lu,\"ms\":%llu,\"mouse\":%d,\"device\":%u,\"origin\":%u,\"foregroundPid\":%lu,\"blocked\":%s}\n",
                        GetCurrentProcessId(), GetTickCount64(), activation ? activation->fMouse : 0,
                        unsigned(source.deviceType), unsigned(source.originId), foregroundPid, blocked ? "true" : "false");
                    HANDLE file = CreateFileW(tracePath, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
                        nullptr, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
                    if (file != INVALID_HANDLE_VALUE)
                    {
                        DWORD written = 0;
                        if (length > 0 && length < sizeof(line))
                            WriteFile(file, line, DWORD(length), &written, nullptr);
                        if (blocked) traceActivationStack(file);
                        CloseHandle(file);
                    }
                }
                if (blocked)
                    return 1;
            }
            else
            {
                auto* creation = reinterpret_cast<CBT_CREATEWND*>(lparam);
                if (!(creation->lpcs->style & WS_CHILD))
                {
                    // Subclasses can outlive the external hook. Keep their
                    // callback image mapped until this Studio process exits.
                    HMODULE module = nullptr;
                    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                        reinterpret_cast<LPCWSTR>(&launchWindowProc), &module))
                        SetWindowSubclass(reinterpret_cast<HWND>(wparam), launchWindowProc, 1, 0);
                    if (shouldStayInBackground()) creation->hwndInsertAfter = HWND_BOTTOM;
                    placeOnRequestedMonitor(creation->lpcs->x, creation->lpcs->y,
                        creation->lpcs->cx, creation->lpcs->cy);
                }
            }
        }
    }
    return CallNextHookEx(nullptr, code, wparam, lparam);
}

#include "renium_launch_windows_process.h"
