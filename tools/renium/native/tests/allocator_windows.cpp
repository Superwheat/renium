#include <Windows.h>
#include <cassert>
#include <array>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include "../renium_engine_allocator_windows.h"
#include "../renium_studio_history.h"
#include "../renium_play_loader_windows.h"

static bool AddOwnerReference(void* owner) {
    auto uses = reinterpret_cast<volatile long*>(static_cast<unsigned char*>(owner) + 8);
    auto value = *uses;
    while (value > 0) {
        const auto observed = InterlockedCompareExchange(uses, value + 1, value);
        if (observed == value) return true;
        value = observed;
    }
    return false;
}
static void ReleaseOwnerReference(void* owner) {
    auto uses = reinterpret_cast<volatile long*>(static_cast<unsigned char*>(owner) + 8);
    assert(InterlockedDecrement(uses) > 0);
}
#include "../renium_play_join_windows.h"
#include "../renium_play_caption_windows.h"

extern "C" __declspec(dllimport) void* mi_malloc(std::size_t);
extern "C" __declspec(dllimport) void* mi_malloc_aligned(std::size_t, std::size_t);
extern "C" __declspec(dllimport) void mi_free(void*);
extern "C" __declspec(dllimport) void EngineFinish(void*, std::string, std::uint32_t, renium_history::Options);
extern "C" __declspec(dllimport) void EngineGetString(void*);
extern "C" __declspec(dllimport) void EngineStoreCallback(std::function<void(std::string)>);
extern "C" __declspec(dllimport) void EngineInvokeCallback();
extern "C" __declspec(dllimport) std::uintptr_t EngineHistoryPredecessor(renium_history::Binding, bool);

// Exercise allocation during executable static initialization too.
static const std::string initial(512, 'i');

