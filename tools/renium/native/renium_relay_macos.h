#pragma once
#include "renium_teleport_dispatch_macos.h"

// The plugin invokes its own relay on the correct DataModel scheduler. Native
// consumers receive engine-owned Variants without guessing a task-queue ABI.
namespace renium_mac_relay {
using namespace renium_mac_teleport;
struct Binding {
    std::uint64_t model, modelOwner, relay, relayOwner;
    std::uint64_t parent, children, klass, self, attributeSlot, attribute, invoke;
    std::uint32_t hostPid, reserved;
    std::uint64_t hostSeconds, hostMicros;
};
static_assert(sizeof(Binding) == 112);
inline bool Host(const Binding& r) {
    LoadRequest host{};
    host.hostPid = r.hostPid; host.hostSeconds = r.hostSeconds; host.hostMicros = r.hostMicros;
    return renium_mac_playtest::HostAlive(host);
}
inline void* Model(const Binding& r, void* instance) {
    for (unsigned depth = 0; instance && depth < 5; ++depth) {
        if (Class(instance, r.klass, "DataModel")) return instance;
        instance = reinterpret_cast<void*>(Word(instance, r.parent));
    }
    return nullptr;
}
struct Task {
    Binding binding{};
    std::shared_ptr<renium_history::WeakOwner> model, relay;
    std::function<void(Values)> consume;
    std::atomic<unsigned> receipt{0};
    std::string error;
    Clock::time_point deadline = Clock::now() + std::chrono::seconds(10);
};
inline std::mutex mutex;
inline std::shared_ptr<Task> current;
using Getter = Variant(*)(void*, StringView);
inline std::atomic<Getter> original{nullptr};
inline Variant Entry(void*, StringView);
inline void Restore(const std::shared_ptr<Task>& task) {
    if (current != task) return;
    const auto& r = task->binding;
    Exchange(r.attributeSlot, reinterpret_cast<void*>(&Entry), reinterpret_cast<void*>(r.attribute));
    current.reset();
}
inline Variant Entry(void* receiver, StringView key) {
    std::shared_ptr<Task> task;
    {
        std::lock_guard lock(mutex);
        if (current && receiver == reinterpret_cast<void*>(current->binding.relay)
            && std::string_view(key.data, key.size) == "__ReniumRouteInvoke") {
            task = current; task->receipt = 1; Restore(task);
        }
    }
    if (task) {
        try {
            const auto& r = task->binding;
            if (!Host(r) || !task->model->Alive() || !task->relay->Alive() || Clock::now() >= task->deadline
                || Model(r, receiver) != reinterpret_cast<void*>(r.model) || !Class(receiver, r.klass, "BindableFunction"))
                throw std::runtime_error("Native route relay ended");
            reinterpret_cast<Invoke>(r.invoke)(receiver, std::make_shared<Tuple>(),
                [task](Values values) {
                    try { task->consume(values); }
                    catch (const std::exception& error) { task->error = error.what(); }
                    task->receipt.store(2, std::memory_order_release);
                }, [task](std::string error) {
                    task->error = std::move(error); task->receipt.store(2, std::memory_order_release);
                });
        } catch (const std::exception& error) {
            task->error = error.what(); task->receipt.store(2, std::memory_order_release);
        }
    }
    return original.load()(receiver, key);
}
inline std::shared_ptr<Task> Arm(const Binding& r, std::function<void(Values)> consume) {
    if (!r.model || !r.modelOwner || !r.relay || !r.relayOwner || !r.attributeSlot || !r.attribute || !r.invoke
        || r.reserved || !Host(r) || r.parent >= 65536 || r.children >= 65536 || r.klass >= 65536 || r.self >= 65536
        || Word(reinterpret_cast<void*>(r.model), r.self + 8) != r.modelOwner
        || Word(reinterpret_cast<void*>(r.relay), r.self + 8) != r.relayOwner)
        throw std::runtime_error("Invalid native route relay binding");
    auto task = std::make_shared<Task>();
    task->binding = r; task->consume = std::move(consume);
    task->model = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(r.modelOwner));
    task->relay = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(r.relayOwner));
    std::lock_guard lock(mutex);
    if (current) throw std::runtime_error("A native route relay is pending");
    original.store(reinterpret_cast<Getter>(r.attribute));
    if (!Exchange(r.attributeSlot, reinterpret_cast<void*>(r.attribute), reinterpret_cast<void*>(&Entry)))
        throw std::runtime_error("Native route relay getter changed");
    current = task;
    std::thread([task] {
        for (;;) {
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
            std::lock_guard lock(mutex);
            if (current != task) return;
            if (Clock::now() >= task->deadline || !Host(task->binding) || !task->relay->Alive()) {
                Restore(task); return;
            }
        }
    }).detach();
    return task;
}
inline bool Complete(const std::shared_ptr<Task>& task) {
    if (task->receipt.load(std::memory_order_acquire) != 2) return false;
    if (!task->error.empty()) throw std::runtime_error(task->error);
    return true;
}
inline void Cancel(const std::shared_ptr<Task>& task) {
    std::lock_guard lock(mutex); Restore(task);
}
}
