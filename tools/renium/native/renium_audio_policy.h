#pragma once
#include <cstdint>

namespace renium_audio {
inline bool Muted(unsigned mode, std::uint64_t deadline, std::uint64_t now, bool focused) {
    return now < deadline && (mode == 2 || (mode == 4 && !focused));
}
}
