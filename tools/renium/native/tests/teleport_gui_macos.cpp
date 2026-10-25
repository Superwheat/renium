#include <cassert>
#include <cstdio>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include "../renium_studio_history.h"
#include "../renium_teleport_gui_macos.h"

namespace {
using namespace renium_mac_gui;
struct Descriptor { std::uintptr_t unused = 0; const std::string* name; };
struct Object {
    std::uintptr_t unused = 0;
    std::weak_ptr<void> self;
    Descriptor* descriptor = nullptr;
    Object* parent = nullptr;
};
unsigned forwarded = 0, loaded = 0, removed = 0;
bool reject = false;
Variant GetAttribute(void*, StringView) { ++forwarded; return {}; }
bool ReadyOriginal(void*) { return true; }
AttributeGetter attributeSlot = GetAttribute;
Ready finishedSlot = ReadyOriginal, removedSlot = ReadyOriginal;
void Add(void* context, std::string name, Instance core) {
    assert(context && core && name == "LoadingScript");
    if (reject) throw std::runtime_error("CoreScript rejected");
    ++loaded;
}
void Remove(void*) { ++removed; }
renium_mac_gui::Request Send(renium_mac_gui::Request r) {
    std::vector<unsigned char> output;
    renium_mac_gui::Handle(std::string(reinterpret_cast<const char*>(&r), sizeof(r)), output);
    assert(output.size() == sizeof(r));
    std::memcpy(&r, output.data(), sizeof(r));
    return r;
}
std::uint64_t Address(const void* value) { return reinterpret_cast<std::uintptr_t>(value); }
std::uint64_t Owner(const Instance& value) { return Word(&value, 8); }
}

int main() {
    using namespace renium_mac_gui;
    const std::string modelName = "DataModel", relayName = "BindableFunction", contextName = "ScriptContext",
        coreName = "CoreGui", firstName = "ReplicatedFirst";
    Descriptor modelClass{0, &modelName}, relayClass{0, &relayName}, contextClass{0, &contextName},
        coreClass{0, &coreName}, firstClass{0, &firstName};
    auto make = [](Descriptor* type, Object* parent) {
        auto value = std::make_shared<Object>();
        value->self = value; value->descriptor = type; value->parent = parent;
        return value;
    };
    auto model = make(&modelClass, nullptr);
    auto core = make(&coreClass, model.get()), context = make(&contextClass, model.get());
    auto first = make(&firstClass, model.get()), relay = make(&relayClass, core.get());
    auto other = make(&firstClass, model.get());
    renium_mac_gui::Request r{};
    r.version = 1; r.size = sizeof(r); r.action = 1; r.token = 1; r.show = 1;
    r.model = Address(model.get()); r.modelOwner = Owner(model);
    r.relay = Address(relay.get()); r.relayOwner = Owner(relay);
    r.core = Address(core.get()); r.context = Address(context.get()); r.first = Address(first.get());
    r.attributeSlot = Address(&attributeSlot); r.attributeOriginal = Address(reinterpret_cast<void*>(&GetAttribute));
    r.add = Address(reinterpret_cast<void*>(&Add)); r.remove = Address(reinterpret_cast<void*>(&Remove));
    r.finishedSlot = Address(&finishedSlot); r.finishedOriginal = Address(reinterpret_cast<void*>(&ReadyOriginal));
    r.removedSlot = Address(&removedSlot); r.removedOriginal = r.finishedOriginal;
    r.selfOffset = offsetof(Object, self); r.classOffset = offsetof(Object, descriptor); r.parentOffset = offsetof(Object, parent);
    r.hostPid = getpid();
    proc_bsdinfo info{};
    assert(proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info));
    r.hostSeconds = info.pbi_start_tvsec; r.hostMicros = info.pbi_start_tvusec;
    constexpr char key[] = "__ReniumNativeGuiInvoke";
    const StringView trigger{key, sizeof(key) - 1};
    Send(r);
    attributeSlot(other.get(), trigger); attributeSlot(relay.get(), {"other", 5});
    assert(forwarded == 2 && loaded == 0);
    attributeSlot(relay.get(), trigger);
    assert(forwarded == 3 && loaded == 1 && attributeSlot == GetAttribute);
    assert(!finishedSlot(first.get()) && !removedSlot(first.get()));
    assert(finishedSlot(other.get()) && removedSlot(other.get()));
    r.action = 3; assert(Send(r).receipt == 2);
    r.action = 2; Send(r);
    r.action = 1; ++r.token; r.show = 0; Send(r);
    attributeSlot(relay.get(), trigger);
    assert(removed == 1 && finishedSlot == ReadyOriginal && removedSlot == ReadyOriginal);
    r.action = 2; Send(r);

    // A real loader exception must reach the receipt and restore every binding.
    r.action = 1; ++r.token; r.show = 1; reject = true; Send(r);
    attributeSlot(relay.get(), trigger);
    r.action = 3;
    bool failed = false;
    try { Send(r); } catch (const std::exception& e) { failed = std::string(e.what()) == "CoreScript rejected"; }
    assert(failed && attributeSlot == GetAttribute && finishedSlot == ReadyOriginal && removedSlot == ReadyOriginal);
    r.action = 2; Send(r); reject = false;

    // Stop destroys the receiver; its watchdog must release the loading gates.
    r.action = 1; ++r.token; Send(r); attributeSlot(relay.get(), trigger);
    r.action = 2; Send(r); first.reset();
    std::this_thread::sleep_for(std::chrono::milliseconds(250));
    assert(finishedSlot == ReadyOriginal && removedSlot == ReadyOriginal);
    r.action = 1; ++r.token; Send(r); relay.reset();
    std::this_thread::sleep_for(std::chrono::milliseconds(250));
    assert(attributeSlot == GetAttribute);
    std::puts("Mac CoreGui forwarding, native loader, receiver isolation, error propagation and Stop cleanup passed");
}