namespace play_loader_test {
struct Config { std::string bytes; std::uint64_t universe = 7, padding = 0, place = 11; };
struct Capture { std::uint64_t padding[5]{}; Config* config; };
static HANDLE entered = CreateEventW(nullptr, TRUE, FALSE, nullptr);
static HANDLE release = CreateEventW(nullptr, TRUE, FALSE, nullptr);
static std::atomic<bool> delay{false};
static void Producer(void* opaque, void*) {
    auto& config = *static_cast<Capture*>(opaque)->config;
    config.bytes.~basic_string();
    EngineGetString(&config.bytes); // engine allocation destroyed by the helper
    config.bytes.replace(0, 7, "<roblox");
    if (delay.exchange(false)) { SetEvent(entered); WaitForSingleObject(release, 2000); }
}
static auto callback = &Producer;
static std::string currentNonce = "owned-test-launch";
static std::uintptr_t stringType = 0x5678;
static void CopyString(const void* value, void* output) { new(output) std::string(*static_cast<const std::string*>(value)); }
static void MoveString(void* value, void* output) { new(output) std::string(std::move(*static_cast<std::string*>(value))); }
static void DestroyString(void* value) { static_cast<std::string*>(value)->~basic_string(); }
static renium_play_teleport::Operations stringOperations{&CopyString, &MoveString, &DestroyString};
static void* Attribute(void* instance, void* output, renium_play_loader::StringView name) {
    assert(instance == reinterpret_cast<void*>(0x1234) && std::string(name.data, name.size) == "__ReniumLaunchNonce");
    auto* value = new(output) renium_play_loader::VariantBuffer{};
    value->type = reinterpret_cast<std::uintptr_t>(&stringType);
    value->operations = reinterpret_cast<std::uintptr_t>(&stringOperations);
    new(value->payload) std::string(currentNonce);
    return output;
}
static renium_play_loader::Request Request(std::uint64_t token, const std::string& payload) {
    using namespace renium_play_loader;
    renium_play_loader::Request r{};
    r.version = Protocol; r.size = sizeof(r); r.action = 1; r.token = token;
    r.slot = reinterpret_cast<std::uintptr_t>(&callback); r.original = reinterpret_cast<std::uintptr_t>(&Producer);
    r.sourceModel = 0x1234; r.sourcePlace = 11; r.sourceUniverse = 7;
    r.getAttribute = reinterpret_cast<std::uintptr_t>(&Attribute); r.stringTypeTable = stringType;
    r.sourceInstance = 0x1234;
    strcpy_s(r.launchNonce, "owned-test-launch");
    r.destinationPlace = 12; r.destinationUniverse = 7;
    r.fields = {offsetof(Capture, config), offsetof(Config, bytes), offsetof(Config, place), offsetof(Config, universe)};
    r.bytes = reinterpret_cast<std::uintptr_t>(payload.data()); r.length = payload.size();
    r.hostPid = GetCurrentProcessId(); r.lifetimeMs = 2000;
    FILETIME created{}, exited{}, kernel{}, user{};
    assert(GetProcessTimes(GetCurrentProcess(), &created, &exited, &kernel, &user));
    r.hostCreation = static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime;
    return r;
}
static void Run() {
    using namespace renium_play_loader;
    std::uint64_t token = 1;
    for (const auto direct : {false, true}) for (const auto size : {32, 8192, 131072, 2097152}) {
        std::string payload(size, 'd'); payload.replace(0, 7, "<roblox");
        auto r = Request(token++, payload);
        r.directModel = direct;
        Handle(r);
        Config config; Capture capture{{}, &config};
        void* unrelated = reinterpret_cast<void*>(0x5678);
        callback(&capture, direct ? unrelated : &unrelated);
        assert(current->phase == Armed && config.place == 11);
        void* model = reinterpret_cast<void*>(0x1234);
        callback(&capture, direct ? model : &model);
        assert(current->phase == Consumed && config.bytes == payload && config.place == 12 && config.universe == 7);
        assert(callback == &Producer);
    }
    const std::string payload = "<roblox saved test payload";
    auto r = Request(token++, payload);
    r.action = 2; Handle(r); r.action = 1;
    bool rejected = false;
    try { Handle(r); } catch (const std::runtime_error&) { rejected = true; }
    assert(rejected && callback == &Producer);
    r = Request(token++, payload); Handle(r);
    Config oldConfig; Capture oldCapture{{}, &oldConfig};
    void* model = reinterpret_cast<void*>(0x1234);
    delay = true;
    std::thread pending([&] { callback(&oldCapture, &model); });
    assert(WaitForSingleObject(entered, 1000) == WAIT_OBJECT_0);
    r.action = 2; Handle(r);
    auto next = Request(token++, payload); Handle(next);
    SetEvent(release); pending.join();
    assert(oldConfig.place == 11 && callback == &CallbackEntry && current->phase == Armed);
    Config nextConfig; Capture nextCapture{{}, &nextConfig};
    callback(&nextCapture, &model);
    assert(nextConfig.place == 12 && callback == &Producer);
    auto replaced = Request(token++, payload); Handle(replaced);
    currentNonce = "manual-replacement";
    Config manual; Capture manualCapture{{}, &manual};
    callback(&manualCapture, &model);
    assert(current->phase == Cancelled && manual.place == 11 && callback == &Producer);
    currentNonce = "owned-test-launch";
    auto expired = Request(token++, payload); expired.lifetimeMs = 1; Handle(expired);
    const auto lease = current;
    assert(WaitForSingleObject(lease->finished, 1000) == WAIT_OBJECT_0);
    assert(lease->phase == Cancelled && callback == &Producer);
    CloseHandle(entered); CloseHandle(release);
}
}
namespace arrival_test { static void Run(std::uint64_t dispatch, std::uint64_t table); }
namespace teleport_dispatch_test {
using namespace renium_play_teleport;
static unsigned forwarded = 0, invoked = 0;
static std::function<void(Values)> completion;
static Failure failure;
static void Forward(void*, std::int64_t place, Players players, Instance options, Success success, Failure error) {
    assert(place == 12 && players->size() == 1 && options);
    ++forwarded;
    success(options);
}
static void InvokeRelay(void*, Values values, std::function<void(Values)> success, Failure error) {
    assert(values->values.size() == 3);
    assert(*reinterpret_cast<const std::string*>(values->values[0].payload) == "12");
    assert(*reinterpret_cast<const Instance*>(values->values[1].payload));
    assert(*reinterpret_cast<const Instance*>(values->values[2].payload));
    // Copy the argument vector just as the engine's asynchronous invocation
    // does, then retain both continuations beyond the native wrapper's return.
    const Tuple copied = *values;
    assert(copied.values[2].operations == reinterpret_cast<std::uintptr_t>(&instanceOperations));
    completion = std::move(success);
    failure = std::move(error);
    ++invoked;
}
static void* Attribute(void*, void* output, renium_play_loader::StringView key) {
    assert(std::string(key.data, key.size) == "__ReniumNative");
    return play_loader_test::Attribute(reinterpret_cast<void*>(0x1234), output, {"__ReniumLaunchNonce", 19});
}
struct ClassDescriptor { void* table = nullptr; const std::string* name; };
struct NativeInstance {
    std::uint64_t padding = 0;
    NativeInstance* parent = nullptr;
    const std::vector<Instance>* children = nullptr;
    ClassDescriptor* kind = nullptr;
};
static const std::string resultName = "TeleportAsyncResult";
static ClassDescriptor resultClass{nullptr, &resultName};
static Instance* CreateResult(void*, Instance* output, void* context, bool serialized) {
    assert(!context && serialized);
    auto result = std::make_shared<NativeInstance>(); result->kind = &resultClass;
    *output = result; return output;
}
static void* resultTable[]{reinterpret_cast<void*>(&CreateResult)};
static void** resultFactory = resultTable;
static void Early(Request r, Players peers, Instance options, Values reply);
static void Run() {
    const std::string modelName = "DataModel", storageName = "ServerStorage", relayName = "BindableFunction";
    ClassDescriptor modelClass{nullptr, &modelName}, storageClass{nullptr, &storageName}, relayClass{nullptr, &relayName};
    NativeInstance model, unrelated;
    model.kind = &modelClass;
    auto source = std::make_shared<NativeInstance>(), relay = std::make_shared<NativeInstance>(), storage = std::make_shared<NativeInstance>();
    auto player = std::make_shared<int>(3), options = std::make_shared<int>(4);
    std::vector<Instance> roots{storage}, children{relay};
    model.children = &roots; storage->kind = &storageClass; storage->children = &children; relay->kind = &relayClass;
    source->parent = &model;
    auto peers = std::make_shared<std::vector<Instance>>(); peers->push_back(player);
    auto slot = &Forward;
    Request r{}; r.version = 6; r.size = sizeof(r); r.action = 1; r.token = 1;
    r.slot = reinterpret_cast<std::uintptr_t>(&slot); r.original = reinterpret_cast<std::uintptr_t>(&Forward);
    r.parentOffset = offsetof(NativeInstance, parent); r.classOffset = offsetof(NativeInstance, kind);
    r.childrenOffset = offsetof(NativeInstance, children);
    strcpy_s(r.runtime, play_loader_test::currentNonce.c_str());
    r.invoke = reinterpret_cast<std::uintptr_t>(&InvokeRelay); r.getAttribute = reinterpret_cast<std::uintptr_t>(&Attribute);
    r.instanceType = 42; r.stringTypeTable = play_loader_test::stringType;
    r.tableType = 43;
    r.resultFactory = reinterpret_cast<std::uintptr_t>(&resultFactory); r.resultCreate = reinterpret_cast<std::uintptr_t>(&CreateResult);
    r.hostPid = GetCurrentProcessId(); r.lifetimeMs = 1000;
    FILETIME created{}, exited{}, kernel{}, user{};
    assert(GetProcessTimes(GetCurrentProcess(), &created, &exited, &kernel, &user));
    r.hostCreation = static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime;
    Handle(r);
    unsigned accepted = 0, failed = 0;
    slot(&unrelated, 12, peers, options, [&](Instance value) { assert(value == options); ++accepted; }, [&](std::string) { ++failed; });
    assert(forwarded == 1 && invoked == 0);
    slot(source.get(), 12, peers, options, [&](Instance value) {
        assert(value && Class(value.get(), offsetof(NativeInstance, kind), "TeleportAsyncResult")); ++accepted;
    }, [&](std::string) { ++failed; });
    assert(forwarded == 1 && invoked == 1 && accepted == 1);
    auto reply = std::make_shared<Tuple>();
    AppendInstance(*reply, std::make_shared<int>(45), r.tableType);
    auto& teleportData = reply->values.emplace_back();
    play_loader_test::Attribute(reinterpret_cast<void*>(0x1234), &teleportData, {"__ReniumLaunchNonce", 19});
    *reinterpret_cast<std::string*>(teleportData.payload) = std::string(8192, 'd');
    auto& sequence = reply->values.emplace_back();
    play_loader_test::Attribute(reinterpret_cast<void*>(0x1234), &sequence, {"__ReniumLaunchNonce", 19});
    *reinterpret_cast<std::string*>(sequence.payload) = "7";
    completion(reply); completion = {}; failure = {};
    assert(accepted == 2 && failed == 0);
    assert(Arrival(r.token, 7, 12)->join.type == r.tableType);
    arrival_test::Run(r.token, r.tableType);
    slot(source.get(), 12, {}, options, [&](Instance) { ++accepted; }, [&](std::string) { ++failed; });
    assert(failed == 1);
    // Studio throws from a synchronous error continuation. Dispatch must not
    // consume that exception and strand the original Luau caller.
    bool synchronousError = false;
    try {
        slot(source.get(), 12, {}, options, [](Instance) { assert(false); },
            [](std::string message) { throw std::runtime_error(message); });
    } catch (const std::runtime_error& error) {
        synchronousError = std::string(error.what()).find("connected players") != std::string::npos;
    }
    assert(synchronousError);
    std::string rejected;
    slot(source.get(), 12, peers, options, [](Instance) { assert(false); },
        [&](std::string message) { rejected = std::move(message); });
    auto rejection = std::make_shared<Tuple>();
    rejection->values.push_back(sequence);
    *reinterpret_cast<std::string*>(rejection->values[0].payload) = "Player already has a pending teleport";
    completion(rejection); completion = {}; failure = {};
    assert(rejected == "Player already has a pending teleport");
    r.action = 2; Handle(r);
    assert(slot == &Forward);
    r.action = 1; r.token = 2; Handle(r);
    current->deadline.store(0);
    r.action = 3;
    bool expired = false;
    try { Handle(r); } catch (const std::runtime_error&) { expired = true; }
    assert(expired);
    r.action = 2; Handle(r);
    assert(slot == &Forward);
    Early(r, peers, options, reply);
}
}

