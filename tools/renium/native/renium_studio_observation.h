// One leased native signal subscription replaces per-instance Lua attribute
// subscriptions during export, push, or a retained baseline. It observes;
// it never suppresses Studio's events or other subscribers.
#include "renium_signal_append.h"
#include <algorithm>
namespace renium_observation {
struct Params {
    ReniumSerializerParams base;
    std::uint32_t signalOffset, appendAbi;
    std::uintptr_t ensure, allocate, append, disconnect, assign;
    void* descriptor;
    HANDLE stop, ready;
    SharedInstance relay;
    std::uintptr_t relayOffset, fireRelay;
    HANDLE host, renew;
};
static_assert(sizeof(Params) == sizeof(ReniumSerializerParams) + 120);
static_assert(offsetof(Params, signalOffset) == sizeof(ReniumSerializerParams));
static_assert(offsetof(Params, appendAbi) == sizeof(ReniumSerializerParams) + 4);

struct Task {
    Params params{};
    void* connection = nullptr;
    bool changed = false;
    bool relayArmed = false;
    std::atomic<bool> cancelled{false};
    std::vector<void*> owners;
    std::shared_ptr<void> modelHold;
    std::shared_ptr<Task> installed;
    std::string error;
};

static void Fire(Task* task, const SharedInstance* input) {
    const auto& p = task->params;
    reinterpret_cast<void(__fastcall*)(void*, const SharedInstance*)>(p.fireRelay)(
        static_cast<unsigned char*>(p.relay.instance) + p.relayOffset, input);
}

static void __fastcall Changed(void* node, const SharedInstance* input, void* descriptor) {
    auto task = *reinterpret_cast<Task**>(static_cast<unsigned char*>(node) + 0x28);
    const auto& p = task->params;
    if (input->instance == p.relay.instance && descriptor == p.descriptor) {
        ReleaseOwnerReference(input->owner);
        return;
    }
    if ((!task->changed || p.relay.instance) && descriptor == p.descriptor) {
        auto instance = input->instance;
        std::size_t depth = 0;
        while (instance && depth++ < 4096) {
            bool included = false;
            for (std::size_t index = 0; index < p.base.count; ++index)
                if (instance == p.base.roots[index].instance) { included = true; break; }
            if (included) {
                if (p.relay.instance) Fire(task, input);
                else task->changed = true;
                break;
            }
            instance = *reinterpret_cast<void**>(static_cast<unsigned char*>(instance) + p.base.parentOffset);
        }
        if (depth >= 4096) task->changed = true;
    }
    ReleaseOwnerReference(input->owner);
}

static void Finish(Task* task) {
    auto& p = task->params;
    if (task->relayArmed) {
        task->relayArmed = false;
        // Restore local listeners before unarming, including timeout/cancel.
        // Normal success has already released tracking and this has no listener.
        SharedInstance empty{};
        Fire(task, &empty);
    }
    if (task->connection) {
        reinterpret_cast<void(__fastcall*)(void**)>(p.disconnect)(&task->connection);
        void* empty = nullptr;
        reinterpret_cast<void(__fastcall*)(void**, void**)>(p.assign)(&task->connection, &empty);
    }
    while (!task->owners.empty()) {
        ReleaseOwnerReference(task->owners.back());
        task->owners.pop_back();
    }
    task->installed.reset();
}

static void Begin(const std::shared_ptr<Task>& task) {
    const auto& p = task->params;
    if (task->cancelled.load()) return;
    DWORD pid = 0;
    GetWindowThreadProcessId(reinterpret_cast<HWND>(p.base.window), &pid);
    if (pid != GetCurrentProcessId() || p.base.processId != pid
        || p.base.moduleBase != reinterpret_cast<std::uintptr_t>(GetModuleHandleW(nullptr)))
        throw std::runtime_error("Attribute observation target changed");
    const auto instance = reinterpret_cast<unsigned char*>(p.base.dataModel) + p.base.dataModelInstanceOffset;
    const auto self = reinterpret_cast<const SharedInstance*>(instance + p.base.selfOffset);
    if (self->instance != instance || reinterpret_cast<std::uintptr_t>(self->owner) != p.base.dataModelOwner)
        throw std::runtime_error("Attribute observation DataModel changed");
    const auto children = *reinterpret_cast<const SharedVector* const*>(instance + p.base.childrenOffset);
    const auto childCount = CaptureChildCount(children);
    if (childCount > 256) throw std::runtime_error("Attribute observation service list changed");
    for (std::size_t index = 0; index < p.base.count; ++index) {
        const auto& root = p.base.roots[index];
        bool present = false;
        for (std::size_t child = 0; child < childCount; ++child)
            if (children->begin[child].instance == root.instance && children->begin[child].owner == root.owner) { present = true; break; }
        if (!present) throw std::runtime_error("Attribute observation service was removed");
        const auto identity = reinterpret_cast<const SharedInstance*>(static_cast<unsigned char*>(root.instance) + p.base.selfOffset);
        if (identity->instance != root.instance || identity->owner != root.owner
            || *reinterpret_cast<void**>(static_cast<unsigned char*>(root.instance) + p.base.parentOffset) != instance)
            throw std::runtime_error("Attribute observation service changed");
        if (!task->connection) {
            if (!AddOwnerReference(root.owner)) throw std::runtime_error("Attribute observation service expired");
            task->owners.push_back(root.owner);
        }
    }
    if (p.relay.instance) {
        const auto self = reinterpret_cast<const SharedInstance*>(
            static_cast<unsigned char*>(p.relay.instance) + p.base.selfOffset);
        if (self->instance != p.relay.instance || self->owner != p.relay.owner
            || !p.fireRelay || !p.relayOffset || p.relayOffset >= 0x1000 || p.relayOffset % 8)
            throw std::runtime_error("Native attribute relay changed");
        auto parent = p.relay.instance;
        for (unsigned depth = 0; parent && parent != instance && depth < 4096; ++depth)
            parent = *reinterpret_cast<void**>(static_cast<unsigned char*>(parent) + p.base.parentOffset);
        if (parent != instance) throw std::runtime_error("Native attribute relay was detached");
        if (!task->connection) {
            if (!AddOwnerReference(p.relay.owner)) throw std::runtime_error("Native attribute relay expired");
            task->owners.push_back(p.relay.owner);
        }
    }
    // Renewal revalidates the existing identities on the model queue. It never
    // installs another callback or adds another owner reference.
    if (task->connection) return;
    auto signal = reinterpret_cast<void**>(p.base.dataModel + p.signalOffset);
    reinterpret_cast<void(__fastcall*)(void**)>(p.ensure)(signal);
    auto node = static_cast<unsigned char*>(reinterpret_cast<void*(__fastcall*)(std::size_t)>(p.allocate)(0x48));
    if (!node) throw std::bad_alloc();
    memset(node, 0, 0x48);
    *reinterpret_cast<LONG*>(node + 4) = 1;
    *reinterpret_cast<void**>(node + 8) = reinterpret_cast<void*>(&Changed);
    *reinterpret_cast<void**>(node + 0x20) = *signal;
    *reinterpret_cast<Task**>(node + 0x28) = task.get();
    InterlockedIncrement(reinterpret_cast<LONG*>(static_cast<unsigned char*>(*signal) + 4));
    AppendSignal(p.appendAbi, p.append, *signal, node, &task->connection);
    task->installed = task;
    if (p.relay.instance) {
        task->relayArmed = true;
        Fire(task.get(), &p.relay);
    }
}

static bool Dispatch(const std::shared_ptr<Task>& task, bool begin) {
    const auto raw = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (!raw) return false;
    auto completed = std::shared_ptr<void>(raw, [](void* event) { CloseHandle(event); });
    std::function<void()> work{[task, begin, completed]() {
        try {
            if (begin) Begin(task);
            else Finish(task.get());
        } catch (const std::exception& error) {
            task->error = error.what();
            Finish(task.get());
        }
        SetEvent(completed.get());
    }};
    const auto& p = task->params.base;
    if (!SubmitDataModelTask(p.submitTask)(reinterpret_cast<void*>(p.taskContext), &work, 1))
        return false;
    if (WaitForSingleObject(completed.get(), 2000) == WAIT_OBJECT_0) return true;
    task->cancelled.store(true);
    return false;
}

static DWORD Run(Params* input) {
    auto task = std::make_shared<Task>();
    task->params = *input;
    auto p = *input;
    if (p.appendAbi > 1 || !p.stop || !p.ready || !p.host || !p.renew || !p.base.count || p.base.count > 256
        || !p.base.timeoutMs || p.base.timeoutMs > 1800000
        || !p.base.submitTask || !p.base.taskContext || !p.base.dataModelOwner)
        throw std::runtime_error("Invalid attribute observation context");
    auto owner = reinterpret_cast<void*>(p.base.dataModelOwner);
    task->owners.reserve(p.base.count + 1);
    if (!AddOwnerReference(owner)) throw std::runtime_error("Attribute observation DataModel expired");
    // Delayed queue work must keep the model alive until after Finish returns.
    task->modelHold = std::shared_ptr<void>(owner, ReleaseOwnerReference);
    p.base.status = 1;
    if (!Dispatch(task, true)) {
        p.base.status = 0xE621;
        SetError(&p.base, "Studio did not arm attribute observation before its deadline");
    } else if (!task->error.empty()) {
        p.base.status = 0xE620;
        SetError(&p.base, task->error.c_str());
    }
    if (p.base.status == 1) {
        p.base.status = 2;
        if (PublishCaptureResponse(&p.base, 0) != 0) {
            p.base.status = 0xE622;
            SetError(&p.base, "Attribute observation transport closed");
        }
    }
    SetEvent(p.ready);
    if (p.base.status == 2) {
        // A forcibly stopped daemon cannot signal its cancellation event. The
        // process handle also retires its native callback and DataModel owners.
        HANDLE cancellation[] = { p.stop, p.host, p.renew };
        const auto deadline = GetTickCount64() + p.base.timeoutMs;
        DWORD result = WAIT_TIMEOUT;
        for (auto now = GetTickCount64(); now < deadline; now = GetTickCount64()) {
            const auto remaining = deadline - now;
            result = WaitForMultipleObjects(3, cancellation, FALSE,
                static_cast<DWORD>((std::min)(remaining, 120000ull)));
            if (result != WAIT_OBJECT_0 + 2) break;
            if (!Dispatch(task, true) || !task->error.empty()) { result = WAIT_FAILED; break; }
            result = WAIT_TIMEOUT;
        }
        if (result != WAIT_OBJECT_0) {
            p.base.status = 0xE623;
            SetError(&p.base, "Attribute observation expired or its host stopped");
        }
    }
    const bool finished = Dispatch(task, false);
    if (!finished) {
        // Queued cleanup owns task until it runs; no callback can reference a
        // freed state. The host rejects the snapshot on this path.
        p.base.status = 0xE624;
        SetError(&p.base, "Studio did not finish attribute observation before its deadline");
    } else if (!task->error.empty()) {
        p.base.status = 0xE620;
        SetError(&p.base, task->error.c_str());
    }
    if (p.base.status == 2) {
        p.base.status = task->changed ? 0xE625 : 4;
        if (task->changed) SetError(&p.base, "Studio attributes changed during export; retry the sync");
    }
    return PublishCaptureResponse(&p.base, p.base.status == 4 ? 0 : p.base.status);
}
}

