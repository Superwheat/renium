// Transactional Terrain writes run entirely on the selected DataModel queue.
#pragma once
#include "renium_terrain_grid.h"
#include "renium_studio_history.h"
#include "renium_terrain_observation.h"
#if defined(_WIN32)
#include <bcrypt.h>
#pragma comment(lib, "bcrypt.lib")
#endif

namespace renium_terrain {
struct Binding {
    std::uintptr_t object, table, getter, setter, getterSlot, setterSlot;
};
static_assert(sizeof(Binding) == 48);
struct ClearBinding { std::uintptr_t descriptor, table, memberOffset, function; };
using Fingerprint = std::array<unsigned char, 64>;
struct Result { bool changed = false; Fingerprint fingerprint{}; };

static Fingerprint Hash(std::string_view smooth, std::string_view physics) {
    Fingerprint value{};
    std::size_t at = 0;
    for (const auto bytes : {smooth, physics}) {
#if defined(_WIN32)
        if (BCryptHash(BCRYPT_SHA256_ALG_HANDLE, nullptr, 0,
            reinterpret_cast<PUCHAR>(const_cast<char*>(bytes.data())), static_cast<ULONG>(bytes.size()),
            value.data() + at, 32) < 0) throw std::runtime_error("Cannot fingerprint Terrain");
#else
        CC_SHA256(bytes.data(), static_cast<CC_LONG>(bytes.size()), value.data() + at);
#endif
        at += 32;
    }
    return value;
}

template<class Read>
static void Validate(const Binding& binding, Read read, std::uintptr_t limit = 128) {
    std::uintptr_t value = 0;
    if (!binding.object || binding.getterSlot >= limit || binding.setterSlot >= limit ||
        !read(binding.object, &value, sizeof(value)) || value != binding.table ||
        !read(binding.table + binding.getterSlot, &value, sizeof(value)) || value != binding.getter ||
        !read(binding.table + binding.setterSlot, &value, sizeof(value)) || value != binding.setter)
        throw std::runtime_error("Terrain property binding changed before execution");
}

static std::string Get(const Binding& binding, void* terrain) {
#if defined(_WIN32)
    alignas(std::string) unsigned char storage[sizeof(std::string)];
    reinterpret_cast<void* (*)(void*, void*, void*)>(binding.getter)(reinterpret_cast<void*>(binding.object), storage, terrain);
    auto value = reinterpret_cast<std::string*>(storage);
    std::string result = std::move(*value);
    value->~basic_string();
    return result;
#else
    return reinterpret_cast<std::string (*)(void*, void*)>(binding.getter)(reinterpret_cast<void*>(binding.object), terrain);
#endif
}

static void Set(const Binding& binding, void* terrain, const std::string& value) {
    reinterpret_cast<void (*)(void*, void*, const std::string*)>(binding.setter)(reinterpret_cast<void*>(binding.object), terrain, &value);
}

struct Write {
    Binding smooth{}, physics{}, acquisition{};
    ClearBinding clear{};
    void* terrain;
    std::shared_ptr<void> owner;
    std::string beforeSmooth, beforePhysics, writtenSmooth, writtenPhysics;
    void RestoreAcquisition(const std::string& value) const {
        if (Get(acquisition, terrain) != value &&
            !reinterpret_cast<bool (*)(void*, void*, const std::string*)>(acquisition.setter)(
                reinterpret_cast<void*>(acquisition.object), terrain, &value))
            throw std::runtime_error("Studio did not restore Terrain acquisition metadata");
    }
    void ReplaceSmooth(const std::string& value) const {
        // Loading SmoothGrid stamps AcquisitionMethod=Legacy. Grid transfer
        // must preserve that separate saved field, including during rollback.
        const auto method = Get(acquisition, terrain);
        try {
            reinterpret_cast<void (*)(void*)>(clear.function)(terrain);
            Set(smooth, terrain, value);
        } catch (...) {
            RestoreAcquisition(method);
            throw;
        }
        RestoreAcquisition(method);
    }
    void Check(bool written) const {
        if (Get(smooth, terrain) != (written ? writtenSmooth : beforeSmooth) ||
            Get(physics, terrain) != (written ? writtenPhysics : beforePhysics))
            throw std::runtime_error("Terrain changed outside this sync transaction");
    }
    void Rollback() const {
        renium_terrain_observation::OwnWrite own(terrain);
        const auto currentSmooth = Get(smooth, terrain), currentPhysics = Get(physics, terrain);
        const auto restored = renium_terrain::Rollback(beforeSmooth, writtenSmooth, currentSmooth);
        if (restored != currentSmooth) {
            ReplaceSmooth(restored);
        }
        // Exact cancellation preserves the engine's original empty-chunk layout
        // too. With outside painting, the SmoothGrid setter updates its geometry.
        if (currentSmooth == writtenSmooth && currentPhysics == writtenPhysics && currentPhysics != beforePhysics)
            Set(physics, terrain, beforePhysics);
        if (Get(smooth, terrain) != restored)
            throw std::runtime_error("Studio did not restore the Terrain voxel grid");
    }
};

template<class Read>
static Result Apply(void* terrain, std::shared_ptr<void> owner, std::uintptr_t model,
    std::string_view input, Read read) {
    // Fixed bindings + lengths followed by token and the two native byte strings.
    constexpr std::size_t bindingsSize = sizeof(Binding) * 3 + sizeof(ClearBinding);
    constexpr std::size_t header = bindingsSize + sizeof(Fingerprint) + 16;
    if (input.size() < header || input.size() > 128 * 1024 * 1024)
        throw std::runtime_error("Invalid native Terrain payload");
    auto write = std::make_shared<Write>();
    std::memcpy(&write->smooth, input.data(), sizeof(Binding));
    std::memcpy(&write->physics, input.data() + sizeof(Binding), sizeof(Binding));
    std::memcpy(&write->clear, input.data() + sizeof(Binding) * 2, sizeof(ClearBinding));
    std::memcpy(&write->acquisition, input.data() + sizeof(Binding) * 2 + sizeof(ClearBinding), sizeof(Binding));
    std::uint32_t lengths[4]{};
    std::memcpy(lengths, input.data() + bindingsSize + sizeof(Fingerprint), sizeof(lengths));
    if (!lengths[0] || lengths[0] > 256 || !lengths[3] || lengths[3] > 4 ||
        (!(lengths[3] & 1) && lengths[1]) || (!(lengths[3] & 2) && lengths[2]) ||
        static_cast<std::uint64_t>(header) + lengths[0] + lengths[1] + lengths[2] != input.size())
        throw std::runtime_error("Invalid native Terrain field lengths");
    const std::string token(input.substr(header, lengths[0]));
    const std::string smooth(input.substr(header + lengths[0], lengths[1]));
    const std::string physics(input.substr(header + lengths[0] + lengths[1], lengths[2]));
    Validate(write->smooth, read); Validate(write->physics, read);
    Validate(write->acquisition, read, 256);
    std::uintptr_t member = 0;
    if (write->clear.memberOffset < 64 || write->clear.memberOffset >= 256 || write->clear.memberOffset % 8 ||
        !read(write->clear.descriptor, &member, sizeof(member)) || member != write->clear.table ||
        !read(write->clear.descriptor + write->clear.memberOffset, &member, sizeof(member)) || member != write->clear.function)
        throw std::runtime_error("Terrain Clear binding changed before execution");
    if (lengths[3] & 1) Index(smooth); // Reject malformed or unsupported formats before mutation.
    write->terrain = terrain; write->owner = std::move(owner);
    write->beforeSmooth = Get(write->smooth, terrain); write->beforePhysics = Get(write->physics, terrain);
    Index(write->beforeSmooth);
    const auto fingerprint = Hash(write->beforeSmooth, write->beforePhysics);
    if (lengths[3] == 4) return {false, fingerprint};
    if (std::memcmp(fingerprint.data(), input.data() + bindingsSize, fingerprint.size()) != 0)
        throw std::runtime_error("Terrain changed outside this sync transaction");
    std::function<void()> check;
    {
        std::lock_guard lock(renium_history::registrationsMutex);
        const auto found = renium_history::registrations.find(token);
        if (found == renium_history::registrations.end() || found->second.model != model)
            throw std::runtime_error("Terrain write has no registered history transaction");
        check = found->second.checkTerrain;
    }
    if (check) check();
    const bool changeSmooth = (lengths[3] & 1) && write->beforeSmooth != smooth;
    const bool changePhysics = (lengths[3] & 2) && write->beforePhysics != physics;
    if (!changeSmooth && !changePhysics) return {false, fingerprint};
    write->writtenSmooth = write->beforeSmooth; write->writtenPhysics = write->beforePhysics;
    {
        std::lock_guard lock(renium_history::registrationsMutex);
        const auto found = renium_history::registrations.find(token);
        if (found == renium_history::registrations.end() || found->second.model != model)
            throw std::runtime_error("Terrain write has no registered history transaction");
        const auto previous = found->second.rollbackTerrain;
        found->second.rollbackTerrain = [write, previous] { write->Rollback(); if (previous) previous(); };
    }
    // Capture the actual accepted state even if an engine setter throws. A
    // daemon timeout cannot drop the restoration stored with its history token.
    try {
        renium_terrain_observation::OwnWrite own(terrain);
        if (changeSmooth) {
            // SmoothGrid loads only listed chunks; it never removes stale ones.
            write->ReplaceSmooth(smooth);
        }
        write->writtenSmooth = Get(write->smooth, terrain);
        if ((lengths[3] & 2) && Get(write->physics, terrain) != physics) Set(write->physics, terrain, physics);
        write->writtenPhysics = Get(write->physics, terrain);
    } catch (...) {
        write->writtenSmooth = Get(write->smooth, terrain); write->writtenPhysics = Get(write->physics, terrain);
        throw;
    }
    if (((lengths[3] & 1) && write->writtenSmooth != smooth) || ((lengths[3] & 2) && write->writtenPhysics != physics))
        throw std::runtime_error("Studio did not accept the complete Terrain voxel grid");
    {
        std::lock_guard lock(renium_history::registrationsMutex);
        auto& registration = renium_history::registrations.at(token);
        registration.checkTerrain = [write] { write->Check(true); };
        registration.terrainWritten = true;
    }
    return {true, Hash(write->writtenSmooth, write->writtenPhysics)};
}
}
