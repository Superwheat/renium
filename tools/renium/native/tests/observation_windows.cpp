#include <Windows.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <cassert>
#include <functional>
#include <memory>
#include <string>
#include <vector>
#include "../renium_capture_context.h"

// Synthetic engine, relocated fields, and a virtual clock: no Studio process.
struct SharedVector { SharedInstance* begin; SharedInstance* end; };
static std::size_t CaptureChildCount(const SharedVector* v) { return v->end - v->begin; }
static bool AddOwnerReference(void* p) { ++*static_cast<unsigned*>(p); return true; }
static void ReleaseOwnerReference(void* p) { assert(*static_cast<unsigned*>(p)); --*static_cast<unsigned*>(p); }
static void SetError(ReniumSerializerParams* p, const char* text) { strcpy_s(p->error, text); }
static DWORD PublishCaptureResponse(ReniumSerializerParams*, DWORD result) { return result; }
using SubmitDataModelTask = bool(*)(void*, std::function<void()>*, int);
static ULONGLONG ticks;
static unsigned renewals, subscriptions, disconnections, notifications;
static void* currentNode;
static unsigned changes;
static DWORD terminal;
static bool delayFinish;
static std::function<void()> pending, duringRenewal;
static DWORD WindowPid(HWND, DWORD* pid) { *pid = GetCurrentProcessId(); return 1; }
static ULONGLONG Clock() { return ticks; }
static DWORD WaitOne(HANDLE event, DWORD timeout) {
    return pending ? WAIT_TIMEOUT : WaitForSingleObject(event, timeout);
}
static DWORD WaitMany(DWORD count, const HANDLE*, BOOL, DWORD timeout) {
    assert(count == 3 && timeout <= 120000);
    if (renewals) {
        --renewals;
        ticks += 30000;
        if (duringRenewal) duringRenewal();
        return WAIT_OBJECT_0 + 2;
    }
    if (terminal == WAIT_TIMEOUT) ticks += timeout;
    return terminal;
}
#define GetWindowThreadProcessId WindowPid
#define GetTickCount64 Clock
#define WaitForMultipleObjects WaitMany
#define WaitForSingleObject WaitOne
#include "../renium_studio_observation.h"
#undef GetWindowThreadProcessId
#undef GetTickCount64
#undef WaitForMultipleObjects
#undef WaitForSingleObject

static bool Submit(void*, std::function<void()>* work, int) {
    if (delayFinish && subscriptions) pending = *work;
    else (*work)();
    return true;
}
static std::array<LONG, 4> signal{};
static void Ensure(void** p) { *p = signal.data(); }
static void* Allocate(std::size_t size) { return new unsigned char[size]; }
static void Append(void*, void* node) { ++subscriptions; currentNode = node; }
static void Disconnect(void**) { ++disconnections; }
static void Assign(void** connection, void**) { delete[] static_cast<unsigned char*>(*connection); *connection = nullptr; }
static void Fire(void*, const SharedInstance*) { ++notifications; }