namespace arrival_test {
using namespace renium_play_join;
using teleport_dispatch_test::ClassDescriptor;
using teleport_dispatch_test::NativeInstance;
struct Model : NativeInstance { std::uint64_t place = 12, universe = 7; };
struct Player : NativeInstance { std::shared_ptr<const void> join; };
struct Service : NativeInstance { Variant data; };
static Variant* arguments = nullptr;
static unsigned executed = 0, joined = 0, localRead = 0;
static void ExecuteForward(void*, Variant value, std::function<void(Variant)> success, std::function<void(std::string)>) {
    ++executed; success(std::move(value));
}
static int JoinForward(void*, void* lua) { ++joined; assert(lua == reinterpret_cast<void*>(55)); return 1; }
static void* LocalForward(void* receiver, void* out) { ++localRead; return new(out) Variant(static_cast<Service*>(receiver)->data); }
static void* Arguments(void*, void* out) { return new(out) Variant(*arguments); }
static void* Attribute(void*, void* out, renium_play_loader::StringView key) {
    return play_loader_test::Attribute(reinterpret_cast<void*>(0x1234), out, key);
}
static void Run(std::uint64_t dispatch, std::uint64_t table) {
    const std::string modelName = "DataModel", testName = "StudioTestService", networkName = "NetworkClient", playerName = "Player", serviceName = "TeleportService";
    ClassDescriptor modelKind{nullptr, &modelName}, testKind{nullptr, &testName}, networkKind{nullptr, &networkName}, playerKind{nullptr, &playerName}, serviceKind{nullptr, &serviceName};
    Model source, destination, unrelated;
    source.kind = destination.kind = unrelated.kind = &modelKind;
    auto test = std::make_shared<NativeInstance>(), network = std::make_shared<NativeInstance>();
    test->kind = &testKind; network->kind = &networkKind;
    std::vector<renium_play_teleport::Instance> roots{test, network}; destination.children = &roots; unrelated.children = &roots;
    Player player; player.parent = &destination; player.kind = &playerKind;
    Service service; service.parent = &destination; service.kind = &serviceKind;
    renium_play_teleport::Tuple input;
    renium_play_teleport::AppendInstance(input, std::make_shared<int>(100), table);
    arguments = &input.values[0];
    auto execute = &ExecuteForward; auto join = &JoinForward; auto local = &LocalForward;
    Request r{}; r.version = 2; r.size = sizeof(r); r.action = 1; r.token = 123;
    r.dispatchToken = dispatch; r.attempt = 7; r.place = 12; r.universe = 7;
    r.sourceModel = reinterpret_cast<std::uintptr_t>(&source); r.sourceTest = reinterpret_cast<std::uintptr_t>(&source);
    r.executeSlot = reinterpret_cast<std::uintptr_t>(&execute); r.executeOriginal = reinterpret_cast<std::uintptr_t>(&ExecuteForward);
    r.joinSlot = reinterpret_cast<std::uintptr_t>(&join); r.joinOriginal = reinterpret_cast<std::uintptr_t>(&JoinForward);
    r.localSlot = reinterpret_cast<std::uintptr_t>(&local); r.localOriginal = reinterpret_cast<std::uintptr_t>(&LocalForward);
    r.joinField = reinterpret_cast<char*>(&player.join) - reinterpret_cast<char*>(&player);
    r.localField = reinterpret_cast<char*>(&service.data) - reinterpret_cast<char*>(&service);
    r.placeField = reinterpret_cast<char*>(&destination.place) - reinterpret_cast<char*>(&destination);
    r.universeField = reinterpret_cast<char*>(&destination.universe) - reinterpret_cast<char*>(&destination);
    r.parentOffset = offsetof(NativeInstance, parent); r.childrenOffset = offsetof(NativeInstance, children); r.classOffset = offsetof(NativeInstance, kind);
    r.tableType = table; r.getArgs = reinterpret_cast<std::uintptr_t>(&Arguments);
    r.getAttribute = reinterpret_cast<std::uintptr_t>(&Attribute); r.stringTypeTable = play_loader_test::stringType;
    strcpy_s(r.nonce, "owned-test-launch");
    r.hostPid = GetCurrentProcessId(); r.lifetimeMs = 2000;
    FILETIME created{}, exited{}, kernel{}, user{};
    assert(GetProcessTimes(GetCurrentProcess(), &created, &exited, &kernel, &user));
    r.hostCreation = static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime;
    Handle(r);
    void* lua = reinterpret_cast<void*>(55);
    assert(join(&player, lua) == 1 && !player.join); // No owned launch yet.
    auto success = [](Variant) {};
    execute(&unrelated, *arguments, success, {});
    assert(join(&player, lua) == 1 && !player.join);
    play_loader_test::currentNonce = "replacement";
    execute(&source, *arguments, success, {});
    assert(join(&player, lua) == 1 && !player.join);
    play_loader_test::currentNonce = "owned-test-launch";
    execute(&source, *arguments, success, {});
    destination.place = 13;
    assert(join(&player, lua) == 1 && !player.join);
    destination.place = 12;
    renium_play_teleport::Tuple other;
    renium_play_teleport::AppendInstance(other, std::make_shared<int>(101), table);
    arguments = &other.values[0];
    assert(join(&player, lua) == 1 && !player.join);
    arguments = &input.values[0];
    assert(join(&player, lua) == 1 && player.join);
    auto retained = player.join;
    // Studio may initialize an empty native table before the first getter.
    // Arrival must replace that table too, not only a null shared pointer.
    player.join = std::make_shared<int>(999);
    assert(join(&player, lua) == 1 && player.join == retained);
    Variant output; local(&service, &output);
    assert(localRead == 1 && executed == 3 && joined == 7);
    assert(*reinterpret_cast<std::string*>(output.payload) == std::string(8192, 'd'));
    r.action = 2; Handle(r);
    assert(execute == &ExecuteForward && join == &JoinForward && local == &LocalForward);
    assert(join(&player, lua) == 1 && player.join == retained); // Native field outlives hooks.
    assert(*reinterpret_cast<std::string*>(service.data.payload) == std::string(8192, 'd'));
    r.action = 1; r.token = 124; r.lifetimeMs = 1; Handle(r);
    const auto deadline = GetTickCount64() + 1000;
    const auto restored = [&] {
        std::lock_guard lock(renium_play_join::mutex);
        return execute == &ExecuteForward && join == &JoinForward && local == &LocalForward;
    };
    while (!restored() && GetTickCount64() < deadline) Sleep(10);
    assert(restored());
    arguments = nullptr;
}
}

