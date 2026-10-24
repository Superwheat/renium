#pragma once
#include <map>

// A process-local cleanup lease survives the daemon that writes the caption.
// It only restores the exact text that daemon last owned; no activation or input.
namespace renium_play_caption {
struct Request {
    std::uint32_t version, size, action, hostPid;
    std::uint64_t token, window, hostCreation;
    wchar_t ordinary[2048], written[2048];
    char error[256];
};
static_assert(sizeof(Request) == 8488);
struct Lease {
    HANDLE host = nullptr;
    HWND window = nullptr;
    std::uint64_t token = 0;
    bool active = true;
    std::wstring ordinary, written;
    ~Lease() { if (host) CloseHandle(host); }
};
inline std::mutex mutex;
inline std::map<HWND, std::shared_ptr<Lease>> leases;

inline void Restore(const std::shared_ptr<Lease>& lease) {
    std::wstring ordinary, written;
    {
        std::lock_guard lock(mutex);
        auto found = leases.find(lease->window);
        if (found == leases.end() || found->second != lease || !lease->active) return;
        lease->active = false;
        ordinary = lease->ordinary; written = lease->written;
    }
    DWORD pid = 0;
    GetWindowThreadProcessId(lease->window, &pid);
    wchar_t title[2048]{};
    if (pid == GetCurrentProcessId() && GetWindowTextW(lease->window, title, 2048)
        && !written.empty() && title == written) {
        // This dedicated cleanup thread owns the string until the UI handles
        // the synchronous message. A busy UI cannot block another root or API.
        SetWindowTextW(lease->window, ordinary.c_str());
    }
    std::lock_guard lock(mutex);
    auto found = leases.find(lease->window);
    if (found != leases.end() && found->second == lease) leases.erase(found);
}

inline void Handle(Request& r) {
    if (r.version != 1 || r.size != sizeof(r) || !r.token || !r.window)
        throw std::runtime_error("Incompatible teleport caption protocol; restart Studio");
    const auto window = reinterpret_cast<HWND>(r.window);
    std::lock_guard lock(mutex);
    auto found = leases.find(window);
    if (r.action == 3) {
        if (found != leases.end() && found->second->token == r.token) {
            found->second->active = false;
            leases.erase(found);
        }
        return;
    }
    if (r.action != 1 && r.action != 2) throw std::runtime_error("Unknown teleport caption operation");
    DWORD pid = 0;
    GetWindowThreadProcessId(window, &pid);
    if (pid != GetCurrentProcessId() || r.ordinary[2047] || r.written[2047] || !r.ordinary[0])
        throw std::runtime_error("Teleport caption window changed");
    if (r.action == 2) {
        if (found == leases.end() || found->second->token != r.token || !found->second->active)
            throw std::runtime_error("Teleport caption owner ended");
        found->second->ordinary = r.ordinary; found->second->written = r.written;
        return;
    }
    if (found != leases.end()) throw std::runtime_error("Teleport caption already has an owner");
    auto lease = std::make_shared<Lease>();
    lease->window = window; lease->token = r.token;
    lease->ordinary = r.ordinary; lease->written = r.written;
    lease->host = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, r.hostPid);
    FILETIME created{}, exited{}, kernel{}, user{};
    if (!lease->host || !GetProcessTimes(lease->host, &created, &exited, &kernel, &user)
        || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != r.hostCreation)
        throw std::runtime_error("Teleport caption host changed");
    leases.emplace(window, lease);
    try {
        std::thread([lease] {
            while (WaitForSingleObject(lease->host, 100) == WAIT_TIMEOUT) {
                std::lock_guard lock(mutex);
                if (!lease->active) return;
            }
            Restore(lease);
        }).detach();
    } catch (...) { leases.erase(window); throw; }
}
}
extern "C" __declspec(dllexport) DWORD WINAPI ReniumPlayCaption(void* input) {
    auto& request = *static_cast<renium_play_caption::Request*>(input);
    try { renium_play_caption::Handle(request); return 0; }
    catch (const std::exception& error) { strncpy_s(request.error, error.what(), _TRUNCATE); return 1; }
}
