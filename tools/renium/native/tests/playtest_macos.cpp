#include <algorithm>
#include <cassert>
#include <chrono>
#include <cstdio>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include "../renium_studio_history.h"
#include "../renium_playtest_macos.h"
#include "../renium_route_code_macos.h"

using namespace renium_mac_playtest;
namespace {
template<class T> struct TrackedAllocator {
    using value_type = T;
    int* released;
    explicit TrackedAllocator(int* value) : released(value) {}
    template<class U> TrackedAllocator(const TrackedAllocator<U>& other) : released(other.released) {}
    T* allocate(std::size_t size) { return std::allocator<T>().allocate(size); }
    void deallocate(T* value, std::size_t size) { ++*released; std::allocator<T>().deallocate(value, size); }
};
std::string activeNonce = "owned-loader-test";
std::uint64_t stringTable = 123;
struct Operations { void(*copy)(const void*, void*); void(*move)(void*, void*); void(*destroy)(void*); };
void Copy(const void* source, void* destination) { new(destination) std::string(*static_cast<const std::string*>(source)); }
void Move(void* source, void* destination) { new(destination) std::string(std::move(*static_cast<std::string*>(source))); }
void Destroy(void* value) { static_cast<std::string*>(value)->~basic_string(); }
const Operations strings{Copy, Move, Destroy};
Variant Attribute(void*, StringView key) {
    assert(std::string(key.data, key.size) == "__ReniumLaunchNonce");
    Variant result;
    result.type = reinterpret_cast<std::uintptr_t>(&stringTable);
    result.operations = reinterpret_cast<std::uintptr_t>(&strings);
    new(result.payload) std::string(activeNonce);
    return result;
}
struct Source { std::uintptr_t unused[2]{}; void* owner = nullptr; };
struct Config { std::string bytes; std::uint64_t place = 0, universe = 0; };
struct Capture { void* unused = nullptr; Config* config = nullptr; };
unsigned forwarded = 0;
int (*routeForward)(int) = nullptr;
int RouteTarget(int value) { return routeForward(value) + 100; }
void Original(void* capture, void**) {
    ++forwarded;
    auto& config = *static_cast<Capture*>(capture)->config;
    config.bytes = "<roblox>source-content</roblox>";
    config.place = 101;
    config.universe = 202;
}
Producer slot = Original;

LoadRequest Request(const std::shared_ptr<Source>& source, std::uint64_t token) {
    LoadRequest r{};
    r.version = 1; r.size = sizeof(r); r.action = 1; r.token = token;
    r.slot = reinterpret_cast<std::uintptr_t>(&slot);
    r.original = reinterpret_cast<std::uintptr_t>(&Original);
    r.sourceInstance = r.sourceOuter = reinterpret_cast<std::uintptr_t>(source.get());
    std::memcpy(&source->owner, reinterpret_cast<const unsigned char*>(&source) + sizeof(void*), sizeof(void*));
    r.sourceOwner = reinterpret_cast<std::uintptr_t>(source->owner);
    r.sourcePlace = 101; r.destinationPlace = 303; r.sourceUniverse = r.destinationUniverse = 202;
    r.fields = {offsetof(Capture, config), offsetof(Config, bytes), offsetof(Config, place), offsetof(Config, universe)};
    r.getAttribute = reinterpret_cast<std::uintptr_t>(&Attribute);
    r.stringTypeTable = stringTable;
    r.hostPid = getpid(); r.lifetimeMs = 30000;
    proc_bsdinfo info{};
    assert(proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info));
    r.hostSeconds = info.pbi_start_tvsec; r.hostMicros = info.pbi_start_tvusec;
    std::strcpy(r.nonce, activeNonce.c_str());
    return r;
}

LoadRequest Send(LoadRequest request, const std::string& content = {}) {
    std::string payload(reinterpret_cast<const char*>(&request), sizeof(request));
    payload += content;
    std::vector<unsigned char> output;
    Load(payload, output);
    assert(output.size() == sizeof(request));
    std::memcpy(&request, output.data(), sizeof(request));
    return request;
}
}