namespace teleport_dispatch_test {
struct Context { void* table; std::mutex mutex; std::vector<std::function<void()>> work; };
struct Owner { void* table = nullptr; long uses = 1; long weak = 1; };
struct EarlyModel : NativeInstance {
    std::uint64_t place = 12, universe = 7;
    void* self = this;
    Owner* owner = nullptr;
    Context* context = nullptr;
};
static bool Submit(void* pointer, std::function<void()>* work, std::uint32_t mode) {
    assert(mode == 1);
    auto& context = *static_cast<Context*>(pointer);
    std::lock_guard lock(context.mutex);
    context.work.push_back(*work);
    return true;
}
static void Drain(Context& context) {
    std::vector<std::function<void()>> work;
    { std::lock_guard lock(context.mutex); work.swap(context.work); }
    for (auto& callback : work) callback();
}
static void* EarlyAttribute(void*, void* output, renium_play_loader::StringView key) {
    if (std::string(key.data, key.size) == "__ReniumNative") return Attribute(nullptr, output, key);
    return arrival_test::Attribute(nullptr, output, key);
}
static void Early(Request r, Players peers, Instance options, Values reply) {
    const std::string modelName = "DataModel", testName = "StudioTestService", serviceName = "TeleportService", storageName = "ServerStorage", relayName = "BindableFunction";
    ClassDescriptor modelClass{nullptr, &modelName}, testClass{nullptr, &testName}, serviceClass{nullptr, &serviceName}, storageClass{nullptr, &storageName}, relayClass{nullptr, &relayName};
    Owner owner;
    Context context{reinterpret_cast<void*>(0x1230)};
    EarlyModel edit, model;
    edit.kind = model.kind = &modelClass; model.owner = &owner; model.context = &context;
    NativeInstance service; service.parent = &model; service.kind = &serviceClass;
    auto test = std::make_shared<NativeInstance>(), storage = std::make_shared<NativeInstance>(), relay = std::make_shared<NativeInstance>();
    test->kind = &testClass; storage->kind = &storageClass; relay->kind = &relayClass;
    std::vector<Instance> roots{test, storage}, children;
    model.children = &roots; storage->children = &children;
    Tuple input; AppendInstance(input, std::make_shared<int>(100), r.tableType);
    arrival_test::arguments = &input.values[0];
    auto slot = &Forward; auto execute = &arrival_test::ExecuteForward;
    r.slot = reinterpret_cast<std::uintptr_t>(&slot); r.token = 3; r.action = 1; r.lifetimeMs = 5000;
    r.getAttribute = reinterpret_cast<std::uintptr_t>(&EarlyAttribute);
    r.executeSlot = reinterpret_cast<std::uintptr_t>(&execute); r.executeOriginal = reinterpret_cast<std::uintptr_t>(&arrival_test::ExecuteForward);
    r.launchPlace = 12; r.launchUniverse = 7; strcpy_s(r.launchNonce, "owned-test-launch");
    r.launch = { reinterpret_cast<std::uintptr_t>(&edit), reinterpret_cast<std::uintptr_t>(&edit),
        reinterpret_cast<std::uintptr_t>(&arrival_test::Arguments), r.parentOffset, r.childrenOffset, r.classOffset,
        offsetof(EarlyModel, place), offsetof(EarlyModel, universe), r.tableType, r.getAttribute, r.stringTypeTable,
        offsetof(EarlyModel, self), offsetof(EarlyModel, context), ~std::uint64_t{}, reinterpret_cast<std::uintptr_t>(context.table), reinterpret_cast<std::uintptr_t>(&Submit) };
    Handle(r);
    const auto lease = current;
    unsigned accepted = 0, failed = 0;
    auto success = [&](Instance value) { assert(value); ++accepted; };
    auto error = [&](std::string) { ++failed; };
    // Unowned receivers and a replaced Edit nonce still take the engine path.
    model.universe = 8;
    execute(&model, *arrival_test::arguments, [](Variant) {}, {});
    slot(&service, 12, peers, options, success, error); assert(accepted == 1);
    model.universe = 7;
    play_loader_test::currentNonce = "replacement";
    execute(&edit, *arrival_test::arguments, [](Variant) {}, {});
    slot(&service, 12, peers, options, success, error); assert(accepted == 2);
    play_loader_test::currentNonce = "owned-test-launch";
    // Manual Play omits ExecutePlayModeAsync. Verified source ownership must
    // hold its early call until the matching runtime relay appears anyway.
    slot(&service, 12, peers, options, success, error);
    assert(accepted == 2 && failed == 0 && owner.uses == 2); // Queued before plugin startup.
    children.push_back(relay);
    Pump(lease); Drain(context);
    assert(completion && accepted == 2);
    completion(reply); completion = {}; failure = {};
    Pump(lease); Drain(context);
    assert(accepted == 3 && failed == 0 && owner.uses == 1);
    // Scope advancement cancels a pending old-server call exactly once.
    execute(&edit, *arrival_test::arguments, [](Variant) {}, {});
    children.clear();
    slot(&service, 12, peers, options, success, error);
    r.action = 5; strcpy_s(r.launchNonce, "next-launch"); Handle(r);
    Pump(lease); Drain(context); Pump(lease);
    assert(failed == 1 && owner.uses == 1);
    assert(LaunchScope(r.token, "next-launch") != nullptr);
    play_loader_test::currentNonce = "next-launch";
    execute(&edit, *arrival_test::arguments, [](Variant) {}, {});
    slot(&service, 12, peers, options, success, error);
    Pump(lease); // Leave a task queued while its launch ends.
    r.action = 2; Handle(r);
    Pump(lease); Drain(context); Pump(lease);
    assert(failed == 2 && accepted == 3 && owner.uses == 1);
    assert(slot == &Forward && execute == &arrival_test::ExecuteForward);
    // A daemon reconnect initially owns the existing runtime relay. Its first
    // transition upgrades that same lease before the next model starts.
    const auto launch = r;
    r.token = 4; r.action = 1; r.launchNonce[0] = 0; r.executeSlot = r.executeOriginal = 0;
    r.launch = {}; r.launchPlace = r.launchUniverse = 0;
    Handle(r);
    assert(!current->scope.load() && execute == &arrival_test::ExecuteForward);
    r = launch; r.token = 4; r.action = 5;
    Handle(r);
    assert(current->scope.load() && execute == &renium_play_teleport::ExecuteEntry);
    execute(&edit, *arrival_test::arguments, [](Variant) {}, {});
    children.push_back(relay);
    slot(&service, 12, peers, options, success, error);
    completion(reply); completion = {}; failure = {};
    Pump(current); Drain(context);
    assert(accepted == 4 && failed == 2 && owner.uses == 1);
    r.action = 2; Handle(r);
    assert(slot == &Forward && execute == &arrival_test::ExecuteForward);
    arrival_test::arguments = nullptr;
}
}

