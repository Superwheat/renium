#pragma once
#include "renium_relay_macos.h"
#include "renium_route_code_macos.h"
#include <array>
#include <mach/vm_statistics.h>
#include <sys/mman.h>

void* ReniumTeleportClientWidget(std::uint32_t window);
namespace renium_mac_test_client {
using namespace renium_mac_relay;
struct Request {
    std::uint32_t version, size, action, receipt;
    std::uint64_t token;
    Binding relay;
    std::uint64_t table, baseOffset, baseTable, parse, schedule, start, open, destroy, closeSlots[8];
    std::uint64_t calls[8];
    std::uint32_t count, window;
    std::uint32_t closeCount, reserved;
};
static_assert(sizeof(Request) == 344);
inline std::atomic<void*> retainedWidget{nullptr};
inline std::atomic<bool(*)(void*)> originalClose{nullptr};
inline bool CloseWidget(void* widget) {
    void* expected = widget;
    if (retainedWidget.compare_exchange_strong(expected, nullptr)) return false;
    return originalClose.load()(widget);
}
struct Transition {
    Request request{};
    std::shared_ptr<Task> task;
    Clock::time_point deadline = Clock::now() + std::chrono::seconds(60);
    unsigned installed = 0;
    unsigned redirected = 0;
    void* close[8]{};
    void Restore() {
        while (installed) {
            const auto i = --installed;
            renium_mac_route_code::Call(request.calls[i], request.start, request.open);
        }
        retainedWidget.store(nullptr);
        while (redirected) {
            const auto i = --redirected;
            Exchange(request.closeSlots[i], reinterpret_cast<void*>(&CloseWidget), close[i]);
        }
    }
};
inline std::mutex transitionMutex;
inline std::shared_ptr<Transition> pending;

struct OpenerSearch {
    const Request& request;
    static constexpr std::size_t BufferSize = 1024 * 1024;
    unsigned char* bytes;
    explicit OpenerSearch(const Request& r) : request(r) {
        bytes = static_cast<unsigned char*>(mmap(nullptr, BufferSize, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANON, -1, 0));
        if (bytes == MAP_FAILED) throw std::runtime_error("Cannot allocate client owner scan buffer");
    }
    ~OpenerSearch() { munmap(bytes, BufferSize); }
    OpenerSearch(const OpenerSearch&) = delete;
    OpenerSearch& operator=(const OpenerSearch&) = delete;
    Clock::time_point deadline = Clock::now() + std::chrono::seconds(10);
    std::uintptr_t found = 0;
    std::uint64_t scanned = 0;

