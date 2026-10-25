#include <cassert>
#include <cstdio>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include "../renium_studio_history.h"
#include "../renium_teleport_dispatch_macos.h"

using namespace renium_mac_teleport;
namespace {
std::uint64_t stringTable = 10, tableType = 20, instanceType = 30;
std::string runtime = "owned-runtime";
struct Descriptor { std::uintptr_t unused = 0; const std::string* name; };
const std::string modelName = "DataModel", storageName = "ServerStorage", relayName = "BindableFunction", resultName = "TeleportAsyncResult";
Descriptor modelClass{0, &modelName}, storageClass{0, &storageName}, relayClass{0, &relayName}, resultClass{0, &resultName};
struct Object { Descriptor* descriptor; Object* parent; std::vector<Instance>* children; };
void Copy(const void* source, void* target) { new(target) std::string(*static_cast<const std::string*>(source)); }
void Move(void* source, void* target) { new(target) std::string(std::move(*static_cast<std::string*>(source))); }
void Destroy(void* value) { static_cast<std::string*>(value)->~basic_string(); }
const renium_mac_teleport::Operations strings{Copy, Move, Destroy};
Variant Text(const std::string& text) {
    Variant value;
    value.type = reinterpret_cast<std::uintptr_t>(&stringTable);
    value.operations = reinterpret_cast<std::uintptr_t>(&strings);
    new(value.payload) std::string(text);
    return value;
}
Variant GetAttribute(void*, StringView key) {
    assert(std::string(key.data, key.size) == "__ReniumNative");
    return Text(runtime);
}
Instance Create(void*, void* unused, bool allow) {
    assert(!unused && allow);
    return std::make_shared<Object>(Object{&resultClass, nullptr, nullptr});
}
void* createTable[] = {reinterpret_cast<void*>(&Create)};
void* factory = createTable;
unsigned forwarded = 0, invoked = 0;
std::function<void(Values)> complete;
void InvokeFunction(void*, Values args, std::function<void(Values)> success, Failure) {
    ++invoked;
    assert(args && args->values.size() == 3);
    assert(*reinterpret_cast<const std::string*>(args->values[0].payload) == "303");
    assert(args->values[1].type == reinterpret_cast<std::uintptr_t>(&instanceType));
    assert(*reinterpret_cast<const Instance*>(args->values[1].payload));
    assert(*reinterpret_cast<const Instance*>(args->values[2].payload));
    complete = std::move(success);
}
void Original(void*, std::int64_t place, Players players, Instance options, Success success, Failure failure) {
    assert(place == 303 && players && players->size() == 1 && options && success && failure);
    ++forwarded;
    success(options);
}
Teleport slot = Original;
Request Send(Request r) {
    std::vector<unsigned char> output;
    Handle(std::string(reinterpret_cast<const char*>(&r), sizeof(r)), output);
    assert(output.size() == sizeof(r));
    std::memcpy(&r, output.data(), sizeof(r));
    return r;
}
}

int main() {
    auto model = std::make_shared<Object>(Object{&modelClass, nullptr, nullptr});
    auto storage = std::make_shared<Object>(Object{&storageClass, model.get(), nullptr});
    auto relay = std::make_shared<Object>(Object{&relayClass, storage.get(), nullptr});
    std::vector<Instance> roots{storage}, children{relay};
    model->children = &roots; storage->children = &children;
    Object service{nullptr, model.get(), nullptr};
    auto options = std::make_shared<int>(7);
    auto player = std::make_shared<int>(8);
    auto players = std::make_shared<std::vector<Instance>>(std::initializer_list<Instance>{player});
    Request r{};
    r.version = 2; r.size = sizeof(r); r.action = 1; r.token = 1;
    r.slot = reinterpret_cast<std::uintptr_t>(&slot); r.original = reinterpret_cast<std::uintptr_t>(&Original);
    r.invoke = reinterpret_cast<std::uintptr_t>(&InvokeFunction); r.getAttribute = reinterpret_cast<std::uintptr_t>(&GetAttribute);
    r.instanceType = reinterpret_cast<std::uintptr_t>(&instanceType); r.stringTypeTable = stringTable;
    r.tableType = reinterpret_cast<std::uintptr_t>(&tableType);
    r.resultFactory = reinterpret_cast<std::uintptr_t>(&factory); r.resultCreate = reinterpret_cast<std::uintptr_t>(&Create);
    r.parentOffset = offsetof(Object, parent); r.childrenOffset = offsetof(Object, children); r.classOffset = offsetof(Object, descriptor);
    r.hostPid = getpid(); r.lifetimeMs = 30000;
    proc_bsdinfo info{};
    assert(proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info));
    r.hostSeconds = info.pbi_start_tvsec; r.hostMicros = info.pbi_start_tvusec;
    std::strcpy(r.runtime, runtime.c_str());
    Send(r);
    assert(slot != Original);

    unsigned succeeded = 0, failed = 0;
    slot(&service, 303, players, options, [&](Instance value) {
        assert(Class(value.get(), r.classOffset, "TeleportAsyncResult")); ++succeeded;
    }, [&](std::string) { ++failed; });
    assert(invoked == 1 && succeeded == 0 && failed == 0);
    auto response = std::make_shared<Tuple>();
    AppendInstance(*response, player, r.tableType);
    response->values.emplace_back(Text("data"));
    response->values.emplace_back(Text("42"));
    complete(response);
    assert(succeeded == 1 && failed == 0);
    complete = {};
    r.action = 4; r.attempt = 42; r.destination = 303;
    assert(Send(r).receipt == 1);
    r.attempt = 41;
    assert(Send(r).receipt == 0);

    slot(&service, 303, players, options, [&](Instance) { ++succeeded; }, [&](std::string message) {
        assert(message == "rejected"); ++failed; throw std::runtime_error("original continuation");
    });
    auto rejected = std::make_shared<Tuple>();
    rejected->values.emplace_back(Text("rejected"));
    bool propagated = false;
    try { complete(rejected); }
    catch (const std::runtime_error& error) { propagated = std::string(error.what()) == "original continuation"; }
    assert(propagated && failed == 1 && succeeded == 1);
    complete = {};

    runtime = "unrelated-runtime";
    slot(&service, 303, players, options, [&](Instance value) { assert(value == options); ++succeeded; }, [&](std::string) { ++failed; });
    assert(forwarded == 1 && invoked == 2 && succeeded == 2 && failed == 1);
    r.action = 2;
    Send(r);
    assert(slot == Original);
    std::puts("Mac dispatch preserves ABI, async result/data, error continuation, unrelated forwarding and cancellation");
}
