#pragma once
#include "renium_teleport_dispatch_macos.h"

namespace renium_mac_gui {
using namespace renium_mac_teleport;
struct Request {
    std::uint32_t version, size, action, receipt;
    std::uint64_t token, model, modelOwner, relay, relayOwner, context, core, first;
    std::uint64_t attributeSlot, attributeOriginal, add, remove;
    std::uint64_t finishedSlot, finishedOriginal, removedSlot, removedOriginal;
    std::uint64_t parentOffset, classOffset, selfOffset;
    std::uint32_t show, hostPid;
    std::uint64_t hostSeconds, hostMicros;
};
static_assert(sizeof(Request) == 192);
using AttributeGetter = Variant(*)(void*, StringView);
using Ready = bool(*)(void*);
struct Task {
    Request request{};
    std::shared_ptr<renium_history::WeakOwner> model, relay;
    Clock::time_point deadline;
    std::string error;
    std::uint32_t receipt = 0;
};
struct Display {
    Request request{};
    std::weak_ptr<void> first;
    Clock::time_point deadline;
};
inline std::mutex mutex;
inline std::shared_ptr<Task> current;
inline std::shared_ptr<Display> display;
inline std::atomic<AttributeGetter> original{nullptr};
inline std::atomic<Ready> finishedOriginal{nullptr}, removedOriginal{nullptr};
inline Variant AttributeEntry(void*, StringView);

inline bool Host(const Request& r) {
    LoadRequest host{};
    host.hostPid = r.hostPid; host.hostSeconds = r.hostSeconds; host.hostMicros = r.hostMicros;
    return renium_mac_playtest::HostAlive(host);
}
inline bool Held(void* receiver) {
    std::lock_guard lock(mutex);
    return display && display->first.lock().get() == receiver && Clock::now() < display->deadline && Host(display->request);
}
inline bool Finished(void* receiver) { return Held(receiver) ? false : finishedOriginal.load()(receiver); }
inline bool Removed(void* receiver) { return Held(receiver) ? false : removedOriginal.load()(receiver); }
inline void RestoreDisplay() {
    if (!display) return;
    const auto r = display->request;
    Exchange(r.finishedSlot, reinterpret_cast<void*>(&Finished), reinterpret_cast<void*>(r.finishedOriginal));
    Exchange(r.removedSlot, reinterpret_cast<void*>(&Removed), reinterpret_cast<void*>(r.removedOriginal));
    display.reset();
}
inline void RestoreTask(const std::shared_ptr<Task>& task) {
    const auto& r = task->request;
    Exchange(r.attributeSlot, reinterpret_cast<void*>(&AttributeEntry), reinterpret_cast<void*>(r.attributeOriginal));
}
inline Instance Owned(const Request& r, std::uint64_t address, const char* kind) {
    auto object = reinterpret_cast<void*>(address);
    auto held = reinterpret_cast<const std::weak_ptr<void>*>(static_cast<char*>(object) + r.selfOffset)->lock();
    if (held.get() != object || !Class(object, r.classOffset, kind))
        throw std::runtime_error("Loading screen object ended or changed type");
    auto root = object;
    for (unsigned depth = 0; root && depth < 4; ++depth) {
        if (root == reinterpret_cast<void*>(r.model)) return held;
        root = reinterpret_cast<void*>(Word(root, r.parentOffset));
    }
    throw std::runtime_error("Loading screen object belongs to another model");
}
inline void Present(const Request& r) {
    const auto context = Owned(r, r.context, "ScriptContext");
    const auto core = Owned(r, r.core, "CoreGui");
    const auto first = Owned(r, r.first, "ReplicatedFirst");
    if (r.show) {
        auto next = std::make_shared<Display>();
        next->request = r; next->first = first; next->deadline = Clock::now() + std::chrono::seconds(120);
        {
            std::lock_guard lock(mutex);
            if (display) throw std::runtime_error("Default loading screen is already active");
            finishedOriginal.store(reinterpret_cast<Ready>(r.finishedOriginal));
            removedOriginal.store(reinterpret_cast<Ready>(r.removedOriginal));
            if (!Exchange(r.finishedSlot, reinterpret_cast<void*>(r.finishedOriginal), reinterpret_cast<void*>(&Finished)))
                throw std::runtime_error("Loading replication binding changed");
            if (!Exchange(r.removedSlot, reinterpret_cast<void*>(r.removedOriginal), reinterpret_cast<void*>(&Removed))) {
                Exchange(r.finishedSlot, reinterpret_cast<void*>(&Finished), reinterpret_cast<void*>(r.finishedOriginal));
                throw std::runtime_error("Loading removal binding changed");
            }
            display = next;
        }
        try {
            std::thread([next] {
                for (;;) {
                    std::this_thread::sleep_for(std::chrono::milliseconds(100));
                    std::lock_guard lock(mutex);
                    if (display != next) return;
                    if (next->first.expired() || Clock::now() >= next->deadline || !Host(next->request)) {
                        RestoreDisplay(); return;
                    }
                }
            }).detach();
            reinterpret_cast<void(*)(void*, std::string, Instance)>(r.add)(context.get(), "LoadingScript", core);
        } catch (...) { std::lock_guard lock(mutex); RestoreDisplay(); throw; }
    } else {
        {
            std::lock_guard lock(mutex);
            if (display && display->first.lock() == first) RestoreDisplay();
        }
        reinterpret_cast<void(*)(void*)>(r.remove)(first.get());
    }
}
inline Variant AttributeEntry(void* receiver, StringView key) {
    std::shared_ptr<Task> task;
    {
        std::lock_guard lock(mutex);
        if (current && !current->receipt && receiver == reinterpret_cast<void*>(current->request.relay)
            && std::string_view(key.data, key.size) == "__ReniumNativeGuiInvoke") {
            task = current;
            task->receipt = 1;
            RestoreTask(task);
        }
    }
    if (task) {
        try {
            if (!task->model->Alive() || !task->relay->Alive() || !Host(task->request) || Clock::now() >= task->deadline)
                throw std::runtime_error("Teleport GUI notification expired");
            Owned(task->request, task->request.relay, "BindableFunction");
            // The selected plugin invokes this getter on its actual model scheduler.
            // No external thread or guessed DataModel task context calls the engine.
            Present(task->request);
        } catch (const std::exception& error) { task->error = error.what(); }
        std::lock_guard lock(mutex);
        task->receipt = 2;
    }
    return original.load(std::memory_order_acquire)(receiver, key);
}
inline void Handle(const std::string& payload, std::vector<unsigned char>& output) {
    Request r{};
    if (payload.size() != sizeof(r)) throw std::runtime_error("Truncated Mac teleport GUI request");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 1 || r.size != sizeof(r) || !r.token || r.action < 1 || r.action > 3)
        throw std::runtime_error("Incompatible Mac teleport GUI request");
    std::lock_guard lock(mutex);
    if (r.action == 1) {
        if (!r.model || !r.modelOwner || !r.relay || !r.relayOwner || !r.context || !r.core || !r.first
            || !r.attributeSlot || !r.attributeOriginal || !r.add || !r.remove || !Host(r)
            || r.parentOffset >= 65536 || r.classOffset >= 65536 || r.selfOffset >= 65536
            || r.parentOffset % 8 || r.classOffset % 8 || r.selfOffset % 8 || r.show > 1
            || (r.show && (!r.finishedSlot || !r.finishedOriginal || !r.removedSlot || !r.removedOriginal))
            || Word(reinterpret_cast<void*>(r.model), r.selfOffset + 8) != r.modelOwner
            || Word(reinterpret_cast<void*>(r.relay), r.selfOffset + 8) != r.relayOwner)
            throw std::runtime_error("Invalid Mac teleport GUI ownership");
        if (current) throw std::runtime_error("A teleport GUI notification is pending");
        auto task = std::make_shared<Task>();
        task->request = r;
        task->model = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(r.modelOwner));
        task->relay = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(r.relayOwner));
        task->deadline = Clock::now() + std::chrono::seconds(5);
        original.store(reinterpret_cast<AttributeGetter>(r.attributeOriginal), std::memory_order_release);
        if (!Exchange(r.attributeSlot, reinterpret_cast<void*>(r.attributeOriginal), reinterpret_cast<void*>(&AttributeEntry)))
            throw std::runtime_error("Teleport GUI getter changed");
        current = task;
        try { std::thread([task] {
            for (;;) {
                std::this_thread::sleep_for(std::chrono::milliseconds(100));
                std::lock_guard lock(mutex);
                if (current != task) return;
                if (Clock::now() >= task->deadline || !Host(task->request) || !task->model->Alive() || !task->relay->Alive()) {
                    RestoreTask(task); current.reset(); return;
                }
            }
        }).detach(); } catch (...) { RestoreTask(task); current.reset(); throw; }
    } else {
        if (!current || current->request.token != r.token) throw std::runtime_error("Teleport GUI notification ended");
        r.receipt = current->receipt;
        if (r.action == 2) { RestoreTask(current); current.reset(); }
        else if (r.receipt == 2 && !current->error.empty()) throw std::runtime_error(current->error);
    }
    output.resize(sizeof(r));
    std::memcpy(output.data(), &r, sizeof(r));
}
}