    void CheckTime() const {
        if (Clock::now() >= deadline)
            throw std::runtime_error("Client owner scan timed out (readMiB=" + std::to_string(scanned >> 20) + ")");
    }
    void Scan(std::uint64_t address, std::uint64_t length) {
        const auto scratch = reinterpret_cast<std::uintptr_t>(bytes);
        for (std::uint64_t at = 0; at < length; at += BufferSize) {
            CheckTime();
            mach_vm_size_t read = 0;
            const auto size = std::min<std::uint64_t>(BufferSize, length - at);
            // A region can be retired concurrently; never dereference it.
            if (mach_vm_read_overwrite(mach_task_self(), address + at, size,
                    reinterpret_cast<mach_vm_address_t>(bytes), &read) != KERN_SUCCESS) continue;
            scanned += read;
            for (std::size_t i = 0; i + 8 <= read; i += 8) {
                if (Word(bytes, i) != request.table) continue;
                const auto candidate = address + at + i;
                // The scan buffer can share its allocator's mapping with the
                // real owner. Exclude only the buffer, never its entire region.
                if (candidate >= scratch && candidate < scratch + BufferSize) continue;
                std::uint64_t base = 0;
                mach_vm_size_t copied = 0;
                if (mach_vm_read_overwrite(mach_task_self(), candidate + request.baseOffset, 8,
                        reinterpret_cast<mach_vm_address_t>(&base), &copied) != KERN_SUCCESS
                        || copied != 8 || base != request.baseTable) continue;
                if (found && found != candidate)
                    throw std::runtime_error("Client has multiple complete PlaceOpener owners");
                found = candidate;
            }
        }
    }
};
inline void* Opener(const Request& r) {
    // Allocator arenas may be sparse, shared, or externally backed. Studio's
    // PlaceOpener has been observed inside an IOAccelerator shared mapping;
    // neither malloc-zone introspection nor private-only scans cover it.
    OpenerSearch search(r);
    const auto pageSize = static_cast<std::uint64_t>(vm_page_size);
    std::array<int, 4096> dispositions{};
    mach_vm_address_t address = 0;
    natural_t depth = 0;
    for (;;) {
        search.CheckTime();
        mach_vm_size_t length = 0;
        vm_region_submap_info_data_64_t info{};
        mach_msg_type_number_t count = VM_REGION_SUBMAP_INFO_COUNT_64;
        if (mach_vm_region_recurse(mach_task_self(), &address, &length, &depth,
                reinterpret_cast<vm_region_recurse_info_t>(&info), &count) != KERN_SUCCESS) break;
        if (info.is_submap) { ++depth; continue; }
        if (!length || address + length <= address) break;
        const auto end = address + length;
        if (info.user_tag != VM_MEMORY_STACK
            && (info.protection & (VM_PROT_READ | VM_PROT_WRITE)) == (VM_PROT_READ | VM_PROT_WRITE)) {
            const bool anonymous = !info.external_pager
                && (info.share_mode == SM_PRIVATE || info.share_mode == SM_COW || info.share_mode == SM_EMPTY);
            const auto backed = (std::uint64_t(info.pages_resident) + info.pages_swapped_out) * pageSize;
            if (!anonymous || backed >= length / 2) {
                search.Scan(address, length);
            } else if (backed) {
                // Never fault an uncommitted arena into memory merely to search
                // it. Shared/device mappings do not expose this page accounting.
                for (auto at = address; at < end;) {
                    search.CheckTime();
                    const auto size = std::min<std::uint64_t>(end - at, dispositions.size() * pageSize);
                    mach_vm_size_t pages = dispositions.size();
                    if (mach_vm_page_range_query(mach_task_self(), at, size,
                            reinterpret_cast<mach_vm_address_t>(dispositions.data()), &pages) != KERN_SUCCESS
                            || pages != size / pageSize)
                        throw std::runtime_error("Cannot query client owner pages");
                    for (std::size_t page = 0; page < pages;) {
                        constexpr int present = VM_PAGE_QUERY_PAGE_PRESENT | VM_PAGE_QUERY_PAGE_PAGED_OUT;
                        if (!(dispositions[page] & present)) { ++page; continue; }
                        const auto first = page++;
                        while (page < pages && (dispositions[page] & present)) ++page;
                        search.Scan(at + first * pageSize, (page - first) * pageSize);
                    }
                    at += size;
                }
            }
        }
        address = end;
    }
    if (!search.found) throw std::runtime_error("Client PlaceOpener owner was not found");
    return reinterpret_cast<void*>(search.found);
}
struct Configuration { alignas(16) unsigned char bytes[65536]{}; ~Configuration() {} };
inline void Handle(const std::string& payload, std::vector<unsigned char>& output) {
    Request r{};
    if (payload.size() != sizeof(r)) throw std::runtime_error("Truncated Mac client route");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 1 || r.size != sizeof(r) || !r.token || r.action < 1 || r.action > 3)
        throw std::runtime_error("Incompatible Mac client route");
    std::lock_guard lock(transitionMutex);
    if (r.action == 1) {
        if (pending || !r.table || !r.baseTable || !r.baseOffset || r.baseOffset >= 65536
            || !r.parse || !r.schedule || !r.start || !r.open || !r.destroy || !r.closeCount || r.closeCount > 8
            || r.reserved || !r.window || !r.count || r.count > 8)
            throw std::runtime_error("Invalid Mac client route ownership");
        auto transition = std::make_shared<Transition>(); transition->request = r;
        auto opener = Opener(r);
        auto widget = ReniumTeleportClientWidget(r.window);
        const auto close = dlsym(RTLD_DEFAULT, "_ZN7QWidget5closeEv");
        if (!widget || !close) throw std::runtime_error("Client main window ended");
        originalClose.store(reinterpret_cast<bool(*)(void*)>(close));
        retainedWidget.store(widget);
        try {
            for (; transition->redirected < r.closeCount; ++transition->redirected) {
                const auto i = transition->redirected;
                transition->close[i] = reinterpret_cast<void*>(Word(reinterpret_cast<void*>(r.closeSlots[i]), 0));
                if (!transition->close[i] || transition->close[i] == reinterpret_cast<void*>(&CloseWidget)
                    || !Exchange(r.closeSlots[i], transition->close[i], reinterpret_cast<void*>(&CloseWidget)))
                    throw std::runtime_error("Client close import changed");
            }
            for (; transition->installed < r.count; ++transition->installed)
                renium_mac_route_code::Call(r.calls[transition->installed], r.open, r.start);
            transition->task = Arm(r.relay, [r, opener](Values values) {
                if (!values || values->values.size() != 1) throw std::runtime_error("Client configuration relay changed");
                auto configuration = reinterpret_cast<Configuration(*)(const Variant*)>(r.parse)(&values->values[0]);
                try {
                    // This entry schedules its own Tasks fiber and consumes the
                    // engine's configuration. Never call the synchronous wrapper.
                    reinterpret_cast<void(*)(void*, void*, bool)>(r.schedule)(opener, configuration.bytes, false);
                } catch (...) { reinterpret_cast<void(*)(void*)>(r.destroy)(configuration.bytes); throw; }
                reinterpret_cast<void(*)(void*)>(r.destroy)(configuration.bytes);
            });
            pending = transition;
            std::thread([transition] {
                for (;;) {
                    std::this_thread::sleep_for(std::chrono::milliseconds(100));
                    std::lock_guard lock(transitionMutex);
                    if (pending != transition) return;
                    if (!Host(transition->request.relay) || Clock::now() >= transition->deadline) {
                        Cancel(transition->task);
                        try { transition->Restore(); } catch (...) {}
                        pending.reset(); return;
                    }
                }
            }).detach();
        } catch (...) { transition->Restore(); throw; }
    } else {
        if (!pending || pending->request.token != r.token) throw std::runtime_error("Mac client route ended");
        if (r.action == 2) { Cancel(pending->task); pending->Restore(); pending.reset(); }
        else r.receipt = Complete(pending->task) ? 2 : 0;
    }
    output.resize(sizeof(r)); std::memcpy(output.data(), &r, sizeof(r));
}
}
