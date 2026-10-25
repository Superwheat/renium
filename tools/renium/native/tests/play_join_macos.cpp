#include <cassert>
#include <cstdio>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include "../renium_studio_history.h"
#include "../renium_play_join_macos.h"

using namespace renium_mac_teleport;
namespace {
std::uint64_t stringTable = 10, tableType = 20;
const std::string modelName = "DataModel", testName = "StudioTestService",
    networkName = "NetworkClient", playerName = "Player", serviceName = "TeleportService";
struct Descriptor { std::uintptr_t unused = 0; const std::string* name; };
Descriptor modelClass{0, &modelName}, testClass{0, &testName}, networkClass{0, &networkName},
    playerClass{0, &playerName}, serviceClass{0, &serviceName};
struct Object {
    Descriptor* descriptor;
    Object* parent = nullptr;
    std::vector<Instance>* children = nullptr;
    std::uint64_t place = 303, universe = 202;
    std::string nonce = "arrival-test";
    std::shared_ptr<const void> join;
    Variant data;
};
void Copy(const void* source, void* target) { new(target) std::string(*static_cast<const std::string*>(source)); }
void Move(void* source, void* target) { new(target) std::string(std::move(*static_cast<std::string*>(source))); }
void Destroy(void* value) { static_cast<std::string*>(value)->~basic_string(); }
const Operations strings{Copy, Move, Destroy};
Variant Text(const std::string& text) {
    Variant value;
    value.type = reinterpret_cast<std::uintptr_t>(&stringTable);
    value.operations = reinterpret_cast<std::uintptr_t>(&strings);
    new(value.payload) std::string(text);
    return value;
}
Variant GetAttribute(void* object, StringView key) {
    assert(std::string(key.data, key.size) == "__ReniumLaunchNonce");
    return Text(static_cast<Object*>(object)->nonce);
}
unsigned joins = 0, locals = 0;
int JoinOriginal(void*, void* lua) { ++joins; assert(lua == &joins); return 1; }
Variant LocalOriginal(void* service) { ++locals; return static_cast<Object*>(service)->data; }
renium_mac_join::Join joinSlot = JoinOriginal;
renium_mac_join::Local localSlot = LocalOriginal;
renium_mac_join::Request Send(renium_mac_join::Request request) {
    std::vector<unsigned char> output;
    renium_mac_join::Handle(std::string(reinterpret_cast<const char*>(&request), sizeof(request)), output);
    assert(output.size() == sizeof(request));
    std::memcpy(&request, output.data(), sizeof(request));
    return request;
}
}

int main() {
    auto source = std::make_shared<Object>(); source->descriptor = &modelClass;
    auto test = std::make_shared<Object>(); test->descriptor = &testClass;
    auto network = std::make_shared<Object>(); network->descriptor = &networkClass;
    std::vector<Instance> serverRoots{test}, clientRoots{test, network};
    Object server{&modelClass}, client{&modelClass};
    server.children = &serverRoots; client.children = &clientRoots;
    Object player{&playerClass, &server}, clientPlayer{&playerClass, &client}, service{&serviceClass, &client};
    Object other{&modelClass}; other.children = &serverRoots; other.nonce = "unrelated";
    Object otherPlayer{&playerClass, &other};
    auto launch = std::make_shared<Lease>();
    auto& binding = launch->request;
    binding.token = 1; binding.place = 303; binding.universe = 202;
    binding.sourceModel = reinterpret_cast<std::uintptr_t>(source.get());
    binding.classOffset = offsetof(Object, descriptor); binding.parentOffset = offsetof(Object, parent);
    binding.childrenOffset = offsetof(Object, children); binding.placeField = offsetof(Object, place);
    binding.universeField = offsetof(Object, universe);
    binding.getAttribute = reinterpret_cast<std::uintptr_t>(&GetAttribute); binding.stringTypeTable = stringTable;
    binding.tableType = reinterpret_cast<std::uintptr_t>(&tableType);
    binding.hostPid = getpid();
    proc_bsdinfo info{};
    assert(proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info));
    binding.hostSeconds = info.pbi_start_tvsec; binding.hostMicros = info.pbi_start_tvusec;
    std::strcpy(binding.nonce, "arrival-test");
    launch->deadline = Clock::now() + std::chrono::seconds(30);
    void* owner = nullptr;
    std::memcpy(&owner, reinterpret_cast<const unsigned char*>(&source) + sizeof(void*), sizeof(owner));
    launch->root = std::make_shared<renium_history::WeakOwner>(owner);
    auto table = std::make_shared<int>(42);
    Tuple tuple;
    AppendInstance(tuple, table, binding.tableType);
    launch->arrival = std::make_shared<JoinRecord>(7, 303, tuple.values[0], Text("accepted data"));
    renium_mac_teleport::current = launch;
    renium_mac_join::Request r{};
    r.version = 1; r.size = sizeof(r); r.action = 1; r.token = 2; r.dispatch = 1; r.attempt = 7;
    r.place = 303; r.universe = 202;
    r.joinSlot = reinterpret_cast<std::uintptr_t>(&joinSlot); r.joinOriginal = reinterpret_cast<std::uintptr_t>(&JoinOriginal);
    r.localSlot = reinterpret_cast<std::uintptr_t>(&localSlot); r.localOriginal = reinterpret_cast<std::uintptr_t>(&LocalOriginal);
    r.joinField = offsetof(Object, join); r.localField = offsetof(Object, data);
    std::strcpy(r.nonce, "arrival-test");
    auto wrong = r; wrong.attempt = 6;
    bool rejected = false;
    try { Send(wrong); } catch (const std::exception&) { rejected = true; }
    assert(rejected && joinSlot == JoinOriginal && localSlot == LocalOriginal);
    Send(r);
    assert(joinSlot(&otherPlayer, &joins) == 1 && !otherPlayer.join);
    assert(joinSlot(&player, &joins) == 1 && player.join == table);
    assert(joinSlot(&clientPlayer, &joins) == 1 && clientPlayer.join == table);
    auto data = localSlot(&service);
    assert(*reinterpret_cast<const std::string*>(data.payload) == "accepted data");
    auto again = localSlot(&service);
    assert(*reinterpret_cast<const std::string*>(again.payload) == "accepted data");
    r.action = 3; assert(Send(r).receipt == 3);
    assert(joins == 3 && locals == 2);
    r.action = 2; Send(r);
    assert(joinSlot == JoinOriginal && localSlot == LocalOriginal);
    assert(player.join == table && clientPlayer.join == table);
    assert(*reinterpret_cast<const std::string*>(localSlot(&service).payload) == "accepted data");

    r.action = 1; ++r.token; Send(r);
    { std::lock_guard lock(renium_mac_teleport::mutex); launch->active = false; }
    Object late{&playerClass, &server};
    joinSlot(&late, &joins); assert(!late.join);
    std::this_thread::sleep_for(std::chrono::milliseconds(150));
    assert(joinSlot == JoinOriginal && localSlot == LocalOriginal);
    std::puts("Mac arrival preserves ABI, server/client data and ownership, rejects stale attempts, restores on Stop, and persists engine fields");
}
