#pragma once
#include <Windows.h>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <unordered_set>

namespace renium_play_loader {
constexpr std::uint32_t Protocol = 4;
constexpr std::uint64_t MaxBytes = 128ull * 1024 * 1024;
enum Phase : std::uint32_t { Armed = 1, Serializing = 2, Consumed = 3, Cancelled = 4, Failed = 5 };

// All engine fields are discovered from the source callback's data flow.
struct Fields { std::uint64_t capture, string, place, universe; };
struct Request {
    std::uint32_t version, size, action, phase;
    std::uint64_t token, slot, original, sourceModel, sourcePlace, sourceUniverse;
    std::uint64_t destinationPlace, destinationUniverse;
    Fields fields;
    std::uint64_t bytes, length, hostCreation;
    std::uint32_t hostPid, lifetimeMs;
    char error[256];
    std::uint64_t getAttribute, stringTypeTable, directModel, sourceInstance;
    char launchNonce[128];
};
static_assert(sizeof(Request) == 560);
using Callback = void(*)(void*, void*);

struct Lease {
    Request request{};
    std::string payload;
    HANDLE host = nullptr;
    HANDLE finished = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    std::uint64_t deadline = 0;
    std::uint32_t phase = Armed;
    std::string error;
    ~Lease() { if (host) CloseHandle(host); if (finished) CloseHandle(finished); }
    bool Alive() const {
        return GetTickCount64() < deadline && host && WaitForSingleObject(host, 0) == WAIT_TIMEOUT;
    }
};
inline std::mutex mutex;
inline std::shared_ptr<Lease> current;
inline std::unordered_set<std::uint64_t> cancelled;
// The executable and this helper stay mapped for the process lifetime. A
// thread that has already read the wrapper address can safely finish later.
inline std::atomic<Callback> original{nullptr};

inline bool Exchange(std::uint64_t slot, void* expected, void* replacement) {
    DWORD protection = 0;
    auto address = reinterpret_cast<void**>(slot);
    if (!VirtualProtect(address, sizeof(void*), PAGE_READWRITE, &protection))
        throw std::runtime_error("Cannot change the owned Play callback protection");
    const bool exchanged = InterlockedCompareExchangePointer(address, replacement, expected) == expected;
    DWORD ignored = 0;
    if (!VirtualProtect(address, sizeof(void*), protection, &ignored))
        throw std::runtime_error("Cannot restore the owned Play callback protection");
    return exchanged;
}

inline void CallbackEntry(void* capture, void* argument);

inline void Restore(const Lease& lease) {
    if (current && current.get() != &lease) return;
    const auto slot = reinterpret_cast<void**>(lease.request.slot);
    if (*slot == reinterpret_cast<void*>(&CallbackEntry)
        && !Exchange(lease.request.slot, reinterpret_cast<void*>(&CallbackEntry), reinterpret_cast<void*>(lease.request.original)))
        throw std::runtime_error("Play callback changed during restoration");
}

inline std::uint64_t Word(void* pointer, std::uint64_t offset) {
    std::uint64_t value = 0;
    std::memcpy(&value, static_cast<const char*>(pointer) + offset, sizeof(value));
    return value;
}

inline bool SavedString(const std::string& value) {
    return value.size() >= 14 && value.size() <= MaxBytes
        && value.capacity() >= value.size()
        && std::memcmp(value.data(), "<roblox", 7) == 0;
}

// Reflection::Variant's native return buffer and payload manager ABI. Its
// manager destroys the engine-owned value, including unexpected value types.
struct alignas(16) VariantBuffer {
    std::uintptr_t type = 0;
    std::uintptr_t operations = 0;
    unsigned char payload[64]{};
    VariantBuffer() = default;
    VariantBuffer(const VariantBuffer& other) : type(other.type) {
        if (other.operations) {
            using Copy = void(*)(const void*, void*);
            reinterpret_cast<Copy*>(other.operations)[0](other.payload, payload);
            operations = other.operations;
        }
    }
    VariantBuffer(VariantBuffer&& other) : VariantBuffer(static_cast<const VariantBuffer&>(other)) {}
    VariantBuffer& operator=(const VariantBuffer&) = delete;
    ~VariantBuffer() {
        if (operations) {
            using Destroy = void(*)(void*);
            auto destroy = reinterpret_cast<Destroy*>(operations)[2];
            destroy(payload);
        }
    }
};
static_assert(sizeof(VariantBuffer) == 80);
struct StringView { const char* data; std::size_t size; };

inline bool MatchesLaunch(const Request& r) {
    using Attribute = void*(*)(void*, void*, StringView);
    VariantBuffer value;
    constexpr char name[] = "__ReniumLaunchNonce";
    reinterpret_cast<Attribute>(r.getAttribute)(reinterpret_cast<void*>(r.sourceInstance), &value, StringView{name, sizeof(name) - 1});
    if (!value.type || Word(reinterpret_cast<void*>(value.type), 0) != r.stringTypeTable) return false;
    return *reinterpret_cast<const std::string*>(value.payload) == r.launchNonce;
}

inline void CallbackEntry(void* capture, void* argument) {
    std::shared_ptr<Lease> lease;
    const auto forward = original.load(std::memory_order_acquire);
    {
        std::lock_guard lock(mutex);
        const auto model = !current || !argument ? nullptr : current->request.directModel
            ? argument : *static_cast<void**>(argument);
        if (current && current->phase == Armed && model == reinterpret_cast<void*>(current->request.sourceModel)) {
            lease = current;
            lease->phase = lease->Alive() ? Serializing : Cancelled;
        }
    }
    // Preserve the verified pointer/reference argument when forwarding.
    // No host lock or thread-local ownership spans an engine call/fiber yield.
    try { forward(capture, argument); }
    catch (...) {
        if (lease) {
            std::lock_guard lock(mutex);
            lease->phase = Failed;
            lease->error = "Source serialization failed before destination replacement";
            try { Restore(*lease); } catch (...) {}
            SetEvent(lease->finished);
        }
        throw;
    }
    if (!lease) return;
    bool matches = false;
    std::string checkError;
    try { matches = MatchesLaunch(lease->request); }
    catch (const std::exception& error) { checkError = error.what(); }
    std::lock_guard lock(mutex);
    if (lease->phase == Serializing && !lease->Alive()) lease->phase = Cancelled;
    if (lease->phase == Serializing) {
        const auto& r = lease->request;
        auto config = reinterpret_cast<void*>(Word(capture, r.fields.capture));
        auto& bytes = *reinterpret_cast<std::string*>(static_cast<char*>(config) + r.fields.string);
        if (!matches) {
            lease->phase = Cancelled;
            lease->error = checkError.empty() ? "The Play launch was cancelled or replaced before serialization completed" : checkError;
        } else if (Word(config, r.fields.place) != r.sourcePlace || Word(config, r.fields.universe) != r.sourceUniverse || !SavedString(bytes)) {
            lease->phase = Failed;
            lease->error = "The owned source loader input changed; no destination was installed";
        } else {
            // payload was allocated before arming, using Studio's imported
            // allocator. Swap supports larger destinations and preserves the
            // engine's move/destructor ownership across asynchronous loading.
            bytes.swap(lease->payload);
            std::memcpy(static_cast<char*>(config) + r.fields.place, &r.destinationPlace, sizeof(r.destinationPlace));
            std::memcpy(static_cast<char*>(config) + r.fields.universe, &r.destinationUniverse, sizeof(r.destinationUniverse));
            lease->phase = Consumed;
        }
    }
    try { Restore(*lease); }
    catch (const std::exception& error) { lease->error = error.what(); }
    // The original source clone is no longer needed. This uses the same
    // allocator as the executable, including its large-string alignment.
    std::string{}.swap(lease->payload);
    SetEvent(lease->finished);
}

inline void Handle(Request& request) {
    if (request.version != Protocol || request.size != sizeof(Request) || !request.token)
        throw std::runtime_error("Incompatible Play loader protocol; restart Studio with this Renium build");
    if (request.action == 1) {
        const auto& f = request.fields;
        if (!request.slot || request.slot % 8 || !request.original || !request.sourceModel
            || !request.getAttribute || !request.stringTypeTable || request.directModel > 1 || !request.sourceInstance
            || !request.launchNonce[0] || request.launchNonce[127]
            || !request.sourcePlace || !request.sourceUniverse || !request.destinationPlace
            || request.sourceUniverse != request.destinationUniverse
            || !request.bytes || request.length < 14 || request.length > MaxBytes
            || request.lifetimeMs < 1 || request.lifetimeMs > 30000
            || f.capture > 65536 || f.string > 65536 || f.place > 65536 || f.universe > 65536
            || f.capture % 8 || f.string % 8 || f.place % 8 || f.universe % 8
            || f.place < f.string + sizeof(std::string) || f.universe < f.string + sizeof(std::string) || f.place == f.universe)
            throw std::runtime_error("Invalid Play loader input");
        auto lease = std::make_shared<Lease>();
        if (!lease->finished) throw std::runtime_error("Cannot create Play loader completion event");
        lease->request = request;
        lease->payload.assign(reinterpret_cast<const char*>(request.bytes), static_cast<std::size_t>(request.length));
        if (!SavedString(lease->payload)) throw std::runtime_error("Destination is not saved place content");
        lease->host = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, request.hostPid);
        FILETIME created{}, exited{}, kernel{}, user{};
        if (!lease->host || !GetProcessTimes(lease->host, &created, &exited, &kernel, &user)
            || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != request.hostCreation)
            throw std::runtime_error("Play loader owner process changed");
        lease->deadline = GetTickCount64() + request.lifetimeMs;
        std::lock_guard lock(mutex);
        if (cancelled.contains(request.token)) throw std::runtime_error("Play loader request was cancelled before arming");
        if (current && (current->phase == Armed || current->phase == Serializing))
            throw std::runtime_error("The source Play loader already has an owner");
        const auto bound = original.load(std::memory_order_acquire);
        if (bound && bound != reinterpret_cast<Callback>(request.original))
            throw std::runtime_error("The Play callback binding changed in this process");
        original.store(reinterpret_cast<Callback>(request.original), std::memory_order_release);
        current = lease;
        try {
            if (!Exchange(request.slot, reinterpret_cast<void*>(request.original), reinterpret_cast<void*>(&CallbackEntry)))
                throw std::runtime_error("Another owner replaced the Play callback");
            std::thread([lease] {
                const HANDLE handles[] = {lease->finished, lease->host};
                const auto now = GetTickCount64();
                const auto timeout = static_cast<DWORD>(lease->deadline > now ? lease->deadline - now : 0);
                if (WaitForMultipleObjects(2, handles, FALSE, timeout) == WAIT_OBJECT_0) return;
                std::lock_guard lock(mutex);
                if (lease->phase == Armed || lease->phase == Serializing) {
                    lease->phase = Cancelled;
                    try { Restore(*lease); }
                    catch (const std::exception& error) { lease->error = error.what(); }
                    std::string{}.swap(lease->payload);
                    SetEvent(lease->finished);
                }
            }).detach();
        } catch (...) {
            lease->phase = Cancelled;
            Restore(*lease);
            throw;
        }
    } else if (request.action != 2 && request.action != 3) {
        throw std::runtime_error("Unknown Play loader operation");
    }
    std::lock_guard lock(mutex);
    if (request.action == 2) {
        cancelled.insert(request.token);
        if (!current || current->request.token != request.token) { request.phase = Cancelled; return; }
    }
    if (!current || current->request.token != request.token)
        throw std::runtime_error("The Play loader lease was replaced");
    if ((request.action == 2 || !current->Alive()) && (current->phase == Armed || current->phase == Serializing)) {
        current->phase = Cancelled;
        Restore(*current);
        std::string{}.swap(current->payload);
        SetEvent(current->finished);
    }
    request.phase = current->phase;
    strncpy_s(request.error, current->error.c_str(), _TRUNCATE);
}
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumPlayLoader(void* input) {
    auto& request = *static_cast<renium_play_loader::Request*>(input);
    try { renium_play_loader::Handle(request); return 0; }
    catch (const std::exception& error) {
        strncpy_s(request.error, error.what(), _TRUNCATE);
        return 1;
    }
    catch (...) { strcpy_s(request.error, "Unknown native loader failure"); return 1; }
}
