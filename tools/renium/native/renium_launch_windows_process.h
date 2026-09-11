// Load the guard into an explicitly selected process before its main thread
// runs. Local CBT hooks use no system-wide hook DLL slots. Child creation is
// protected at CreateProcessW, before any child window can take input focus.
struct LaunchConfiguration
{
    DWORD monitor = 0;
    wchar_t trace[32768]{};
};

extern "C" __declspec(dllexport) DWORD WINAPI ReniumInitializeLaunch(void* argument);

static HMODULE remoteLaunchModule(DWORD pid, const wchar_t* path)
{
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
    if (snapshot == INVALID_HANDLE_VALUE) return nullptr;
    MODULEENTRY32W entry{sizeof(entry)};
    HMODULE module = nullptr;
    if (Module32FirstW(snapshot, &entry))
        do { if (_wcsicmp(entry.szExePath, path) == 0) { module = entry.hModule; break; } }
        while (Module32NextW(snapshot, &entry));
    CloseHandle(snapshot);
    return module;
}

static DWORD launchRemoteCall(HANDLE process, LPTHREAD_START_ROUTINE function,
    const void* data, SIZE_T size)
{
    void* remote = VirtualAllocEx(process, nullptr, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!remote) return GetLastError();
    DWORD error = ERROR_SUCCESS;
    if (!WriteProcessMemory(process, remote, data, size, nullptr)) error = GetLastError();
    else
    {
        HANDLE thread = CreateRemoteThread(process, nullptr, 0, function, remote, 0, nullptr);
        if (!thread) error = GetLastError();
        else
        {
            const DWORD waited = WaitForSingleObject(thread, 10000);
            if (waited != WAIT_OBJECT_0)
            {
                // A live remote thread can still read its argument. Do not
                // release that memory or terminate an existing Studio thread.
                CloseHandle(thread);
                return waited == WAIT_TIMEOUT ? ERROR_TIMEOUT : GetLastError();
            }
            if (!GetExitCodeThread(thread, &error)) error = GetLastError();
            CloseHandle(thread);
        }
    }
    VirtualFreeEx(process, remote, 0, MEM_RELEASE);
    return error;
}

static DWORD protectLaunchProcess(HANDLE process, DWORD pid, const LaunchConfiguration& configuration)
{
    HMODULE self = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        reinterpret_cast<LPCWSTR>(&ReniumInitializeLaunch), &self)) return GetLastError();
    wchar_t path[32768]{};
    if (!GetModuleFileNameW(self, path, ARRAYSIZE(path))) return GetLastError();
    HMODULE remote = remoteLaunchModule(pid, path);
    if (!remote)
    {
        // Kernel32's loader export has a shared address in same-bitness
        // processes in this Windows boot, including a suspended new process.
        const auto load = reinterpret_cast<LPTHREAD_START_ROUTINE>(GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "LoadLibraryW"));
        if (!load) return GetLastError();
        const DWORD result = launchRemoteCall(process, load, path, (wcslen(path) + 1) * sizeof(wchar_t));
        remote = remoteLaunchModule(pid, path);
        if (!remote) return result == ERROR_TIMEOUT ? result : ERROR_DLL_INIT_FAILED;
    }
    const auto offset = reinterpret_cast<ULONG_PTR>(&ReniumInitializeLaunch) - reinterpret_cast<ULONG_PTR>(self);
    return launchRemoteCall(process,
        reinterpret_cast<LPTHREAD_START_ROUTINE>(reinterpret_cast<ULONG_PTR>(remote) + offset),
        &configuration, sizeof(configuration));
}

extern "C" __declspec(dllexport) DWORD ReniumProtectLaunch(DWORD pid)
{
    HANDLE process = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION
        | PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE, FALSE, pid);
    if (!process) return GetLastError();
    LaunchConfiguration configuration{};
    wchar_t monitor[16]{};
    if (GetEnvironmentVariableW(L"RENIUM_STUDIO_MONITOR", monitor, ARRAYSIZE(monitor)))
        configuration.monitor = wcstoul(monitor, nullptr, 10);
    GetEnvironmentVariableW(L"RENIUM_TRACE_LAUNCH", configuration.trace, ARRAYSIZE(configuration.trace));
    const DWORD error = protectLaunchProcess(process, pid, configuration);
    CloseHandle(process);
    return error;
}