int main() {
    // Startup workers can exit between the thread snapshot and hook install.
    // Their dead Mach ports must not reject an otherwise safe code patch.
    for (unsigned i = 0; i < 8; ++i) {
        std::atomic_bool ready = false, leave = false;
        std::thread worker([&] {
            ready.store(true);
            while (!leave.load()) std::this_thread::yield();
        });
        while (!ready.load()) std::this_thread::yield();
        renium_history::PausedThreads threads;
        leave.store(true);
        worker.join();
        const bool paused = threads.Pause(0, 1);
        threads.Resume();
        assert(paused && "an exited thread cannot execute the patch site");
    }
    const std::uint32_t instructions[] = {0xa9bf7bfd, 0x910003fd, 0xaa0003e9, 0xd503201f,
        0x11000400, 0xa8c17bfd, 0xd65f03c0};
    std::vector<unsigned char> code(sizeof(instructions));
    std::memcpy(code.data(), instructions, sizeof(instructions));
    ReniumSignedCode signedCode(code);
    auto routed = signedCode.Map(nullptr);
    assert(routed != MAP_FAILED);
    const auto entry = reinterpret_cast<std::uintptr_t>(routed);
    assert(reinterpret_cast<int(*)(int)>(routed)(4) == 5);
    routeForward = reinterpret_cast<int(*)(int)>(renium_mac_route_code::Forward(entry, code.data()));
    const auto prefix = std::vector<unsigned char>(code.begin(), code.begin() + 16);
    const auto jump = renium_history::Jump(reinterpret_cast<std::uintptr_t>(&RouteTarget));
    renium_mac_route_code::Replace(entry, prefix, jump);
    assert(reinterpret_cast<int(*)(int)>(routed)(4) == 105);
    renium_mac_route_code::Replace(entry, jump, prefix);
    assert(reinterpret_cast<int(*)(int)>(routed)(4) == 5 && routeForward(4) == 5);

    // A replaced client model can die before its routing lease. Releasing the
    // final native weak owner must free the control block exactly once.
    int released = 0;
    auto departing = std::allocate_shared<int>(TrackedAllocator<int>(&released), 42);
    void* control = nullptr;
    std::memcpy(&control, reinterpret_cast<const char*>(&departing) + sizeof(void*), sizeof(control));
    auto weak = std::make_unique<renium_history::WeakOwner>(control);
    assert(weak->Alive());
    departing.reset();
    assert(!weak->Alive() && released == 0);
    weak.reset();
    assert(released == 1);

    const auto source = std::make_shared<Source>();
    const std::string destination = "<roblox>" + std::string(150000, 'd') + "</roblox>";
    Config config;
    Capture capture{nullptr, &config};
    auto r = Send(Request(source, 1), destination);
    auto other = std::make_shared<Source>();
    void* model = other.get();
    slot(&capture, &model);
    assert(forwarded == 1 && config.place == 101 && slot != Original);
    model = source.get();
    slot(&capture, &model);
    assert(forwarded == 2 && config.bytes == destination && config.place == 303 && config.universe == 202 && slot == Original);
    r.action = 3;
    assert(Send(r).phase == Consumed);

    r = Send(Request(source, 2), destination);
    r.action = 2;
    assert(Send(r).phase == Cancelled && slot == Original);
    slot(&capture, &model);
    assert(config.place == 101 && config.bytes != destination);

    r = Send(Request(source, 3), destination);
    activeNonce = "new-play-after-stop";
    slot(&capture, &model);
    r.action = 3;
    assert(Send(r).phase == Cancelled && config.place == 101 && slot == Original);

    r = Request(source, 4);
    r.lifetimeMs = 1;
    r = Send(r, destination);
    std::this_thread::sleep_for(std::chrono::milliseconds(5));
    slot(&capture, &model);
    r.action = 3;
    assert(Send(r).phase == Cancelled && config.place == 101 && slot == Original);

    r = Request(source, 5);
    ++r.hostSeconds;
    bool rejected = false;
    try { Send(r, destination); } catch (const std::exception&) { rejected = true; }
    assert(rejected && slot == Original);
    std::this_thread::sleep_for(std::chrono::milliseconds(120));
    std::puts("Mac loader ownership, forwarding, large content, Stop, timeout and host identity passed");
}
