#pragma once
#include <cstdint>

namespace renium_fps {
constexpr unsigned kActionStatus = 0;
constexpr unsigned kActionDefault = 1;
constexpr unsigned kActionUnlock = 2;

constexpr std::uint64_t kLeaseMs = 5000;

inline bool Unlocked(unsigned mode, std::uint64_t deadline, std::uint64_t now) {
    return mode == kActionUnlock && now < deadline;
}
}