static DWORD ObservationBoundary(renium_observation::Params* params) {
    try { return renium_observation::Run(params); }
    catch (const std::exception& error) {
        params->base.status = 0xE626;
        SetError(&params->base, error.what());
        return PublishCaptureResponse(&params->base, params->base.status);
    }
    catch (...) {
        params->base.status = 0xE626;
        SetError(&params->base, "Unknown native observation submission failure");
        return PublishCaptureResponse(&params->base, params->base.status);
    }
}
// Separate entry point: a legacy request has no renewal-handle suffix.
extern "C" __declspec(dllexport) DWORD WINAPI ReniumObserveAttributesLease(renium_observation::Params* params) {
    if (!params) return 0xE000;
    if (params->base.protocolVersion != ReniumSerializerVersion) {
        // The old request's extension starts earlier: never read its handles
        // as v2 handles or copy the larger struct before validating the prefix.
        params->base.status = 0xE00A;
        SetError(&params->base, "Incompatible native serializer protocol; use the matching Renium helper");
        VirtualFree(params, 0, MEM_RELEASE);
        return 0xE00A;
    }
    DWORD result = 0xE626;
    __try { result = ObservationBoundary(params); }
    __finally {
        if (params->stop) CloseHandle(params->stop);
        if (params->ready) { SetEvent(params->ready); CloseHandle(params->ready); }
        if (params->host) CloseHandle(params->host);
        if (params->renew) CloseHandle(params->renew);
        VirtualFree(params, 0, MEM_RELEASE);
    }
    return result;
}