static void CaptionCleanup() {
    using namespace renium_play_caption;
    const auto window = CreateWindowExW(0, L"STATIC", L"source", 0, 0, 0, 0, 0, HWND_MESSAGE, nullptr, nullptr, nullptr);
    assert(window);
    Request r{}; r.version = 1; r.size = sizeof(r); r.action = 1; r.token = 91;
    r.window = reinterpret_cast<std::uintptr_t>(window); r.hostPid = GetCurrentProcessId();
    FILETIME created{}, exited{}, kernel{}, user{};
    assert(GetProcessTimes(GetCurrentProcess(), &created, &exited, &kernel, &user));
    r.hostCreation = static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime;
    wcscpy_s(r.ordinary, L"source"); wcscpy_s(r.written, L"source - Teleported to destination");
    Handle(r);
    SetWindowTextW(window, r.written);
    Restore(leases.at(window));
    wchar_t actual[128]{}; GetWindowTextW(window, actual, 128);
    assert(std::wstring(actual) == L"source");
    r.token = 92; Handle(r);
    SetWindowTextW(window, L"another document");
    Restore(leases.at(window));
    GetWindowTextW(window, actual, 128);
    assert(std::wstring(actual) == L"another document");
    r.token = 93; Handle(r);
    r.action = 3; r.token = 92; Handle(r);
    assert(leases.contains(window)); // A stale host cannot release its replacement.
    r.token = 93; Handle(r);
    assert(!leases.contains(window));
    DestroyWindow(window);
}

