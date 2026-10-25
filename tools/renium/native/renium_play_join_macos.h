#pragma once
#include "renium_teleport_dispatch_macos.h"

namespace renium_mac_join {
using namespace renium_mac_teleport;
struct Request {
    std::uint32_t version, size, action, receipt;
    std::uint64_t token, dispatch, attempt, place, universe;
    std::uint64_t joinSlot, joinOriginal, joinField, localSlot, localOriginal, localField;
    char nonce[128];
};
static_assert(sizeof(Request) == 232);
using Join = int(*)(void*, void*);
using Local = Variant(*)(void*);
struct Arrival {
    Request request{};
    std::shared_ptr<renium_mac_teleport::Lease> launch;
    std::shared_ptr<JoinRecord> record;
    Clock::time_point deadline;
    bool active = true;
    std::atomic<std::uint32_t> receipt{0};
    bool Alive() const {
        std::lock_guard launchLock(renium_mac_teleport::mutex);
        return active && Clock::now() < deadline && launch->Alive();
    }
};
inline std::mutex mutex;
inline std::condition_variable changed;
inline std::shared_ptr<Arrival> current;
inline std::atomic<Join> originalJoin{nullptr};
inline std::atomic<Local> originalLocal{nullptr};

inline std::shared_ptr<Arrival> Active() {
    std::lock_guard lock(mutex);
    return current && current->Alive() ? current : nullptr;
}

inline bool Matches(const Arrival& arrival, void* instance, int client) {
    {
        std::lock_guard lock(renium_mac_teleport::mutex);
        if (renium_mac_teleport::current != arrival.launch
            || arrival.launch->request.place != arrival.request.place
            || std::strcmp(arrival.launch->request.nonce, arrival.request.nonce)) return false;
    }
    return Owns(arrival.launch, instance, client) != nullptr;
}

inline int JoinEntry(void* player, void* lua) {
    const auto arrival = Active();
    if (arrival && Matches(*arrival, player, -1)) {
        auto& field = *reinterpret_cast<std::shared_ptr<const void>*>(
            static_cast<char*>(player) + arrival->request.joinField);
        field = *reinterpret_cast<const std::shared_ptr<const void>*>(arrival->record->join.payload);
        arrival->receipt.fetch_or(1);
    }
    return originalJoin.load(std::memory_order_acquire)(player, lua);
}

inline Variant LocalEntry(void* service) {
    const auto arrival = Active();
    if (arrival && Matches(*arrival, service, 1)) {
        auto field = static_cast<char*>(service) + arrival->request.localField;
        if (!Word(field, 8)) {
            const auto& value = arrival->record->data;
            if (value.operations)
                reinterpret_cast<void(**)(const void*, void*)>(value.operations)[0](value.payload, field + 16);
            std::memcpy(field, &value.type, 8);
            std::memcpy(field + 8, &value.operations, 8);
        }
        arrival->receipt.fetch_or(2);
    }
    return originalLocal.load(std::memory_order_acquire)(service);
}

inline void Restore(const std::shared_ptr<Arrival>& arrival) {
    if (current != arrival) return;
    arrival->active = false;
    const auto& r = arrival->request;
    if (Word(reinterpret_cast<void*>(r.joinSlot), 0) == reinterpret_cast<std::uintptr_t>(&JoinEntry))
        Exchange(r.joinSlot, reinterpret_cast<void*>(&JoinEntry), reinterpret_cast<void*>(r.joinOriginal));
    if (Word(reinterpret_cast<void*>(r.localSlot), 0) == reinterpret_cast<std::uintptr_t>(&LocalEntry))
        Exchange(r.localSlot, reinterpret_cast<void*>(&LocalEntry), reinterpret_cast<void*>(r.localOriginal));
    changed.notify_all();
}

inline void Handle(const std::string& payload, std::vector<unsigned char>& output) {
    Request r{};
    if (payload.size() != sizeof(r)) throw std::runtime_error("Truncated Mac arrival request");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 1 || r.size != sizeof(r) || !r.token || r.action < 1 || r.action > 3)
        throw std::runtime_error("Incompatible Mac arrival request");
    std::lock_guard lock(mutex);
    if (r.action == 1) {
        if (!r.dispatch || !r.attempt || !r.place || !r.universe || !r.joinSlot || !r.joinOriginal
            || !r.localSlot || !r.localOriginal || r.joinField >= 65536 || r.localField >= 65536
            || r.joinField % 8 || r.localField % 8 || !r.nonce[0] || r.nonce[127])
            throw std::runtime_error("Invalid Mac arrival binding");
        if (current && current->Alive()) throw std::runtime_error("The Mac arrival already has an owner");
        if (current) Restore(current);
        auto arrival = std::make_shared<Arrival>();
        arrival->request = r;
        arrival->deadline = Clock::now() + std::chrono::seconds(90);
        {
            std::lock_guard launchLock(renium_mac_teleport::mutex);
            const auto launch = renium_mac_teleport::current;
            if (!launch || !launch->Alive() || launch->request.token != r.dispatch || !launch->root
                || std::strcmp(launch->request.nonce, r.nonce) || launch->request.place != r.place
                || launch->request.universe != r.universe || !launch->arrival
                || launch->arrival->id != r.attempt || launch->arrival->place != static_cast<std::int64_t>(r.place)
                || launch->arrival->join.type != launch->request.tableType)
                throw std::runtime_error("Mac arrival does not belong to the accepted teleport launch");
            arrival->launch = launch;
            arrival->record = launch->arrival;
        }
        originalJoin.store(reinterpret_cast<Join>(r.joinOriginal), std::memory_order_release);
        originalLocal.store(reinterpret_cast<Local>(r.localOriginal), std::memory_order_release);
        current = arrival;
        try {
            if (!Exchange(r.joinSlot, reinterpret_cast<void*>(r.joinOriginal), reinterpret_cast<void*>(&JoinEntry))
                || !Exchange(r.localSlot, reinterpret_cast<void*>(r.localOriginal), reinterpret_cast<void*>(&LocalEntry)))
                throw std::runtime_error("Another owner changed the Mac arrival bindings");
            std::thread([arrival] {
                std::unique_lock lock(mutex);
                while (current == arrival && arrival->active) {
                    if (!arrival->Alive()) { Restore(arrival); break; }
                    changed.wait_for(lock, std::chrono::milliseconds(100));
                }
            }).detach();
        } catch (...) { Restore(arrival); throw; }
    } else {
        if (!current || current->request.token != r.token)
            throw std::runtime_error("The Mac arrival lease was replaced");
        if (r.action == 2) Restore(current);
        else r.receipt = current->receipt.load();
    }
    output.resize(sizeof(r));
    std::memcpy(output.data(), &r, sizeof(r));
}
}