static bool launchedStudio(LPCWSTR executable, LPCWSTR command)
{
    wchar_t candidate[32768]{};
    if (executable) wcsncpy_s(candidate, executable, _TRUNCATE);
    else if (command)
    {
        const bool quoted = *command == L'"';
        if (quoted) ++command;
        size_t length = 0;
        while (command[length] && command[length] != (quoted ? L'"' : L' ') && length + 1 < ARRAYSIZE(candidate)) ++length;
        wcsncpy_s(candidate, command, length);
    }
    return _wcsicmp(candidate, rootExecutable) == 0;
}

static BOOL WINAPI launchCreateProcessW(LPCWSTR executable, LPWSTR command, LPSECURITY_ATTRIBUTES processAttributes,
    LPSECURITY_ATTRIBUTES threadAttributes, BOOL inherit, DWORD flags, LPVOID environment, LPCWSTR directory,
    LPSTARTUPINFOW startup, LPPROCESS_INFORMATION process)
{
    const bool protect = belongsToLaunch() && launchedStudio(executable, command);
    if (!protect) return CreateProcessW(executable, command, processAttributes, threadAttributes, inherit,
        flags, environment, directory, startup, process);
    STARTUPINFOEXW background{};
    background.StartupInfo = *startup;
    if (flags & EXTENDED_STARTUPINFO_PRESENT)
        background.lpAttributeList = reinterpret_cast<STARTUPINFOEXW*>(startup)->lpAttributeList;
    background.StartupInfo.dwFlags |= STARTF_USESHOWWINDOW;
    background.StartupInfo.wShowWindow = SW_SHOWNOACTIVATE;
    if (!CreateProcessW(executable, command, processAttributes, threadAttributes, inherit,
        flags | CREATE_SUSPENDED, environment, directory, &background.StartupInfo, process)) return FALSE;
    LaunchConfiguration configuration{};
    configuration.monitor = launchMonitor;
    wcscpy_s(configuration.trace, tracePath);
    DWORD error = protectLaunchProcess(process->hProcess, process->dwProcessId, configuration);
    if (!error && !(flags & CREATE_SUSPENDED) && ResumeThread(process->hThread) == DWORD(-1)) error = GetLastError();
    if (error)
    {
        TerminateProcess(process->hProcess, 1);
        CloseHandle(process->hThread); CloseHandle(process->hProcess); *process = {};
        SetLastError(error); return FALSE;
    }
    return TRUE;
}

static HMODULE WINAPI launchLoadLibraryExW(LPCWSTR path, HANDLE file, DWORD flags)
{
    HMODULE module = LoadLibraryExW(path, file, flags);
    if (module && !(flags & (LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE | LOAD_LIBRARY_AS_IMAGE_RESOURCE)))
        protectQtActivation();
    return module;
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumInitializeLaunch(void* argument)
{
    static SRWLOCK lock = SRWLOCK_INIT;
    AcquireSRWLockExclusive(&lock);
    const auto& configuration = *static_cast<const LaunchConfiguration*>(argument);
    const bool first = rootPid == 0;
    ReniumConfigureLaunch(GetCurrentProcessId());
    if (first)
    {
        launchMonitor = configuration.monitor;
        wcscpy_s(tracePath, configuration.trace);
    }
    // qwindows loads later during a fresh launch. QtCore's library-loading
    // import arms its activation imports immediately when loading finishes.
    bool bound = bindActivationImports(GetModuleHandleW(nullptr));
    const wchar_t* modules[] = {L"Qt5Core.dll", L"Qt5Gui.dll"};
    for (const auto* name : modules)
        if (HMODULE module = GetModuleHandleW(name)) bound = bindActivationImports(module) && bound;
    protectQtActivation();
    const DWORD error = bound ? ERROR_SUCCESS : ERROR_INVALID_FUNCTION;
    ReleaseSRWLockExclusive(&lock);
    return error;
}
