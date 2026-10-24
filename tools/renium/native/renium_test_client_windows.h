#pragma once
#include "renium_teleport_gui_windows.h"

// The engine owns both the configuration schema and the document replacement.
// Only the replacement's final open is redirected from Edit to StartClient.
namespace renium_test_client {
using namespace renium_play_teleport;
struct Request {
    std::uint32_t version, size;
    renium_play_owner::Binding model;
    std::uint64_t relay, relayOwner, invoke, tableType;
    std::uint64_t window, opener, table, parse, replace, start, open, closeSlot;
    std::uint64_t calls[8];
    std::uint32_t count, reserved;
    std::uint64_t receiver, captureSelf, captureConfig, destroy, futureShared;
    char error[256];
};
static_assert(sizeof(Request) == 600);
struct Configuration {
    // An opaque engine-owned value. No Studio configuration fields are used.
    alignas(16) unsigned char bytes[65536]{};
    void(*destroy)(void*) = nullptr;
    std::size_t offset = 0;
    ~Configuration() { if (destroy) destroy(bytes + offset); }
};
inline std::mutex transitionMutex;
inline std::atomic<void*> retainedWidget{nullptr};
inline std::atomic<bool(*)(void*)> originalClose{nullptr};
inline void* StartClient(void*, void*, void*);
inline bool CloseWidget(void* widget) {
    void* expected = widget;
    if (retainedWidget.compare_exchange_strong(expected, nullptr)) return false;
    return originalClose.load()(widget);
}
struct Patches {
    Request request;
    unsigned installed = 0;
    void* close = nullptr;
    bool redirected = false;
    void* continuation = nullptr;
    static void Call(std::uint64_t at, std::uint64_t from, std::uint64_t to) {
        auto code = reinterpret_cast<unsigned char*>(at);
        std::int32_t previous = 0;
        std::memcpy(&previous, code + 1, sizeof(previous));
        const auto displacement = static_cast<std::int64_t>(to) - static_cast<std::int64_t>(at + 5);
        if (code[0] != 0xe8 || at + 5 + previous != from || displacement < INT32_MIN || displacement > INT32_MAX)
            throw std::runtime_error("Client replacement continuation changed");
        DWORD protection = 0, ignored = 0;
        if (!VirtualProtect(code, 5, PAGE_EXECUTE_READWRITE, &protection))
            throw std::runtime_error("Cannot redirect client replacement continuation");
        const auto next = static_cast<std::int32_t>(displacement);
        std::memcpy(code + 1, &next, sizeof(next));
        VirtualProtect(code, 5, protection, &ignored);
        FlushInstructionCache(GetCurrentProcess(), code, 5);
    }
    explicit Patches(const Request& r) : request(r) {}
    static void* Island(const std::uint64_t* calls, unsigned count, std::uintptr_t target) {
        // A near jump island preserves the native call ABI, including its
        // hidden return buffer, while letting the completion run in Tasks.
        SYSTEM_INFO system{};
        GetSystemInfo(&system);
        const auto granularity = static_cast<std::uintptr_t>(system.dwAllocationGranularity);
        auto low = calls[0] > INT32_MAX ? calls[0] - INT32_MAX : granularity;
        auto high = calls[0] + INT32_MAX;
        for (unsigned i = 1; i < count; ++i) {
            low = (std::max)(low, calls[i] > INT32_MAX ? calls[i] - INT32_MAX : granularity);
            high = (std::min)(high, calls[i] + INT32_MAX);
        }
        void* continuation = nullptr;
        for (auto at = (low + granularity - 1) & ~(granularity - 1); at < high;) {
            MEMORY_BASIC_INFORMATION region{};
            if (!VirtualQuery(reinterpret_cast<void*>(at), &region, sizeof(region))) break;
            if (region.State == MEM_FREE) {
                continuation = VirtualAlloc(reinterpret_cast<void*>(at), granularity, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
                if (continuation) break;
            }
            const auto end = reinterpret_cast<std::uintptr_t>(region.BaseAddress) + region.RegionSize;
            if (end <= at) break;
            at = (end + granularity - 1) & ~(granularity - 1);
        }
        if (!continuation) throw std::runtime_error("Cannot allocate client continuation");
        unsigned char jump[14]{0xff, 0x25, 0, 0, 0, 0};
        std::memcpy(jump + 6, &target, sizeof(target));
        std::memcpy(continuation, jump, sizeof(jump));
        DWORD protection = 0;
        if (!VirtualProtect(continuation, granularity, PAGE_EXECUTE_READ, &protection)) {
            VirtualFree(continuation, 0, MEM_RELEASE);
            throw std::runtime_error("Cannot protect client continuation");
        }
        FlushInstructionCache(GetCurrentProcess(), continuation, sizeof(jump));
        return continuation;
    }
    void Arm() {
        continuation = Island(request.calls, request.count, reinterpret_cast<std::uintptr_t>(&StartClient));
        const auto widgets = GetModuleHandleW(L"Qt5Widgets.dll");
        const auto find = reinterpret_cast<void*(*)(std::uintptr_t)>(GetProcAddress(widgets, "?find@QWidget@@SAPEAV1@_K@Z"));
        close = reinterpret_cast<void*>(GetProcAddress(widgets, "?close@QWidget@@QEAA_NXZ"));
        auto widget = find ? find(request.window) : nullptr;
        if (!widget || !close) throw std::runtime_error("Client window is unavailable");
        originalClose.store(reinterpret_cast<bool(*)(void*)>(close));
        retainedWidget.store(widget);
        if (!renium_play_loader::Exchange(request.closeSlot, close, reinterpret_cast<void*>(&CloseWidget)))
            throw std::runtime_error("Client window close binding changed");
        redirected = true;
        for (; installed < request.count; ++installed) Call(request.calls[installed], request.open, reinterpret_cast<std::uintptr_t>(continuation));
    }
    ~Patches() {
        while (installed) {
            try { Call(request.calls[--installed], reinterpret_cast<std::uintptr_t>(continuation), request.open); } catch (...) {}
        }
        retainedWidget.store(nullptr);
        if (redirected) renium_play_loader::Exchange(request.closeSlot, reinterpret_cast<void*>(&CloseWidget), close);
        if (continuation) VirtualFree(continuation, 0, MEM_RELEASE);
    }
};
struct Transition {
    Patches patches;
    HANDLE finished = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    std::string error;
    alignas(16) unsigned char future[65536]{};
    explicit Transition(const Request& request) : patches(request) {
        if (!finished) throw std::runtime_error("Cannot create client travel receipt");
    }
    ~Transition() {
        void* owner = nullptr;
        std::memcpy(&owner, future + patches.request.futureShared + sizeof(void*), sizeof(owner));
        if (owner) ReleaseOwnerReference(owner);
        if (finished) CloseHandle(finished);
    }
};
inline std::mutex pendingMutex;
inline std::shared_ptr<Transition> pending;
inline void* StartClient(void* self, void* result, void* config) {
    std::shared_ptr<Transition> transition;
    { std::lock_guard lock(pendingMutex); transition = pending; }
    const auto& request = transition->patches.request;
    void* output = nullptr;
    try {
        output = reinterpret_cast<void*(*)(void*, void*, void*)>(request.start)(self, result, config);
    } catch (...) {
        transition->error = "Native client start failed";
        SetEvent(transition->finished);
        { std::lock_guard lock(pendingMutex); pending.reset(); }
        throw;
    }
    SetEvent(transition->finished);
    { std::lock_guard lock(pendingMutex); pending.reset(); }
    return output;
}
inline void Handle(Request& request) {
    if (request.version != 2 || request.size != sizeof(request) || request.reserved
        || !request.relay || !request.relayOwner || !request.invoke || !request.tableType
        || !request.opener || !request.table || !request.parse || !request.replace
        || !request.start || !request.open || !request.closeSlot || request.count == 0 || request.count > 8
        || !request.destroy || request.receiver >= 65536 || request.captureSelf > 65528
        || request.captureConfig >= 32768 || request.futureShared > 65520)
        throw std::runtime_error("Incompatible same-window client binding");
    std::unique_lock transition(transitionMutex, std::try_to_lock);
    if (!transition.owns_lock()) throw std::runtime_error("This client is already traveling");
    DWORD pid = 0;
    GetWindowThreadProcessId(reinterpret_cast<HWND>(request.window), &pid);
    if (pid != GetCurrentProcessId() || renium_play_loader::Word(reinterpret_cast<void*>(request.opener), 0) != request.table)
        throw std::runtime_error("Client document owner changed");
    { std::lock_guard lock(pendingMutex); if (pending) throw std::runtime_error("Native client travel is still pending"); }
    auto configuration = std::make_shared<Configuration>();
    std::memcpy(configuration->bytes + request.captureSelf, &request.opener, sizeof(request.opener));
    auto task = std::make_shared<renium_teleport_gui::Task>();
    task->request.model = request.model;
    auto model = reinterpret_cast<void*>(request.model.sourceModel);
    task->modelHold = renium_play_owner::Hold(model, request.model);
    auto relay = reinterpret_cast<void*>(request.relay);
    if (renium_play_owner::Model(relay, request.model) != model || !Class(relay, request.model.classOffset, "BindableFunction"))
        throw std::runtime_error("Client route belongs to another DataModel");
    if (!AddOwnerReference(reinterpret_cast<void*>(request.relayOwner))) throw std::runtime_error("Client route ended");
    task->relayHold = std::shared_ptr<void>(reinterpret_cast<void*>(request.relayOwner), ReleaseOwnerReference);
    const auto input = request;
    if (!renium_play_owner::Submit(model, request.model, [task, configuration, input] {
        if (!task->active.load()) { task->Finish(); return; }
        try {
            reinterpret_cast<Invoke>(input.invoke)(reinterpret_cast<void*>(input.relay), std::make_shared<Tuple>(),
                [task, configuration, input](Values values) {
                    try {
                        if (!task->active.load()) { task->Finish(); return; }
                        if (!values || values->values.size() != 1 || values->values[0].type != input.tableType)
                            throw std::runtime_error("Client route configuration changed type");
                        reinterpret_cast<void*(*)(void*, const Variant*)>(input.parse)(configuration->bytes + input.captureConfig, &values->values[0]);
                        configuration->offset = input.captureConfig;
                        configuration->destroy = reinterpret_cast<void(*)(void*)>(input.destroy);
                        task->Finish();
                    } catch (const std::exception& e) { task->Finish(e.what()); }
                }, [task](std::string error) { task->Finish(error); });
        } catch (const std::exception& e) { task->Finish(e.what()); }
    })) throw std::runtime_error("Client route queue ended");
    const auto wait = WaitForSingleObject(task->finished, 3000);
    task->active.store(false);
    if (wait != WAIT_OBJECT_0) throw std::runtime_error("Client route configuration timed out");
    if (!task->error.empty()) throw std::runtime_error(task->error);
    task.reset(); // Never hold the departing DataModel across its teardown.
    auto operation = std::make_shared<Transition>(request);
    operation->patches.Arm();
    { std::lock_guard lock(pendingMutex); pending = operation; }
    // The scheduler accepts external threads; its synchronous public wrapper
    // requires a running Tasks fiber. The engine copies this capture itself.
    std::string label = "ClientTravel";
    try {
        reinterpret_cast<void*(*)(void*, void*, std::string*, void*)>(request.replace)(
            reinterpret_cast<void*>(request.opener + request.receiver), operation->future, &label, configuration->bytes);
    } catch (...) {
        { std::lock_guard lock(pendingMutex); pending.reset(); }
        throw;
    }
    configuration.reset();
    if (WaitForSingleObject(operation->finished, 25000) != WAIT_OBJECT_0)
        throw std::runtime_error("Native client replacement timed out");
    if (!operation->error.empty()) throw std::runtime_error(operation->error);
}
}
extern "C" __declspec(dllexport) DWORD WINAPI ReniumTestClient(void* input) {
    auto& request = *static_cast<renium_test_client::Request*>(input);
    try { renium_test_client::Handle(request); return 0; }
    catch (const std::exception& error) { strncpy_s(request.error, error.what(), _TRUNCATE); return 1; }
}
