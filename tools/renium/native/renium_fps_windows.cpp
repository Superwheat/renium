#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <mutex>
#include <stdexcept>
#include "renium_fps_policy.h"

namespace {
using ShouldSkipRenderFn = bool(__fastcall*)(void*, void*, void*, void*);

constexpr std::size_t kSchedulerSlotCount = 8;
constexpr std::uint64_t kRecentCheckWindow = 64;

struct SchedulerSlot {
    std::atomic<std::uintptr_t> object{0};
    std::atomic<std::int32_t> throttle_count{0};
    std::atomic<std::uint64_t> seen_seq{0};
};

struct Layout {
    std::uint32_t frame_counter_offset = 0;
    std::uint32_t throttle_count_offset = 0;
    std::int32_t reset_counter_value = 0;
    std::uint64_t vtable_slot_addr = 0;
    std::uint64_t expected_should_skip_addr = 0;
};

struct State {
    std::mutex mutex;
    std::atomic<unsigned> mode{renium_fps::kActionDefault};
    std::atomic<std::uint64_t> deadline{0};
    std::atomic<ShouldSkipRenderFn> original_should_skip{nullptr};
    std::atomic<std::uint32_t> frame_counter_offset{0};
    std::atomic<std::uint32_t> throttle_count_offset{0};
    std::atomic<std::int32_t> reset_counter_value{0};
    std::atomic<std::uint64_t> render_checks{0};
    std::atomic<std::uint64_t> unthrottled_frames{0};
    SchedulerSlot schedulers[kSchedulerSlotCount]{};
    bool hook_installed = false;
    void* hooked_slot = nullptr;
};

State& GetState() {
    static auto* state = new State;
    return *state;
}

void RecordSchedulerThrottle(State& state, void* self, std::int32_t throttle_count, std::uint64_t seq) {
    const auto key = reinterpret_cast<std::uintptr_t>(self);
    std::size_t empty_slot = kSchedulerSlotCount;
    std::size_t oldest_slot = 0;
    std::uint64_t oldest_seq = UINT64_MAX;
    for (std::size_t i = 0; i < kSchedulerSlotCount; ++i) {
        const auto existing = state.schedulers[i].object.load(std::memory_order_relaxed);
        if (existing == key) {
            state.schedulers[i].throttle_count.store(throttle_count, std::memory_order_relaxed);
            state.schedulers[i].seen_seq.store(seq, std::memory_order_release);
            return;
        }
        if (existing == 0 && empty_slot == kSchedulerSlotCount) {
            empty_slot = i;
        }
        const auto seen = state.schedulers[i].seen_seq.load(std::memory_order_relaxed);
        if (seen < oldest_seq) {
            oldest_seq = seen;
            oldest_slot = i;
        }
    }
    const std::size_t target = empty_slot < kSchedulerSlotCount ? empty_slot : oldest_slot;
    state.schedulers[target].object.store(key, std::memory_order_relaxed);
    state.schedulers[target].throttle_count.store(throttle_count, std::memory_order_relaxed);
    state.schedulers[target].seen_seq.store(seq, std::memory_order_release);
}

std::int32_t ReadActiveThrottleCount(const State& state) {
    const auto current_seq = state.render_checks.load(std::memory_order_acquire);
    std::int32_t recent_max = 0;
    bool has_recent = false;
    std::int32_t any_max = 0;
    for (std::size_t i = 0; i < kSchedulerSlotCount; ++i) {
        if (state.schedulers[i].object.load(std::memory_order_relaxed) == 0) {
            continue;
        }
        const auto seen = state.schedulers[i].seen_seq.load(std::memory_order_acquire);
        const auto count = state.schedulers[i].throttle_count.load(std::memory_order_relaxed);
        if (count > any_max) {
            any_max = count;
        }
        if (current_seq >= seen && (current_seq - seen) <= kRecentCheckWindow) {
            if (!has_recent || count > recent_max) {
                recent_max = count;
                has_recent = true;
            }
        }
    }
    return has_recent ? recent_max : any_max;
}

bool PageRangeMatches(std::uint64_t addr, std::size_t size, DWORD allowed_mask) {
    if (addr == 0 || size == 0) return false;
    std::uintptr_t current = static_cast<std::uintptr_t>(addr);
    const std::uintptr_t end = current + size;
    if (end < current) return false;
    while (current < end) {
        MEMORY_BASIC_INFORMATION info{};
        if (!VirtualQuery(reinterpret_cast<const void*>(current), &info, sizeof(info))) {
            return false;
        }
        if (info.State != MEM_COMMIT) return false;
        if (info.Protect & (PAGE_GUARD | PAGE_NOACCESS)) return false;
        if ((info.Protect & allowed_mask) == 0) return false;
        const auto region_end =
            reinterpret_cast<std::uintptr_t>(info.BaseAddress) + info.RegionSize;
        if (region_end <= current) return false;
        current = region_end;
    }
    return true;
}

bool IsReadable(std::uint64_t addr, std::size_t size) {
    constexpr DWORD kReadable = PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY |
        PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    return PageRangeMatches(addr, size, kReadable);
}

bool IsExecutable(std::uint64_t addr) {
    constexpr DWORD kExecutable = PAGE_EXECUTE | PAGE_EXECUTE_READ |
        PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    return PageRangeMatches(addr, 1, kExecutable);
}

bool __fastcall HookedShouldSkipRender(void* self, void* a2, void* a3, void* a4) {
    auto& state = GetState();
    auto* original = state.original_should_skip.load(std::memory_order_acquire);
    const auto seq = state.render_checks.fetch_add(1, std::memory_order_relaxed) + 1;
    bool bypassed_throttle = false;
    if (self != nullptr) {
        const auto throttle_offset = state.throttle_count_offset.load(std::memory_order_acquire);
        const auto throttle_count = *reinterpret_cast<const std::int32_t*>(
            static_cast<const char*>(self) + throttle_offset);
        RecordSchedulerThrottle(state, self, throttle_count, seq);
        if (throttle_count > 0 &&
            state.mode.load(std::memory_order_acquire) == renium_fps::kActionUnlock &&
            renium_fps::Unlocked(
                renium_fps::kActionUnlock,
                state.deadline.load(std::memory_order_acquire),
                GetTickCount64())) {
            const auto counter_offset =
                state.frame_counter_offset.load(std::memory_order_acquire);
            *reinterpret_cast<std::int32_t*>(static_cast<char*>(self) + counter_offset) =
                state.reset_counter_value.load(std::memory_order_acquire);
            bypassed_throttle = true;
        }
    }
    const bool skipped = original ? original(self, a2, a3, a4) : false;
    if (bypassed_throttle && !skipped) {
        state.unthrottled_frames.fetch_add(1, std::memory_order_relaxed);
    }
    return skipped;
}

void ProbeRenderVtable(const Layout& layout) {
    if (!IsReadable(layout.vtable_slot_addr, sizeof(void*))) {
        throw std::runtime_error("RenderSchedulerState vtable slot is not readable");
    }
    if (!IsExecutable(layout.expected_should_skip_addr)) {
        throw std::runtime_error("RenderSchedulerState::shouldSkipRender is not executable");
    }
    if (layout.reset_counter_value <= 0 || layout.reset_counter_value > 1023) {
        throw std::runtime_error("RenderSchedulerState frame counter modulus is out of range");
    }
    void* const current =
        *reinterpret_cast<void* const*>(static_cast<std::uintptr_t>(layout.vtable_slot_addr));
    void* const expected =
        reinterpret_cast<void*>(static_cast<std::uintptr_t>(layout.expected_should_skip_addr));
    if (current != expected && current != reinterpret_cast<void*>(&HookedShouldSkipRender)) {
        throw std::runtime_error(
            "RenderSchedulerState vtable slot does not match its probed entry");
    }
}

void InstallRenderHook(State& state, const Layout& layout) {
    auto* raw_slot =
        reinterpret_cast<void*>(static_cast<std::uintptr_t>(layout.vtable_slot_addr));
    auto* slot = static_cast<void* volatile*>(raw_slot);
    void* const current = *slot;
    if (current == reinterpret_cast<void*>(&HookedShouldSkipRender)) {
        state.hook_installed = true;
        state.hooked_slot = raw_slot;
        return;
    }
    void* const expected =
        reinterpret_cast<void*>(static_cast<std::uintptr_t>(layout.expected_should_skip_addr));
    state.original_should_skip.store(
        reinterpret_cast<ShouldSkipRenderFn>(expected), std::memory_order_release);
    state.frame_counter_offset.store(layout.frame_counter_offset, std::memory_order_release);
    state.throttle_count_offset.store(layout.throttle_count_offset, std::memory_order_release);
    state.reset_counter_value.store(layout.reset_counter_value, std::memory_order_release);

    DWORD old_protect = 0;
    if (!VirtualProtect(raw_slot, sizeof(void*), PAGE_READWRITE, &old_protect)) {
        throw std::runtime_error("Could not unlock RenderSchedulerState vtable protection");
    }
    void* const replaced = InterlockedCompareExchangePointer(
        slot, reinterpret_cast<void*>(&HookedShouldSkipRender), expected);
    DWORD unused = 0;
    VirtualProtect(raw_slot, sizeof(void*), old_protect, &unused);
    if (replaced != expected) {
        throw std::runtime_error("RenderSchedulerState vtable slot changed during attachment");
    }
    state.hook_installed = true;
    state.hooked_slot = raw_slot;
}

void RestoreRenderHook(State& state) {
    if (!state.hook_installed || !state.hooked_slot) return;
    auto* raw_slot = state.hooked_slot;
    auto* slot = static_cast<void* volatile*>(raw_slot);
    auto* original = state.original_should_skip.load(std::memory_order_acquire);
    if (original != nullptr && *slot == reinterpret_cast<void*>(&HookedShouldSkipRender)) {
        DWORD old_protect = 0;
        if (VirtualProtect(raw_slot, sizeof(void*), PAGE_READWRITE, &old_protect)) {
            InterlockedCompareExchangePointer(
                slot,
                reinterpret_cast<void*>(original),
                reinterpret_cast<void*>(&HookedShouldSkipRender));
            DWORD unused = 0;
            VirtualProtect(raw_slot, sizeof(void*), old_protect, &unused);
        }
    }
    state.hook_installed = false;
    state.hooked_slot = nullptr;
}

struct Parameters {
    std::uint32_t size;
    std::uint32_t version;
    std::uint32_t action;
    std::uint32_t frame_counter_offset;
    std::uint32_t throttle_count_offset;
    std::int32_t reset_counter_value;
    std::uint64_t vtable_slot_addr;
    std::uint64_t expected_should_skip_addr;
    std::uint32_t status;
    std::uint32_t unlocked;
    std::uint32_t reserved_focused;
    std::uint32_t hook_installed;
    std::int32_t last_throttle_count;
    std::uint32_t reserved_focus_throttled;
    std::uint64_t render_checks;
    std::uint64_t unthrottled_frames;
    char error[256];
};
static_assert(sizeof(Parameters) == 336);
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumFpsStep(void* input) {
    auto* params = static_cast<Parameters*>(input);
    if (!params || params->size != sizeof(Parameters) || params->version != 1 ||
        params->action > renium_fps::kActionUnlock) {
        return ERROR_INVALID_PARAMETER;
    }
    auto& state = GetState();
    std::lock_guard<std::mutex> lock(state.mutex);
    try {
        HMODULE module = nullptr;
        if (!GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                reinterpret_cast<LPCWSTR>(&ReniumFpsStep),
                &module)) {
            throw std::runtime_error("Could not retain Studio FPS helper");
        }
        const Layout layout{
            params->frame_counter_offset,
            params->throttle_count_offset,
            params->reset_counter_value,
            params->vtable_slot_addr,
            params->expected_should_skip_addr,
        };
        ProbeRenderVtable(layout);

        const auto now = GetTickCount64();
        if (params->action == renium_fps::kActionUnlock) {
            InstallRenderHook(state, layout);
            state.deadline.store(now + renium_fps::kLeaseMs, std::memory_order_release);
            state.mode.store(renium_fps::kActionUnlock, std::memory_order_release);
        } else if (params->action == renium_fps::kActionDefault) {
            state.mode.store(renium_fps::kActionDefault, std::memory_order_release);
            state.deadline.store(0, std::memory_order_release);
            RestoreRenderHook(state);
        }

        const bool is_unlocked = renium_fps::Unlocked(
            state.mode.load(std::memory_order_acquire),
            state.deadline.load(std::memory_order_acquire),
            GetTickCount64());
        params->unlocked = is_unlocked ? 1 : 0;
        params->reserved_focused = 0;
        params->hook_installed = state.hook_installed ? 1 : 0;
        params->last_throttle_count = ReadActiveThrottleCount(state);
        params->reserved_focus_throttled = 0;
        params->render_checks = state.render_checks.load(std::memory_order_relaxed);
        params->unthrottled_frames = state.unthrottled_frames.load(std::memory_order_relaxed);
        params->status = 1;
    } catch (const std::exception& error) {
        std::snprintf(params->error, sizeof(params->error), "%s", error.what());
        params->status = 0;
    }
    return 0;
}

#ifndef RENIUM_FPS_NO_DLLMAIN
BOOL WINAPI DllMain(HINSTANCE, DWORD, LPVOID) { return TRUE; }
#endif
