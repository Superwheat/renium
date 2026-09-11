// A private relay exposes native voxel changes without polling or changing
// Studio's protected property descriptors. Only Terrain's secondary vptr changes.
#pragma once
#include <memory>
#include <mutex>
#include <unordered_map>
#include <vector>

namespace renium_terrain_observation {
struct Binding {
    std::uintptr_t table, offset, count, slot;
};
struct Relay {
    std::uintptr_t instance, owner, classDescriptor, parent, identityBinding, identityGetter;
    unsigned char identity[16];
    std::uintptr_t descriptor, descriptorTable, setter;
};
struct Request { Binding binding; Relay relay; };
static_assert(sizeof(Binding) == 32 && sizeof(Request) == 120);
struct Table { Binding binding; std::vector<std::uintptr_t> entries; };
struct Subscription {
    Relay relay;
    std::shared_ptr<renium_history::WeakOwner> terrainOwner, relayOwner;
    std::function<std::shared_ptr<void>()> retain;
};
static std::mutex tablesMutex;
// Immutable clones contain no instance owners: one per engine interface.
static std::unordered_map<std::uintptr_t, std::unique_ptr<Table>> tables;
static std::unordered_map<void*, std::shared_ptr<Subscription>> subscriptions;
static std::unordered_map<void*, std::size_t> ownWrites;
struct OwnWrite {
    void* terrain;
    explicit OwnWrite(void* target) : terrain(target) {
        std::lock_guard lock(tablesMutex); ++ownWrites[terrain];
    }
    ~OwnWrite() {
        std::lock_guard lock(tablesMutex);
        const auto found = ownWrites.find(terrain);
        if (--found->second == 0) ownWrites.erase(found);
    }
};

static Table* Find(std::uintptr_t vtable) {
    std::lock_guard lock(tablesMutex);
    for (const auto& [original, table] : tables)
        if (table && vtable == reinterpret_cast<std::uintptr_t>(table->entries.data() + 2)) return table.get();
    return nullptr;
}

static void Changed(void* listener, const void* region, std::uint32_t flags) {
    const auto& binding = Find(*reinterpret_cast<std::uintptr_t*>(listener))->binding;
    reinterpret_cast<void (*)(void*, const void*, std::uint32_t)>(
        *reinterpret_cast<std::uintptr_t*>(binding.table + binding.slot))(listener, region, flags);
    auto terrain = static_cast<unsigned char*>(listener) - binding.offset;
    std::shared_ptr<Subscription> subscription;
    {
        std::lock_guard lock(tablesMutex);
        if (ownWrites.contains(terrain)) return;
        const auto found = subscriptions.find(terrain);
        if (found == subscriptions.end()) return;
        subscription = found->second;
        if (!subscription->terrainOwner->Alive() || !subscription->relayOwner->Alive()) {
            subscriptions.erase(found);
            return;
        }
    }
    if (const auto held = subscription->retain()) {
        const auto& relay = subscription->relay;
        const std::string value = "true";
        reinterpret_cast<bool (*)(void*, void*, const std::string*)>(relay.setter)(
            reinterpret_cast<void*>(relay.descriptor), reinterpret_cast<void*>(relay.instance), &value);
    }
}

static Binding GetBinding(void* terrain) {
    std::lock_guard lock(tablesMutex);
    for (const auto& [original, table] : tables)
        if (table && *reinterpret_cast<std::uintptr_t*>(static_cast<unsigned char*>(terrain) + table->binding.offset)
            == reinterpret_cast<std::uintptr_t>(table->entries.data() + 2)) return table->binding;
    return {};
}

template<class Read, class Retain, class Release>
static void Install(void* terrain, void* terrainOwner, const Request& request,
    std::uintptr_t classOffset, std::uintptr_t selfOffset, std::uintptr_t parentOffset,
    Read read, Retain retain, Release release) {
    const auto& binding = request.binding;
    const auto& relay = request.relay;
    if (!binding.offset || binding.offset > 4096 || binding.offset % 8 ||
        binding.count < 2 || binding.count > 16 || binding.slot >= binding.count * 8 || binding.slot % 8)
        throw std::runtime_error("Invalid Terrain notification layout");
    auto listener = reinterpret_cast<std::uintptr_t*>(static_cast<unsigned char*>(terrain) + binding.offset);
    const auto existing = Find(*listener);
    const auto equal = [&](std::uintptr_t address, std::uintptr_t expected) {
        std::uintptr_t value = 0;
        return read(address, &value, sizeof(value)) && value == expected;
    };
    if (!relay.setter || !equal(relay.descriptor, relay.descriptorTable) ||
        (*listener != binding.table && (!existing || existing->binding.table != binding.table)) ||
        !equal(relay.instance + classOffset, relay.classDescriptor) || !equal(relay.instance + selfOffset, relay.instance) ||
        !equal(relay.instance + selfOffset + 8, relay.owner) || !equal(relay.instance + parentOffset, relay.parent))
        throw std::runtime_error("Terrain notification target changed");
    if (!retain(reinterpret_cast<void*>(relay.owner))) throw std::runtime_error("Terrain relay expired");
    const auto relayHold = std::shared_ptr<void>(reinterpret_cast<void*>(relay.owner), release);
    struct Identity { std::uint64_t low, high; } identity{};
#if defined(_WIN32)
    reinterpret_cast<void* (*)(void*, void*, void*)>(relay.identityGetter)(
        reinterpret_cast<void*>(relay.identityBinding), &identity, reinterpret_cast<void*>(relay.instance));
#else
    identity = reinterpret_cast<Identity (*)(void*, void*)>(relay.identityGetter)(
        reinterpret_cast<void*>(relay.identityBinding), reinterpret_cast<void*>(relay.instance));
#endif
    if (std::memcmp(&identity, relay.identity, sizeof(identity))) throw std::runtime_error("Terrain relay identity changed");
    auto subscription = std::make_shared<Subscription>();
    subscription->relay = relay;
    subscription->terrainOwner = std::make_shared<renium_history::WeakOwner>(terrainOwner);
    subscription->relayOwner = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(relay.owner));
    subscription->retain = [weak = subscription->relayOwner, retain, release]() -> std::shared_ptr<void> {
        if (!retain(weak->owner)) return {};
        return std::shared_ptr<void>(weak->owner, release);
    };
    std::lock_guard lock(tablesMutex);
    for (auto it = subscriptions.begin(); it != subscriptions.end();) {
        if (!it->second->terrainOwner->Alive() || !it->second->relayOwner->Alive()) it = subscriptions.erase(it);
        else ++it;
    }
    auto& table = tables[binding.table];
    if (!table) {
        auto next = std::make_unique<Table>();
        next->binding = binding;
        next->entries.resize(binding.count + 2);
        if (!read(binding.table - 16, next->entries.data(), next->entries.size() * 8))
            throw std::runtime_error("Cannot copy Terrain listener interface");
        next->entries[2 + binding.slot / 8] = reinterpret_cast<std::uintptr_t>(&Changed);
        table = std::move(next);
    } else if (std::memcmp(&table->binding, &binding, sizeof(binding))) {
        throw std::runtime_error("Conflicting Terrain notification layout");
    }
    subscriptions[terrain] = std::move(subscription);
    *listener = reinterpret_cast<std::uintptr_t>(table->entries.data() + 2);
}
}
