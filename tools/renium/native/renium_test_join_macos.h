#pragma once
#include "renium_relay_macos.h"
#include "renium_route_code_macos.h"

namespace renium_mac_test_join {
using namespace renium_mac_relay;
struct Request {
    std::uint32_t version, size, action, receipt;
    std::uint64_t token;
    Binding relay;
    std::uint64_t place, universe, placeField, universeField;
    std::uint64_t joinSlot, join, joinField, localSlot, local, localField, tableType;
    std::uint64_t calls[3], writers[3];
    std::int64_t userId;
    std::uint32_t client, reserved;
    char name[64], displayName[128];
    unsigned char prefixes[2][16];
};
static_assert(sizeof(Request) == 512);
struct Arrival {
    Request request{};
    Values values;
    std::vector<Instance> existing;
    void* player = nullptr;
    void* local = nullptr;
    std::shared_ptr<Task> task;
    Clock::time_point deadline = Clock::now() + std::chrono::seconds(60);
    unsigned installed = 0;
    bool Alive() const { return Host(request.relay) && Clock::now() < deadline; }
};
using Join = int(*)(void*, void*);
using Local = Variant(*)(void*);
using Number = void(*)(void*, StringView, std::int64_t);
using Text = void(*)(void*, StringView, StringView);
inline std::mutex mutex;
inline std::shared_ptr<Arrival> current;
inline std::atomic<Join> originalJoin{nullptr};
inline std::atomic<Local> originalLocal{nullptr};
inline std::atomic<Number> originalNumber{nullptr};
inline std::atomic<Text> originalText{nullptr};
// Forwarders survive restoration: a thread may already be crossing one.
inline void* forwarders[2]{};
inline std::uint64_t entries[2]{};
inline bool Matches(const Arrival& arrival, void* instance) {
    const auto& r = arrival.request;
    const auto model = Model(r.relay, instance);
    return model && (r.client ? model != reinterpret_cast<void*>(r.relay.model) : model == reinterpret_cast<void*>(r.relay.model))
        && Word(model, r.placeField) == r.place && Word(model, r.universeField) == r.universe;
}
__attribute__((noinline)) inline void IdentityNumber(void* writer, StringView key, std::int64_t value) {
    const auto caller = reinterpret_cast<std::uintptr_t>(__builtin_extract_return_addr(__builtin_return_address(0)));
    {
        std::lock_guard lock(mutex);
        if (current && current->request.client && caller == current->request.calls[0] + 4
            && current->Alive() && std::string_view(key.data, key.size) == "UserId")
            value = current->request.userId;
    }
    originalNumber.load()(writer, key, value);
}
__attribute__((noinline)) inline void IdentityText(void* writer, StringView key, StringView value) {
    const auto caller = reinterpret_cast<std::uintptr_t>(__builtin_extract_return_addr(__builtin_return_address(0)));
    std::shared_ptr<Arrival> arrival;
    { std::lock_guard lock(mutex); arrival = current; }
    if (arrival && arrival->request.client
        && (caller == arrival->request.calls[1] + 4 || caller == arrival->request.calls[2] + 4) && arrival->Alive()) {
        const auto name = std::string_view(key.data, key.size);
        if (caller == arrival->request.calls[1] + 4 && name == "UserName") value = {arrival->request.name, std::strlen(arrival->request.name)};
        else if (caller == arrival->request.calls[2] + 4 && name == "DisplayName") value = {arrival->request.displayName, std::strlen(arrival->request.displayName)};
    }
    originalText.load()(writer, key, value);
}
inline int GetJoin(void* player, void* lua) {
    {
        std::lock_guard lock(mutex);
        const auto arrival = current;
        if (arrival && arrival->values && arrival->Alive() && Matches(*arrival, player)
            && (!arrival->player || arrival->player == player)
            && std::none_of(arrival->existing.begin(), arrival->existing.end(), [player](const auto& item) { return item.get() == player; })) {
            arrival->player = player;
            auto& field = *reinterpret_cast<std::shared_ptr<const void>*>(static_cast<char*>(player) + arrival->request.joinField);
            field = *reinterpret_cast<const std::shared_ptr<const void>*>(arrival->values->values[0].payload);
        }
    }
    return originalJoin.load()(player, lua);
}
inline Variant GetLocal(void* service) {
    {
        std::lock_guard lock(mutex);
        const auto arrival = current;
        if (arrival && arrival->request.client && arrival->values && arrival->Alive() && Matches(*arrival, service) && arrival->local != service) {
            auto field = static_cast<char*>(service) + arrival->request.localField;
            const auto operations = Word(field, 8);
            if (operations) reinterpret_cast<void(**)(void*)>(operations)[2](field + 16);
            const auto& value = arrival->values->values[1];
            if (value.operations) reinterpret_cast<void(**)(const void*, void*)>(value.operations)[0](value.payload, field + 16);
            std::memcpy(field, &value.type, 8); std::memcpy(field + 8, &value.operations, 8);
            arrival->local = service;
        }
    }
    return originalLocal.load()(service);
}
inline void Restore(const std::shared_ptr<Arrival>& arrival) {
    if (current != arrival) return;
    const auto& r = arrival->request;
    Exchange(r.joinSlot, reinterpret_cast<void*>(&GetJoin), reinterpret_cast<void*>(r.join));
    if (r.client) Exchange(r.localSlot, reinterpret_cast<void*>(&GetLocal), reinterpret_cast<void*>(r.local));
    while (arrival->installed) {
        const auto i = --arrival->installed;
        const auto target = i ? reinterpret_cast<void*>(&IdentityText) : reinterpret_cast<void*>(&IdentityNumber);
        renium_mac_route_code::Replace(r.writers[i], renium_history::Jump(reinterpret_cast<std::uintptr_t>(target)),
            std::vector<unsigned char>(r.prefixes[i], r.prefixes[i] + 16));
    }
    if (arrival->task) Cancel(arrival->task);
    current.reset();
}
inline void Handle(const std::string& payload, std::vector<unsigned char>& output) {
    Request r{};
    if (payload.size() != sizeof(r)) throw std::runtime_error("Truncated routed arrival");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 2 || r.size != sizeof(r) || !r.token || r.action < 1 || r.action > 3 || r.client > 1 || r.reserved)
        throw std::runtime_error("Incompatible routed arrival");
    std::lock_guard lock(mutex);
    if (r.action == 1) {
        if (current || !r.place || !r.universe || r.placeField >= 65536 || r.universeField >= 65536
            || !r.joinSlot || !r.join || !r.localSlot || !r.local || r.joinField >= 65536 || r.localField >= 65536
            || !r.tableType || (r.client && (r.userId >= 0 || !r.name[0] || r.name[63] || !r.displayName[0] || r.displayName[127]
                || !r.calls[0] || !r.calls[1] || !r.calls[2] || !r.writers[0] || !r.writers[1] || r.writers[1] != r.writers[2])))
            throw std::runtime_error("Invalid routed arrival ownership");
        auto arrival = std::make_shared<Arrival>(); arrival->request = r;
        originalJoin.store(reinterpret_cast<Join>(r.join)); originalLocal.store(reinterpret_cast<Local>(r.local));
        current = arrival;
        try {
            if (r.client) {
                for (unsigned i = 0; i < 2; ++i) {
                    if (forwarders[i] && entries[i] != r.writers[i]) throw std::runtime_error("Local player identity writer moved");
                    if (!forwarders[i]) {
                        forwarders[i] = renium_mac_route_code::Forward(r.writers[i], r.prefixes[i]);
                        entries[i] = r.writers[i];
                    }
                    if (i) originalText.store(reinterpret_cast<Text>(forwarders[i]));
                    else originalNumber.store(reinterpret_cast<Number>(forwarders[i]));
                    const auto target = i ? reinterpret_cast<void*>(&IdentityText) : reinterpret_cast<void*>(&IdentityNumber);
                    renium_mac_route_code::Replace(r.writers[i], std::vector<unsigned char>(r.prefixes[i], r.prefixes[i] + 16),
                        renium_history::Jump(reinterpret_cast<std::uintptr_t>(target)));
                    ++arrival->installed;
                }
            }
            if (!Exchange(r.joinSlot, reinterpret_cast<void*>(r.join), reinterpret_cast<void*>(&GetJoin))
                || (r.client && !Exchange(r.localSlot, reinterpret_cast<void*>(r.local), reinterpret_cast<void*>(&GetLocal))))
                throw std::runtime_error("Routed arrival getter changed");
            arrival->task = Arm(r.relay, [weak = std::weak_ptr<Arrival>(arrival)](Values values) {
                auto arrival = weak.lock();
                if (!arrival) throw std::runtime_error("Routed arrival was cancelled");
                const auto& r = arrival->request;
                if (!values || values->values.size() != 2 || values->values[0].type != r.tableType)
                    throw std::runtime_error("Routed arrival data changed type");
                std::vector<Instance> existing;
                if (!r.client) {
                    auto roots = Children(reinterpret_cast<void*>(r.relay.model), r.relay.children, 256);
                    if (roots) for (const auto& root : *roots) if (Class(root.get(), r.relay.klass, "Players")) {
                        auto players = Children(root.get(), r.relay.children, 256);
                        if (players) for (const auto& player : *players) if (Class(player.get(), r.relay.klass, "Player")) existing.push_back(player);
                    }
                }
                std::lock_guard lock(mutex); arrival->existing = std::move(existing); arrival->values = values;
            });
            std::thread([arrival] {
                for (;;) {
                    std::this_thread::sleep_for(std::chrono::milliseconds(100));
                    std::lock_guard lock(mutex);
                    if (current != arrival) return;
                    if (!arrival->Alive()) { try { Restore(arrival); } catch (...) {} return; }
                }
            }).detach();
        } catch (...) { Restore(arrival); throw; }
    } else {
        if (!current || current->request.token != r.token) throw std::runtime_error("Routed arrival lease ended");
        if (r.action == 2) Restore(current);
        else r.receipt = Complete(current->task) ? 2 : 0;
    }
    output.resize(sizeof(r)); std::memcpy(output.data(), &r, sizeof(r));
}
}
