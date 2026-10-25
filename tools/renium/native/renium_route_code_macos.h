#pragma once
#include "renium_playtest_macos.h"

namespace renium_mac_route_code {
// Serialize with the existing signed-page installer. Patching one instruction
// preserves other qualified changes on the same executable page.
inline void Call(std::uint64_t at, std::uint64_t from, std::uint64_t to) {
    std::lock_guard lock(renium_history::installationMutex);
    std::uint32_t word = 0;
    std::memcpy(&word, reinterpret_cast<void*>(at), 4);
    const auto displacement = static_cast<std::int64_t>(to) - static_cast<std::int64_t>(at);
    const auto previous = static_cast<std::int64_t>(static_cast<std::int32_t>(word << 6) >> 4);
    if ((word & 0xfc000000) != 0x94000000 || at + previous != from || displacement % 4
        || displacement < -(1 << 27) || displacement >= (1 << 27))
        throw std::runtime_error("Native route call changed or moved out of range");
    const auto next = 0x94000000u | (static_cast<std::uint32_t>(displacement >> 2) & 0x3ffffff);
    const auto bytes = reinterpret_cast<const unsigned char*>(&next);
    renium_history::ReplaceCode(at, std::vector<unsigned char>(bytes, bytes + 4));
}
inline void Replace(std::uint64_t entry, const std::vector<unsigned char>& expected, const std::vector<unsigned char>& next) {
    std::lock_guard lock(renium_history::installationMutex);
    if (expected.size() != next.size() || std::memcmp(reinterpret_cast<void*>(entry), expected.data(), expected.size()))
        throw std::runtime_error("Native route function changed");
    renium_history::ReplaceCode(entry, next);
}
inline void* Forward(std::uint64_t entry, const unsigned char* prefix) {
    // Stack saves, frame setup and register moves are position independent.
    // x16 belongs to the absolute jump, so a prefix cannot establish its value.
    for (unsigned i = 0; i < 16; i += 4) {
        std::uint32_t word = 0; std::memcpy(&word, prefix + i, 4);
        const auto dst = word & 31, base = (word >> 5) & 31, source = (word >> 16) & 31;
        const bool storePair = (word & 0x3a400000) == 0x28000000 && base == 31;
        const bool frame = (word & 0x1f000000) == 0x11000000 && base == 31 && (dst == 29 || dst == 31);
        const bool move = (word & 0x7fe0ffe0) == 0x2a0003e0 && dst != 16 && source != 16;
        if (!storePair && !frame && !move && word != 0xd503201f)
            throw std::runtime_error("Native route function prefix is not position independent");
    }
    if (std::memcmp(reinterpret_cast<void*>(entry), prefix, 16)) throw std::runtime_error("Native route prefix changed");
    renium_history::Binding binding{};
    binding.voxelUndo = entry; binding.prefixSize = 16; std::memcpy(binding.prefix, prefix, 16);
    return renium_history::Trampoline(binding);
}
}
