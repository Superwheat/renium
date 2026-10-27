#pragma once
#include "renium_play_teleport_windows.h"

namespace renium_teleport_gui {
using namespace renium_play_teleport;
struct Request {
    std::uint32_t version, size;
    renium_play_owner::Binding model;
    std::uint64_t relay, relayOwner, invoke, instanceType, add, remove, finishedSlot, finishedOriginal, removedSlot, removedOriginal;
    std::uint32_t show, hostPid;
    std::uint64_t hostCreation;
    char error[256];
};
static_assert(sizeof(Request) == 488);
using Ready = bool(*)(void*);
struct Lease {
    Request request{};
    std::weak_ptr<void> receiver;
    HANDLE host = nullptr;
    std::uint64_t deadline = 0;
    ~Lease() { if (host) CloseHandle(host); }
};
inline std::mutex mutex;
inline std::shared_ptr<Lease> current;
inline std::atomic<Ready> finishedOriginal{nullptr}, removedOriginal{nullptr};
inline bool Held(void* receiver) {
    std::lock_guard lock(mutex);
    return current && current->receiver.lock().get() == receiver
        && GetTickCount64() < current->deadline && WaitForSingleObject(current->host, 0) == WAIT_TIMEOUT;
}
inline bool Finished(void* receiver) { return Held(receiver) ? false : finishedOriginal.load()(receiver); }
inline bool Removed(void* receiver) { return Held(receiver) ? false : removedOriginal.load()(receiver); }
inline void Restore(const std::shared_ptr<Lease>& lease) {
    if (current != lease) return;
    const auto& r = lease->request;
    renium_play_loader::Exchange(r.finishedSlot, reinterpret_cast<void*>(&Finished), reinterpret_cast<void*>(r.finishedOriginal));
    renium_play_loader::Exchange(r.removedSlot, reinterpret_cast<void*>(&Removed), reinterpret_cast<void*>(r.removedOriginal));
    current.reset();
}
inline void Hold(const Request& r, const Instance& receiver) {
    std::lock_guard lock(mutex);
    if (current) throw std::runtime_error("Default loading screen is already active");
    auto lease = std::make_shared<Lease>();
    lease->request = r; lease->receiver = receiver; lease->deadline = GetTickCount64() + 120000;
    lease->host = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, r.hostPid);
    FILETIME created{}, exited{}, kernel{}, user{};
    if (!lease->host || !GetProcessTimes(lease->host, &created, &exited, &kernel, &user)
        || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != r.hostCreation)
        throw std::runtime_error("Teleport GUI host changed");
    finishedOriginal.store(reinterpret_cast<Ready>(r.finishedOriginal));
    removedOriginal.store(reinterpret_cast<Ready>(r.removedOriginal));
    if (!renium_play_loader::Exchange(r.finishedSlot, reinterpret_cast<void*>(r.finishedOriginal), reinterpret_cast<void*>(&Finished)))
        throw std::runtime_error("Loading replication binding changed");
    try {
        if (!renium_play_loader::Exchange(r.removedSlot, reinterpret_cast<void*>(r.removedOriginal), reinterpret_cast<void*>(&Removed)))
            throw std::runtime_error("Loading removal binding changed");
    } catch (...) {
        renium_play_loader::Exchange(r.finishedSlot, reinterpret_cast<void*>(&Finished), reinterpret_cast<void*>(r.finishedOriginal));
        throw;
    }
    current = lease;
    std::thread([lease] {
        for (;;) {
            const auto wait = WaitForSingleObject(lease->host, 100);
            std::lock_guard lock(mutex);
            if (current != lease) return;
            if (wait != WAIT_TIMEOUT || lease->receiver.expired() || GetTickCount64() >= lease->deadline) {
                Restore(lease); return;
            }
        }
    }).detach();
}
struct Task {
    Request request{};
    std::shared_ptr<void> modelHold, relayHold;
    HANDLE finished = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    std::atomic<bool> active{true};
    std::string error;
    ~Task() { if (finished) CloseHandle(finished); }
    void Finish(const std::string& message = {}) { error = message; SetEvent(finished); }
};
inline Instance InstanceValue(const std::shared_ptr<Task>& task, const Variant& value, const char* expected) {
    if (value.type != task->request.instanceType) throw std::runtime_error("Loading screen argument changed type");
    const auto& instance = *reinterpret_cast<const Instance*>(value.payload);
    const auto& binding = task->request.model;
    if (!instance || !Class(instance.get(), binding.classOffset, expected)
        || renium_play_owner::Model(instance.get(), binding) != reinterpret_cast<void*>(binding.sourceModel))
        throw std::runtime_error("Loading screen argument changed ownership");
    return instance;
}
inline void Handle(Request& request) {
    if (request.version != 6 || request.size != sizeof(request) || !request.instanceType
        || !request.relay || !request.relayOwner || !request.add || !request.remove || !request.invoke
        || (request.show && (!request.finishedSlot || !request.finishedOriginal || !request.removedSlot || !request.removedOriginal)))
        throw std::runtime_error("Incompatible teleport GUI binding");
    auto task = std::make_shared<Task>();
    task->request = request;
    if (!task->finished) throw std::runtime_error("Cannot create teleport GUI receipt");
    auto model = reinterpret_cast<void*>(request.model.sourceModel);
    task->modelHold = renium_play_owner::Hold(model, request.model);
    auto relay = reinterpret_cast<void*>(request.relay);
    if (renium_play_owner::Model(relay, request.model) != model || !Class(relay, request.model.classOffset, "BindableFunction"))
        throw std::runtime_error("Teleport GUI relay belongs to another model");
    if (!AddOwnerReference(reinterpret_cast<void*>(request.relayOwner))) throw std::runtime_error("Teleport GUI relay ended");
    task->relayHold = std::shared_ptr<void>(reinterpret_cast<void*>(request.relayOwner), ReleaseOwnerReference);
    std::weak_ptr<Task> weak = task;
    if (!renium_play_owner::Submit(model, request.model, [weak] {
        const auto task = weak.lock();
        if (!task) return;
        if (!task->active.load()) { task->Finish(); return; }
        try {
            auto arguments = std::make_shared<Tuple>();
            InvokeNative(task->request.invoke, reinterpret_cast<void*>(task->request.relay), arguments,
                [task](Values values) {
                    try {
                        if (!task->active.load()) { task->Finish(); return; }
                        if (!values || values->values.size() != 3)
                            throw std::runtime_error("Teleport GUI arguments changed");
                        const auto context = InstanceValue(task, values->values[0], "ScriptContext");
                        const auto core = InstanceValue(task, values->values[1], "CoreGui");
                        const auto first = InstanceValue(task, values->values[2], "ReplicatedFirst");
                        if (task->request.show) {
                            Hold(task->request, first);
                            using Add = void(*)(void*, std::string, Instance);
                            reinterpret_cast<Add>(task->request.add)(context.get(), "LoadingScript", core);
                        } else {
                            {
                                std::lock_guard lock(mutex);
                                if (current && current->receiver.lock() == first) Restore(current);
                            }
                            reinterpret_cast<void(*)(void*)>(task->request.remove)(first.get());
                        }
                        task->Finish();
                    } catch (const std::exception& error) { task->Finish(error.what()); }
                }, [task](std::string error) { task->Finish(error); });
        } catch (const std::exception& error) { task->Finish(error.what()); }
    })) throw std::runtime_error("Teleport client rejected its GUI notification");
    const auto waited = WaitForSingleObject(task->finished, 3000);
    task->active.store(false);
    if (waited != WAIT_OBJECT_0) throw std::runtime_error("Teleport GUI notification timed out");
    if (!task->error.empty()) throw std::runtime_error(task->error);
}
}
extern "C" __declspec(dllexport) DWORD WINAPI ReniumTeleportGui(void* input) {
    auto& request = *static_cast<renium_teleport_gui::Request*>(input);
    try { renium_teleport_gui::Handle(request); return 0; }
    catch (const std::exception& error) { strncpy_s(request.error, error.what(), _TRUNCATE); return 1; }
}