struct Fixture {
    alignas(16) std::array<unsigned char, 512> model{}, root{}, relay{};
    unsigned modelRefs = 0, rootRefs = 0, relayRefs = 0;
    SharedInstance child{root.data(), &rootRefs};
    SharedVector children{&child, &child + 1};
    renium_observation::Params p{};
    explicit Fixture(unsigned offset) {
        auto& b = p.base;
        b.dataModel = reinterpret_cast<std::uintptr_t>(model.data());
        b.dataModelOwner = reinterpret_cast<std::uintptr_t>(&modelRefs);
        b.selfOffset = offset; b.parentOffset = offset + 32; b.childrenOffset = offset + 48;
        b.moduleBase = reinterpret_cast<std::uintptr_t>(GetModuleHandleW(nullptr));
        b.processId = GetCurrentProcessId(); b.window = 1; b.count = 1; b.roots[0] = child;
        b.submitTask = reinterpret_cast<std::uintptr_t>(&Submit); b.taskContext = 1;
        b.timeoutMs = 1800000;
        *reinterpret_cast<SharedInstance*>(model.data() + offset) = {model.data(), &modelRefs};
        *reinterpret_cast<SharedInstance*>(root.data() + offset) = child;
        p.relay = {relay.data(), &relayRefs};
        *reinterpret_cast<SharedInstance*>(relay.data() + offset) = p.relay;
        *reinterpret_cast<void**>(root.data() + b.parentOffset) = model.data();
        *reinterpret_cast<void**>(relay.data() + b.parentOffset) = root.data();
        *reinterpret_cast<SharedVector**>(model.data() + b.childrenOffset) = &children;
        p.signalOffset = offset + 64;
        p.ensure = reinterpret_cast<std::uintptr_t>(&Ensure);
        p.allocate = reinterpret_cast<std::uintptr_t>(&Allocate);
        p.append = reinterpret_cast<std::uintptr_t>(&Append);
        p.disconnect = reinterpret_cast<std::uintptr_t>(&Disconnect);
        p.assign = reinterpret_cast<std::uintptr_t>(&Assign);
        p.fireRelay = reinterpret_cast<std::uintptr_t>(&Fire); p.relayOffset = 8;
        p.stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        p.ready = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        p.host = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        p.renew = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    }
    ~Fixture() {
        assert(!modelRefs && !rootRefs && !relayRefs);
        for (auto event : {p.stop, p.ready, p.host, p.renew}) CloseHandle(event);
    }
};

int main() {
    for (unsigned offset : {16u, 96u, 256u}) {
        for (unsigned mode = 0; mode != 6; ++mode) {
            Fixture f(offset);
            ticks = 0; subscriptions = disconnections = notifications = changes = 0;
            renewals = mode == 0 ? 20 : mode == 4 ? 100 : mode == 3 ? 1 : 0;
            terminal = mode == 1 ? WAIT_OBJECT_0 + 1 : mode == 2 ? WAIT_TIMEOUT : WAIT_OBJECT_0;
            delayFinish = mode == 5;
            duringRenewal = [&] {
                assert(f.modelRefs == 1 && f.rootRefs == 1 && f.relayRefs == 1);
                assert(subscriptions == 1 && disconnections == 0);
                for (unsigned change = 0; change != 100; ++change) {
                    AddOwnerReference(f.child.owner);
                    renium_observation::Changed(currentNode, &f.child, f.p.descriptor);
                    ++changes;
                }
                assert(f.rootRefs == 1);
                if (mode == 3)
                    *reinterpret_cast<void**>(f.relay.data() + f.p.base.parentOffset) = nullptr;
            };
            const auto result = renium_observation::Run(&f.p);
            if (mode == 0) assert(result == 0 && ticks == 600000);
            else assert(result != 0);
            if (mode == 2) assert(ticks == 120000); // missed renewal remains bounded
            if (mode == 4) assert(ticks == 1800000); // renewals cannot defeat total lifetime
            if (mode == 5) {
                assert(pending && f.modelRefs == 1 && f.rootRefs == 1 && f.relayRefs == 1);
                auto cleanup = std::move(pending); pending = {}; cleanup(); cleanup = {};
            }
            assert(subscriptions == 1 && disconnections == 1 && notifications == changes + 2);
            assert(!f.modelRefs && !f.rootRefs && !f.relayRefs);
        }
        Fixture f(offset);
        auto first = std::make_shared<renium_observation::Task>(); first->params = f.p;
        auto second = std::make_shared<renium_observation::Task>(); second->params = f.p;
        renium_observation::Begin(first); renium_observation::Begin(second);
        assert(f.rootRefs == 2 && f.relayRefs == 2);
        renium_observation::Finish(first.get());
        renium_observation::Begin(second); // Old guard cleanup cannot own the new callback.
        assert(f.rootRefs == 1 && f.relayRefs == 1);
        renium_observation::Finish(second.get());
    }
}
