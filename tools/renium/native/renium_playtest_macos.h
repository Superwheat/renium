#pragma once
#include <condition_variable>
#include <libproc.h>
#include <sys/proc_info.h>
#include <thread>

// The host discovers and verifies the executable contracts. This boundary owns
// libc++ values, callback lifetime, and the process-local pointer exchange.
namespace renium_mac_playtest {
using Clock = std::chrono::steady_clock;
struct Fields { std::uint64_t capture, string, place, universe; };
struct LoadRequest {
    std::uint32_t version, size, action, phase;
    std::uint64_t token, slot, original, sourceOuter, sourceInstance, sourceOwner;
    std::uint64_t sourcePlace, sourceUniverse, destinationPlace, destinationUniverse;
    Fields fields;
    std::uint64_t getAttribute, stringTypeTable;
    std::uint32_t hostPid, lifetimeMs;
    std::uint64_t hostSeconds, hostMicros;
    char nonce[128];
};
static_assert(sizeof(LoadRequest) == 296);

struct alignas(16) Variant {
    std::uintptr_t type = 0, operations = 0;
    unsigned char payload[64]{};
    Variant() = default;
    Variant(const Variant& other) : type(other.type) {
        if (other.operations) {
            reinterpret_cast<void(**)(const void*, void*)>(other.operations)[0](other.payload, payload);
            operations = other.operations;
        }
    }
    Variant(Variant&& other) : Variant(static_cast<const Variant&>(other)) {}
    Variant& operator=(const Variant&) = delete;
    ~Variant() { if (operations) reinterpret_cast<void(**)(void*)>(operations)[2](payload); }
};
static_assert(sizeof(Variant) == 80);
struct StringView { const char* data; std::size_t size; };

inline std::uint64_t Word(const void* object, std::uint64_t offset) {
    std::uint64_t result = 0;
    std::memcpy(&result, static_cast<const unsigned char*>(object) + offset, sizeof(result));
    return result;
}

inline bool HostAlive(const LoadRequest& r) {
    proc_bsdinfo info{};
    return proc_pidinfo(r.hostPid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info)
        && info.pbi_uid == getuid() && info.pbi_start_tvsec == r.hostSeconds && info.pbi_start_tvusec == r.hostMicros;
}

inline bool Exchange(std::uintptr_t slot, void* expected, void* replacement) {
    mach_vm_address_t region = slot;
    mach_vm_size_t length = 0;
    vm_region_basic_info_data_64_t info{};
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_port_t object = MACH_PORT_NULL;
    if (mach_vm_region(mach_task_self(), &region, &length, VM_REGION_BASIC_INFO_64,
            reinterpret_cast<vm_region_info_t>(&info), &count, &object) != KERN_SUCCESS)
        throw std::runtime_error("Play callback memory ended");
    if (object) mach_port_deallocate(mach_task_self(), object);
    if (slot < region || slot + sizeof(void*) > region + length || slot % alignof(void*))
        throw std::runtime_error("Invalid Play callback slot");
    const auto pageSize = sysconf(_SC_PAGESIZE);
    if (pageSize <= 0) throw std::runtime_error("Cannot resolve Mac page size");
    const auto page = slot & ~(static_cast<std::uintptr_t>(pageSize) - 1);
    const bool writable = info.protection & VM_PROT_WRITE;
    if (!writable && mach_vm_protect(mach_task_self(), page, pageSize, false,
            info.protection | VM_PROT_WRITE | VM_PROT_COPY) != KERN_SUCCESS)
        throw std::runtime_error("Cannot make the owned Play callback writable");
    const bool changed = __atomic_compare_exchange_n(reinterpret_cast<void**>(slot), &expected,
        replacement, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE);
    if (!writable && mach_vm_protect(mach_task_self(), page, pageSize, false, info.protection) != KERN_SUCCESS)
        throw std::runtime_error("Cannot restore Play callback protection");
    return changed;
}

inline bool MatchesLaunch(const LoadRequest& r) {
    constexpr char key[] = "__ReniumLaunchNonce";
    using Get = Variant(*)(void*, StringView);
    const auto value = reinterpret_cast<Get>(r.getAttribute)(reinterpret_cast<void*>(r.sourceInstance), {key, sizeof(key) - 1});
    return value.type && Word(reinterpret_cast<void*>(value.type), 0) == r.stringTypeTable
        && *reinterpret_cast<const std::string*>(value.payload) == r.nonce;
}

using Producer = void(*)(void*, void**);
enum Phase : std::uint32_t { Armed = 1, Serializing = 2, Consumed = 3, Cancelled = 4, Failed = 5 };
struct LoadLease {
    LoadRequest request{};
    std::string bytes, error;
    std::shared_ptr<renium_history::WeakOwner> owner;
    Clock::time_point deadline;
    std::uint32_t phase = Armed;
    bool Alive() const { return Clock::now() < deadline && owner->Alive() && HostAlive(request); }
};
inline std::mutex loadMutex;
inline std::condition_variable loadChanged;
inline std::shared_ptr<LoadLease> loading;
inline std::atomic<Producer> producer{nullptr};
inline void Produce(void*, void**);

inline void Restore(const std::shared_ptr<LoadLease>& lease) {
    if (loading != lease) return;
    const auto& r = lease->request;
    const auto current = __atomic_load_n(reinterpret_cast<void**>(r.slot), __ATOMIC_ACQUIRE);
    if (current == reinterpret_cast<void*>(&Produce)
        && !Exchange(r.slot, reinterpret_cast<void*>(&Produce), reinterpret_cast<void*>(r.original)))
        throw std::runtime_error("Play callback changed during cleanup");
}

inline bool SavedPlace(const std::string& bytes) {
    return bytes.size() >= 14 && bytes.size() <= 128u * 1024 * 1024
        && bytes.capacity() >= bytes.size() && std::memcmp(bytes.data(), "<roblox", 7) == 0;
}

inline void Produce(void* capture, void** model) {
    std::shared_ptr<LoadLease> lease;
    {
        std::lock_guard lock(loadMutex);
        if (loading && loading->phase == Armed && model && *model == reinterpret_cast<void*>(loading->request.sourceOuter)) {
            lease = loading;
            lease->phase = lease->Alive() ? Serializing : Cancelled;
        }
    }
    // Preserve the engine's callback and scheduler, including exception unwinding.
    try { producer.load(std::memory_order_acquire)(capture, model); }
    catch (...) {
        if (lease) {
            std::lock_guard lock(loadMutex);
            lease->phase = Failed;
            lease->error = "Source serialization failed";
            try { Restore(lease); } catch (...) {}
            loadChanged.notify_all();
        }
        throw;
    }
    if (!lease) return;
    bool launch = false;
    std::string error;
    try { launch = lease->owner->Alive() && MatchesLaunch(lease->request); }
    catch (const std::exception& e) { error = e.what(); }
    std::lock_guard lock(loadMutex);
    if (!error.empty()) lease->error = error;
    if (lease->phase == Serializing && (!lease->Alive() || !launch)) lease->phase = Cancelled;
    if (lease->phase == Serializing) {
        const auto& r = lease->request;
        auto config = reinterpret_cast<unsigned char*>(Word(capture, r.fields.capture));
        auto& bytes = *reinterpret_cast<std::string*>(config + r.fields.string);
        if (Word(config, r.fields.place) != r.sourcePlace || Word(config, r.fields.universe) != r.sourceUniverse || !SavedPlace(bytes)) {
            lease->phase = Failed;
            lease->error = "The source loader input changed before destination replacement";
        } else {
            bytes.swap(lease->bytes);
            std::memcpy(config + r.fields.place, &r.destinationPlace, 8);
            std::memcpy(config + r.fields.universe, &r.destinationUniverse, 8);
            lease->phase = Consumed;
        }
    }
    try { Restore(lease); } catch (const std::exception& error) { lease->error = error.what(); }
    std::string{}.swap(lease->bytes);
    loadChanged.notify_all();
}

inline void Load(const std::string& payload, std::vector<unsigned char>& output) {
    LoadRequest r{};
    if (payload.size() < sizeof(r)) throw std::runtime_error("Truncated Mac Play loader request");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 1 || r.size != sizeof(r) || !r.token || r.action < 1 || r.action > 3
        || (r.action != 1 && payload.size() != sizeof(r)))
        throw std::runtime_error("Incompatible Mac Play loader request");
    if (r.action == 1) {
        if (!r.slot || !r.original || !r.sourceOuter || !r.sourceInstance || !r.sourceOwner
            || !r.getAttribute || !r.stringTypeTable || !r.nonce[0] || r.nonce[127]
            || !r.sourcePlace || !r.sourceUniverse || !r.destinationPlace || r.destinationUniverse != r.sourceUniverse
            || !r.lifetimeMs || r.lifetimeMs > 30000 || !HostAlive(r)
            || r.fields.capture > 65536 || r.fields.capture % 8 || r.fields.string > 65536 || r.fields.string % 8
            || r.fields.place > 65536 || r.fields.universe > 65536 || r.fields.place % 8 || r.fields.universe % 8
            || r.fields.place < r.fields.string + sizeof(std::string) || r.fields.universe < r.fields.string + sizeof(std::string)
            || r.fields.place == r.fields.universe || Word(reinterpret_cast<void*>(r.sourceInstance), 16) != r.sourceOwner)
            throw std::runtime_error("Invalid owned Mac Play loader binding");
        auto lease = std::make_shared<LoadLease>();
        lease->request = r;
        lease->bytes.assign(payload.data() + sizeof(r), payload.size() - sizeof(r));
        if (!SavedPlace(lease->bytes)) throw std::runtime_error("Destination is not saved place content");
        lease->owner = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(r.sourceOwner));
        lease->deadline = Clock::now() + std::chrono::milliseconds(r.lifetimeMs);
        std::lock_guard lock(loadMutex);
        if (loading && (loading->phase == Armed || loading->phase == Serializing))
            throw std::runtime_error("The source Play loader already has an owner");
        if (producer.load() && producer.load() != reinterpret_cast<Producer>(r.original))
            throw std::runtime_error("The Play loader changed in this process");
        producer.store(reinterpret_cast<Producer>(r.original), std::memory_order_release);
        if (!Exchange(r.slot, reinterpret_cast<void*>(r.original), reinterpret_cast<void*>(&Produce)))
            throw std::runtime_error("Another owner changed the Play loader callback");
        loading = lease;
        try { std::thread([lease] {
            std::unique_lock lock(loadMutex);
            while (loading == lease && (lease->phase == Armed || lease->phase == Serializing)) {
                if (!lease->Alive()) {
                    lease->phase = Cancelled;
                    try { Restore(lease); } catch (const std::exception& e) { lease->error = e.what(); }
                    std::string{}.swap(lease->bytes);
                    break;
                }
                loadChanged.wait_for(lock, std::chrono::milliseconds(100));
            }
        }).detach(); } catch (...) {
            lease->phase = Cancelled;
            Restore(lease);
            std::string{}.swap(lease->bytes);
            throw;
        }
    }
    std::lock_guard lock(loadMutex);
    if (!loading || loading->request.token != r.token) throw std::runtime_error("The Mac Play loader lease was replaced");
    if (r.action == 2 && (loading->phase == Armed || loading->phase == Serializing)) {
        loading->phase = Cancelled;
        Restore(loading);
        std::string{}.swap(loading->bytes);
        loadChanged.notify_all();
    }
    if (!loading->error.empty()) throw std::runtime_error(loading->error);
    r.phase = loading->phase;
    output.resize(sizeof(r));
    std::memcpy(output.data(), &r, sizeof(r));
}
}
