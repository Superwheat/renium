#include <array>
#include <cassert>
#include <cstdint>
#include <cstring>
#include <functional>
#include <stdexcept>
#include <string>

struct Owner { bool alive = true; };
namespace renium_history {
struct WeakOwner {
    void* owner;
    explicit WeakOwner(void* value) : owner(value) {}
    bool Alive() const { return static_cast<Owner*>(owner)->alive; }
};
}
#include "../renium_terrain_observation.h"

static unsigned originalCalls = 0, relayCalls = 0;
static void Original(void*, const void*, std::uint32_t) { ++originalCalls; }
static bool Notify(void*, void*, const std::string* value) { assert(*value == "true"); ++relayCalls; return true; }
struct Identity { std::uint64_t low, high; };
#if defined(_WIN32)
static void* GetIdentity(void*, void* output, void*) {
    const Identity identity{17, 29}; std::memcpy(output, &identity, sizeof(identity)); return output;
}
#else
static Identity GetIdentity(void*, void*) { return {17, 29}; }
#endif

int main() {
    using namespace renium_terrain_observation;
    std::array<std::array<std::uintptr_t, 5>, 3> originals{};
    const std::array<std::size_t, 3> offsets{72, 256, 640};
    const auto read = [](std::uintptr_t address, void* output, std::size_t size) {
        std::memcpy(output, reinterpret_cast<void*>(address), size); return true;
    };
    const auto retain = [](void* owner) { return static_cast<Owner*>(owner)->alive; };
    const auto release = [](void*) {};
    for (std::size_t index = 0; index < offsets.size(); ++index) {
        std::array<std::uintptr_t, 128> terrain{}, relay{}, core{};
        Owner terrainOwner, relayOwner;
        auto& engine = originals[index];
        engine = {0, 0x1234, reinterpret_cast<std::uintptr_t>(&Original),
            reinterpret_cast<std::uintptr_t>(&Original), reinterpret_cast<std::uintptr_t>(&Original)};
        auto oldGeneration = engine;
        oldGeneration[3] = reinterpret_cast<std::uintptr_t>(&Notify);
        terrain[offsets[index] / 8] = reinterpret_cast<std::uintptr_t>(oldGeneration.data() + 2);
        const auto model = reinterpret_cast<std::uintptr_t>(&index);
        core[7] = model;
        relay[1] = reinterpret_cast<std::uintptr_t>(relay.data());
        relay[2] = reinterpret_cast<std::uintptr_t>(&relayOwner);
        relay[5] = 0x4567;
        relay[7] = reinterpret_cast<std::uintptr_t>(core.data());
        std::uintptr_t descriptor = 0x6789;
        Request request{};
        request.binding = {reinterpret_cast<std::uintptr_t>(engine.data() + 2), offsets[index], 3, 8};
        request.relay = {relay[1], relay[2], relay[5], relay[7], 0,
            reinterpret_cast<std::uintptr_t>(&GetIdentity), {},
            reinterpret_cast<std::uintptr_t>(&descriptor), descriptor, reinterpret_cast<std::uintptr_t>(&Notify)};
        const Identity identity{17, 29};
        std::memcpy(request.relay.identity, &identity, sizeof(identity));
        const auto install = [&] {
            Install(terrain.data(), &terrainOwner, request, 40, 8, 56, model, read, retain, release);
        };
        install();
        assert(relayCalls == index * 101);
        request.catchUp = 1;
        assert(GetBinding(terrain.data()).table == request.binding.table);
        assert(subscriptions.size() == 1);
        const auto notify = [&] { Changed(terrain.data() + offsets[index] / 8, nullptr, 1); };
        for (unsigned n = 0; n < 50; ++n) {
            const auto before = relayCalls;
            const auto forwarded = originalCalls;
            notify();
            assert(relayCalls == before + 1 && originalCalls == forwarded + 1);
            { OwnWrite own(terrain.data()); notify(); }
            assert(relayCalls == before + 1 && originalCalls == forwarded + 2);
            install();
            assert(relayCalls == before + 2 && subscriptions.size() == 1);
        }
        core[7] = model + 8;
        bool refused = false;
        try { install(); } catch (const std::runtime_error&) { refused = true; }
        assert(refused);
        core[7] = model;
        relayOwner.alive = false;
        const auto before = relayCalls;
        notify();
        assert(relayCalls == before && subscriptions.empty());
        relayOwner.alive = true;
        install();
        terrainOwner.alive = false;
        notify();
        assert(subscriptions.empty());
    }
    assert(tables.size() == originals.size());
}