#include "continuation_windows.h"
static bool ThrowingSubmit(void*, void*, std::uint32_t mode) {
    if (mode == 1) throw std::bad_alloc();
    throw 42;
}
static bool UnknownThrowingSubmit(void*, void*, std::uint32_t) { throw 42; }
static void PackageBoundary(const char* path) {
    const auto helper = LoadLibraryA(path);
    assert(helper);
    const auto call = reinterpret_cast<DWORD(WINAPI*)(void*)>(GetProcAddress(helper, "ReniumPackageAction"));
    const auto configure = reinterpret_cast<DWORD(WINAPI*)(void*)>(GetProcAddress(helper, "ReniumTaskContinuations"));
    assert(call && configure);
    // Exercise the real exported remote-thread entry in this disposable host.
    // A valid owner remains retained by the test, and no engine task runs.
    alignas(8) std::uint64_t owner[2]{0, 2};
    for (const auto failure : {0, 1, 2}) for (const auto mode : {6, 7, 8}) {
        alignas(8) unsigned char params[688]{};
        const auto put64 = [&](unsigned at, std::uint64_t value) { std::memcpy(params + at, &value, 8); };
        const auto put32 = [&](unsigned at, std::uint32_t value) { std::memcpy(params + at, &value, 4); };
        put64(0, 1); put64(8, 1); put64(16, reinterpret_cast<std::uintptr_t>(owner));
        put32(40, 1000); put64(432, 1); put64(440, 1); put64(448, 1); put64(480, 1); put32(488, mode);
        put64(456, reinterpret_cast<std::uintptr_t>(failure == 2 ? &UnknownThrowingSubmit : &ThrowingSubmit) - 1);
        if (failure) { std::uint32_t abi = 2; assert(configure(&abi) == 0); }
        const auto result = call(params);
        assert(result == (failure == 2 ? 0xE312 : 0xE311));
        assert(*reinterpret_cast<std::uint32_t*>(params + 44) == result && params[160]);
        assert(owner[1] == 2);
    }
    FreeLibrary(helper);
}
int main(int argc, char** argv) {
    renium_continuation::abi.store(renium_continuation::Abi::Msvc);
    renium_continuation::taskAbi.store(renium_continuation::Abi::Msvc);
    assert(initial.size() == 512);
    const auto& allocator = renium_allocator::Engine();
    assert(allocator.allocate == &mi_malloc && allocator.release == &mi_free && allocator.aligned == &mi_malloc_aligned);
    continuation_test::Run();
    assert(argc == 2); PackageBoundary(argv[1]);
    play_loader_test::Run();
    teleport_dispatch_test::Run();
    CaptionCleanup();
    // Import resolution follows the bound IAT, and a partial contract refuses
    // rather than allocating in one heap and releasing in another.
    alignas(16) std::array<unsigned char, 4096> image{};
    const auto dos = reinterpret_cast<IMAGE_DOS_HEADER*>(image.data());
    dos->e_magic = IMAGE_DOS_SIGNATURE; dos->e_lfanew = 128;
    const auto nt = reinterpret_cast<IMAGE_NT_HEADERS64*>(image.data() + 128);
    nt->Signature = IMAGE_NT_SIGNATURE; nt->OptionalHeader.Magic = IMAGE_NT_OPTIONAL_HDR64_MAGIC;
    nt->OptionalHeader.SizeOfImage = static_cast<DWORD>(image.size());
    nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT] = {512, 40};
    const auto import = reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(image.data() + 512);
    import->Name = 768; import->OriginalFirstThunk = 1024; import->FirstThunk = 1280;
    std::memcpy(image.data() + 768, "mimalloc.dll", 13);
    auto names = reinterpret_cast<IMAGE_THUNK_DATA64*>(image.data() + 1024);
    auto addresses = reinterpret_cast<IMAGE_THUNK_DATA64*>(image.data() + 1280);
    names[0].u1.AddressOfData = 1536;
    std::memcpy(image.data() + 1538, "mi_malloc", 10);
    addresses[0].u1.Function = reinterpret_cast<std::uintptr_t>(&mi_malloc);
    auto partial = renium_allocator::ImportedAllocator(image.data());
    assert(partial.imported && !partial.Valid());
    import->Name = 2048; // An executable without mimalloc retains its CRT.
    const auto legacy = renium_allocator::ImportedAllocator(image.data());
    assert(!legacy.imported && legacy.Valid());
    mi_free(mi_malloc(1)); mi_free(mi_malloc_aligned(1, 64));
    renium_history::Binding previous{};
    previous.descriptor = 123; previous.table = 456; previous.memberOffset = 144;
    previous.finish = reinterpret_cast<std::uintptr_t>(&EngineFinish);
    auto predecessor = EngineHistoryPredecessor(previous, false);
    assert(renium_history::CertifiedPredecessor(predecessor, previous) == reinterpret_cast<renium_history::Finish>(&EngineFinish));
    predecessor = EngineHistoryPredecessor(previous, true);
    assert(renium_history::CertifiedPredecessor(predecessor, previous) == reinterpret_cast<renium_history::Finish>(predecessor));
    auto wrong = previous; ++wrong.descriptor;
    assert(!renium_history::CertifiedPredecessor(predecessor, wrong));
    assert(!renium_history::CertifiedPredecessor(previous.finish, previous));
    EngineHistoryPredecessor(previous, false);
    unsigned calls = 0;
    renium_history::originalFinish = reinterpret_cast<renium_history::Finish>(&EngineFinish);
    using TypedFinish = void (*)(void*, std::string, std::uint32_t, renium_history::Options);
    const auto hook = reinterpret_cast<TypedFinish>(&renium_history::ReniumHistoryFinish);
    for (unsigned i = 0; i < 100; ++i) {
        auto owner = std::shared_ptr<const void>(&calls, [](const void*) {});
        hook(&calls, std::string(80, 't'), 1, owner);
        assert(owner.use_count() == 1 && calls == i + 1);
        alignas(std::string) unsigned char storage[sizeof(std::string)];
        EngineGetString(storage);
        const auto value = reinterpret_cast<std::string*>(storage);
        assert(*value == std::string(8192, 'e'));
        value->~basic_string();
        bool called = false;
        // A heap-sized closure forces callback allocation across the boundary.
        EngineStoreCallback([&, padding = std::string(1024, 'p')](std::string text) {
            assert(padding.size() == 1024 && text == std::string(8192, 'c')); called = true;
        });
        EngineInvokeCallback();
        assert(called);
        auto aligned = ::operator new(100, std::align_val_t(64));
        assert(reinterpret_cast<std::uintptr_t>(aligned) % 64 == 0);
        ::operator delete(aligned, std::align_val_t(64));
    }
}
