#pragma once
#include "renium_playtest_macos.h"
#include <sys/mman.h>
#include <strings.h>

namespace renium_mac_route_launch {
struct Shared {
    std::atomic<std::uint32_t> phase{0};
    std::uint32_t client, server, length;
    std::uint64_t token, until;
    char command[65536];
};
inline Shared* shared = nullptr;
inline std::mutex mutex;
inline std::uint64_t Now() {
    timespec value{}; clock_gettime(CLOCK_MONOTONIC, &value);
    return static_cast<std::uint64_t>(value.tv_sec) * 1000 + value.tv_nsec / 1000000;
}
struct Request {
    std::uint32_t version, size, action, phase;
    std::uint64_t token;
    std::uint32_t client, reserved;
};
inline void Handle(const std::string& payload, std::vector<unsigned char>& output) {
    Request r{};
    if (payload.size() != sizeof(r)) throw std::runtime_error("Truncated client launch capture");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 1 || r.size != sizeof(r) || !r.token || r.reserved || r.action < 1 || r.action > 3)
        throw std::runtime_error("Incompatible client launch capture");
    std::lock_guard lock(mutex);
    if (!shared) {
        void* page = mmap(nullptr, sizeof(Shared), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
        if (page == MAP_FAILED) throw std::runtime_error("Cannot create client launch receipt");
        shared = new(page) Shared();
    }
    if (r.action == 1) {
        if (!r.client || kill(r.client, 0) || (shared->phase.load() == 1 && Now() < shared->until))
            throw std::runtime_error("Client launch capture already owned or client ended");
        shared->phase = 0; shared->client = r.client; shared->server = getpid();
        shared->length = 0; shared->token = r.token; shared->until = Now() + 15000;
        shared->phase.store(1, std::memory_order_release);
    } else {
        if (shared->token != r.token) throw std::runtime_error("Client launch capture was replaced");
        if (r.action == 2) shared->phase = 0;
        else {
            r.phase = shared->phase.load(std::memory_order_acquire);
            if (r.phase == 3) throw std::runtime_error("Client launch command exceeds its bound");
        }
    }
    output.resize(sizeof(r)); std::memcpy(output.data(), &r, sizeof(r));
    if (r.action == 3 && r.phase == 2)
        output.insert(output.end(), shared->command, shared->command + shared->length);
}
}

// Called after fork as well as from posix_spawn: no allocation, C++ locks, or
// Objective-C. The receipt lives in shared anonymous memory inherited by fork.
extern "C" bool ReniumCaptureTestClient(char* const args[], pid_t* pid) {
    using namespace renium_mac_route_launch;
    auto state = shared;
    if (!state || state->phase.load(std::memory_order_acquire) != 1 || Now() >= state->until) return false;
    bool client = false, owner = false;
    for (unsigned i = 0; i < 512 && args[i]; ++i) {
        if (!args[i + 1]) break;
        if (!strcasecmp(args[i], "-task") && !strcmp(args[i + 1], "StartClient")) client = true;
        if (!strcasecmp(args[i], "-parentPid")) {
            unsigned value = 0;
            const char* at = args[i + 1];
            while (*at >= '0' && *at <= '9' && value < 10000000) value = value * 10 + *at++ - '0';
            owner = !*at && value == state->server;
        }
    }
    if (!client || !owner) return false;
    std::uint32_t expected = 1;
    if (!state->phase.compare_exchange_strong(expected, 4)) return false;
    std::size_t length = 0;
    for (unsigned i = 0; args[i]; ++i) {
        const auto available = sizeof(state->command) - length;
        const auto size = strnlen(args[i], available);
        if (i >= 512 || size == available) { state->phase.store(3, std::memory_order_release); return false; }
        std::memcpy(state->command + length, args[i], size + 1); length += size + 1;
    }
    state->length = static_cast<std::uint32_t>(length);
    if (pid) *pid = state->client;
    state->phase.store(2, std::memory_order_release);
    return true;
}
