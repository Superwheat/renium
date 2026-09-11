// Registered Renium cancellations restore voxels explicitly. All other history
// operations, including another DataModel's undo during a callback, retain the
// engine's normal voxel playback. Studio can migrate a suspended history call
// to another thread; cancellation ownership belongs to that history object.
#pragma once
#include <array>
#include <atomic>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <vector>
#if defined(_WIN32)
#include <TlHelp32.h>
#else
#include <libkern/OSCacheControl.h>
#include <sys/mman.h>
#include <unistd.h>
#include "renium_signed_code_macos.h"
#endif

namespace renium_history {
struct Binding {
    std::uintptr_t descriptor, table, memberOffset, finish, voxelUndo;
    std::uint32_t prefixSize, reserved;
    unsigned char prefix[32];
};
static_assert(sizeof(Binding) == 80);
using Options = std::optional<std::shared_ptr<const void>>;
using Finish = void (*)(void*, std::string, std::uint32_t, Options);
using VoxelUndo = void (*)(void*, bool);

// The engine uses MSVC/libc++ shared ownership on the respective platforms.
// Keep only its control block alive, so a registration cannot pin a closed place.
struct WeakOwner {
    void* owner;
    explicit WeakOwner(void* value) : owner(value) {
#if defined(_WIN32)
        InterlockedIncrement(reinterpret_cast<volatile long*>(static_cast<unsigned char*>(owner) + 12));
#else
        reinterpret_cast<std::atomic<std::int64_t>*>(static_cast<unsigned char*>(owner) + 16)->fetch_add(1, std::memory_order_relaxed);
#endif
    }
    bool Alive() const {
#if defined(_WIN32)
        return InterlockedCompareExchange(reinterpret_cast<volatile long*>(static_cast<unsigned char*>(owner) + 8), 0, 0) > 0;
#else
        return reinterpret_cast<std::atomic<std::int64_t>*>(static_cast<unsigned char*>(owner) + 8)->load(std::memory_order_acquire) >= 0;
#endif
    }
    ~WeakOwner() {
#if defined(_WIN32)
        if (InterlockedDecrement(reinterpret_cast<volatile long*>(static_cast<unsigned char*>(owner) + 12)) == 0)
            reinterpret_cast<void (*)(void*)>((*reinterpret_cast<void***>(owner))[1])(owner);
#else
        if (reinterpret_cast<std::atomic<std::int64_t>*>(static_cast<unsigned char*>(owner) + 16)->fetch_sub(1, std::memory_order_acq_rel) == 0)
            reinterpret_cast<void (*)(void*)>((*reinterpret_cast<void***>(owner))[3])(owner);
#endif
    }
};

struct Registration {
    void* history;
    std::uintptr_t model;
    std::shared_ptr<WeakOwner> modelOwner;
    // A successful native Terrain write supplies its exact restoration here.
    std::function<void()> rollbackTerrain;
    std::function<void()> checkTerrain;
    bool terrainWritten = false;
};
static std::mutex registrationsMutex;
static std::mutex installationMutex;
static std::unordered_map<std::string, Registration> registrations;
static Binding installed{};
static Finish originalFinish = nullptr;
static VoxelUndo originalVoxelUndo = nullptr;
static std::mutex playbackMutex;
static std::unordered_map<void*, std::size_t> cancellingHistories;

struct CancellationScope {
    void* history;
    explicit CancellationScope(void* current) : history(current) {
        if (history) { std::lock_guard lock(playbackMutex); ++cancellingHistories[history]; }
    }
    ~CancellationScope() {
        if (history) {
            std::lock_guard lock(playbackMutex);
            const auto found = cancellingHistories.find(history);
            if (--found->second == 0) cancellingHistories.erase(found);
        }
    }
};

static void Prune() {
    std::vector<Registration> expired;
    {
        std::lock_guard lock(registrationsMutex);
        for (auto it = registrations.begin(); it != registrations.end();) {
            if (!it->second.modelOwner->Alive()) {
                expired.push_back(std::move(it->second));
                it = registrations.erase(it);
            } else ++it;
        }
    }
    // Releasing an engine object can run callbacks; never do it under our lock.
}

static Binding GetBinding() {
    Prune();
    std::lock_guard lock(installationMutex);
    return originalFinish ? installed : Binding{};
}

static void UndoVoxels(void* entry, bool backwards) {
    void* history = nullptr;
    std::memcpy(&history, entry, sizeof(history));
    {
        std::lock_guard lock(playbackMutex);
        if (cancellingHistories.contains(history)) return;
    }
    originalVoxelUndo(entry, backwards);
}

static void FinishRecording(void* history, std::string token, std::uint32_t operation, Options options) {
    std::optional<Registration> registration;
    {
        std::lock_guard lock(registrationsMutex);
        const auto found = registrations.find(token);
        if (found != registrations.end() && found->second.history == history && found->second.modelOwner->Alive())
            registration = found->second;
    }
    // Restore before Cancel: callbacks from inverse property/parent writes then
    // remain the newest voxel edits. Failed restoration leaves the token alive.
    if (registration && operation == 0 && registration->rollbackTerrain)
        registration->rollbackTerrain();
    if (registration && operation != 0 && registration->terrainWritten && registration->checkTerrain)
        registration->checkTerrain();
    CancellationScope scope(registration && operation == 0 ? history : nullptr);
    originalFinish(history, token, operation, std::move(options));
    if (registration) {
        std::lock_guard lock(registrationsMutex);
        registrations.erase(token);
    }
}

// Installation runs on the DataModel thread. Pause other threads only for the
// code replacement, after allocation, and restore protection before resuming.
class PausedThreads {
#if defined(_WIN32)
    std::vector<HANDLE> threads;
#else
    thread_act_array_t threads = nullptr;
    mach_msg_type_number_t count = 0;
    thread_t current = pthread_mach_thread_np(pthread_self());
#endif
    std::size_t paused = 0;
    bool inspectable = true;
public:
    PausedThreads() {
#if defined(_WIN32)
        const auto snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if (snapshot == INVALID_HANDLE_VALUE) throw std::runtime_error("Cannot inspect Studio threads for history installation");
        THREADENTRY32 entry{};
        entry.dwSize = sizeof(entry);
        if (Thread32First(snapshot, &entry)) do {
            if (entry.th32OwnerProcessID == GetCurrentProcessId() && entry.th32ThreadID != GetCurrentThreadId()) {
                const auto thread = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, FALSE, entry.th32ThreadID);
                if (thread) threads.push_back(thread);
                else if (GetLastError() != ERROR_INVALID_PARAMETER) inspectable = false;
            }
        } while (Thread32Next(snapshot, &entry));
        CloseHandle(snapshot);
#else
        if (task_threads(mach_task_self(), &threads, &count) != KERN_SUCCESS)
            throw std::runtime_error("Cannot inspect Studio threads for history installation");
#endif
    }
    bool Pause(std::uintptr_t address, std::size_t size) {
        if (!inspectable) return false;
#if defined(_WIN32)
        for (; paused < threads.size(); ++paused)
            if (SuspendThread(threads[paused]) == static_cast<DWORD>(-1)) return false;
#else
        for (; paused < count; ++paused)
            if (threads[paused] != current && thread_suspend(threads[paused]) != KERN_SUCCESS) return false;
#endif
        for (std::size_t i = 0; i < paused; ++i) {
            std::uintptr_t pc = 0;
#if defined(_WIN32)
            CONTEXT context{};
            context.ContextFlags = CONTEXT_CONTROL;
            if (!GetThreadContext(threads[i], &context)) return false;
            pc = context.Rip;
#elif defined(__aarch64__)
            if (threads[i] == current) continue;
            arm_thread_state64_t context{};
            mach_msg_type_number_t words = ARM_THREAD_STATE64_COUNT;
            if (thread_get_state(threads[i], ARM_THREAD_STATE64, reinterpret_cast<thread_state_t>(&context), &words) != KERN_SUCCESS) return false;
            pc = arm_thread_state64_get_pc(context);
#else
            if (threads[i] == current) continue;
            x86_thread_state64_t context{};
            mach_msg_type_number_t words = x86_THREAD_STATE64_COUNT;
            if (thread_get_state(threads[i], x86_THREAD_STATE64, reinterpret_cast<thread_state_t>(&context), &words) != KERN_SUCCESS) return false;
            pc = context.__rip;
#endif
            if (pc >= address && pc - address < size) return false;
        }
        return true;
    }
    void Resume() {
        while (paused) {
            --paused;
#if defined(_WIN32)
            ResumeThread(threads[paused]);
#else
            if (threads[paused] != current) thread_resume(threads[paused]);
#endif
        }
    }
    ~PausedThreads() {
        Resume();
#if defined(_WIN32)
        for (auto thread : threads) CloseHandle(thread);
#else
        for (mach_msg_type_number_t i = 0; i < count; ++i) mach_port_deallocate(mach_task_self(), threads[i]);
        if (threads) vm_deallocate(mach_task_self(), reinterpret_cast<vm_address_t>(threads), count * sizeof(thread_t));
#endif
    }
};

static std::vector<unsigned char> Jump(std::uintptr_t destination) {
#if defined(__aarch64__) || defined(_M_ARM64)
    // ldr x16, [pc, #8]; br x16; .quad destination
    std::vector<unsigned char> bytes(16);
    const std::uint32_t words[] = {0x58000050, 0xd61f0200};
    std::memcpy(bytes.data(), words, sizeof(words));
    std::memcpy(bytes.data() + 8, &destination, sizeof(destination));
#else
    // jmp qword ptr [rip]; .quad destination. Preserve every argument register.
    std::vector<unsigned char> bytes(14);
    bytes[0] = 0xff; bytes[1] = 0x25;
    std::memcpy(bytes.data() + 6, &destination, sizeof(destination));
#endif
    return bytes;
}

static void* Trampoline(const Binding& binding) {
    auto bytes = std::vector<unsigned char>(binding.prefix, binding.prefix + binding.prefixSize);
    const auto jump = Jump(binding.voxelUndo + binding.prefixSize);
    bytes.insert(bytes.end(), jump.begin(), jump.end());
#if defined(_WIN32)
    void* memory = VirtualAlloc(nullptr, bytes.size(), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!memory) throw std::runtime_error("Cannot allocate Studio history trampoline");
    std::memcpy(memory, bytes.data(), bytes.size());
    DWORD previous = 0;
    if (!VirtualProtect(memory, bytes.size(), PAGE_EXECUTE_READ, &previous)) {
        VirtualFree(memory, 0, MEM_RELEASE);
        throw std::runtime_error("Cannot protect Studio history trampoline");
    }
    FlushInstructionCache(GetCurrentProcess(), memory, bytes.size());
#else
    ReniumSignedCode signedCode(bytes);
    void* memory = signedCode.Map(nullptr);
    if (memory == MAP_FAILED) throw std::runtime_error("Cannot allocate Studio history trampoline");
    sys_icache_invalidate(memory, bytes.size());
#endif
    return memory;
}

static void ReplaceCode(std::uintptr_t address, const std::vector<unsigned char>& bytes) {
    PausedThreads threads;
#if defined(_WIN32)
    const auto destination = reinterpret_cast<void*>(address);
    DWORD previous = 0, ignored = 0;
    bool wrote = false, restored = false;
    if (threads.Pause(address, bytes.size()) && VirtualProtect(destination, bytes.size(), PAGE_EXECUTE_READWRITE, &previous)) {
        std::memcpy(destination, bytes.data(), bytes.size());
        wrote = true;
        restored = VirtualProtect(destination, bytes.size(), previous, &ignored) != 0;
        FlushInstructionCache(GetCurrentProcess(), destination, bytes.size());
    }
#else
    const auto pageSize = static_cast<std::uintptr_t>(sysconf(_SC_PAGESIZE));
    const auto page = address & ~(pageSize - 1);
    const auto size = ((address + bytes.size() + pageSize - 1) & ~(pageSize - 1)) - page;
    std::vector<unsigned char> replacement(size);
    std::memcpy(replacement.data(), reinterpret_cast<const void*>(page), size);
    std::memcpy(replacement.data() + address - page, bytes.data(), bytes.size());
    ReniumSignedCode signedCode(replacement);
    bool wrote = false;
    if (threads.Pause(address, bytes.size())) {
        wrote = signedCode.Map(reinterpret_cast<void*>(page)) != MAP_FAILED;
        if (wrote) sys_icache_invalidate(reinterpret_cast<void*>(page), size);
    }
    const bool restored = wrote;
    threads.Resume();
#endif
    threads.Resume();
    if (!wrote || !restored) throw std::runtime_error("Cannot install Studio Terrain history playback hook");
}

template<class Read>
static void Register(const Binding& binding, void* history, std::uintptr_t model, void* modelOwner, std::string token, Read read) {
    Prune();
    std::lock_guard installationLock(installationMutex);
    if (token.empty() || token.size() > 256 || binding.memberOffset < 64 || binding.memberOffset >= 256 ||
        binding.memberOffset % 8 || binding.prefixSize < Jump(0).size() || binding.prefixSize > sizeof(binding.prefix))
        throw std::runtime_error("Invalid Studio history binding");
    if (!originalFinish) {
        std::uintptr_t value = 0;
        std::array<unsigned char, 32> prefix{};
        if (!read(binding.descriptor, &value, sizeof(value)) || value != binding.table ||
            !read(binding.descriptor + binding.memberOffset, &value, sizeof(value)) || value != binding.finish ||
#if !defined(_WIN32)
            !read(binding.descriptor + binding.memberOffset + 8, &value, sizeof(value)) || value != 0 ||
#endif
            !read(binding.voxelUndo, prefix.data(), binding.prefixSize) ||
            std::memcmp(prefix.data(), binding.prefix, binding.prefixSize) != 0)
            throw std::runtime_error("Studio history code changed before installation");
        auto jump = Jump(reinterpret_cast<std::uintptr_t>(&UndoVoxels));
        // The resolver supplies whole, position-independent prologue instructions.
        jump.resize(binding.prefixSize, 0);
        originalVoxelUndo = reinterpret_cast<VoxelUndo>(Trampoline(binding));
        installed = binding;
        ReplaceCode(binding.voxelUndo, jump);
        originalFinish = reinterpret_cast<Finish>(binding.finish);
        std::atomic_ref<std::uintptr_t>(*reinterpret_cast<std::uintptr_t*>(binding.descriptor + binding.memberOffset))
            .store(reinterpret_cast<std::uintptr_t>(&FinishRecording), std::memory_order_release);
    } else if (binding.descriptor != installed.descriptor || binding.voxelUndo != installed.voxelUndo) {
        throw std::runtime_error("Studio history registration targets another engine binding");
    }
    std::lock_guard lock(registrationsMutex);
    const auto result = registrations.emplace(std::move(token), Registration{history, model, std::make_shared<WeakOwner>(modelOwner), {}, {}, false});
    if (!result.second && result.first->second.history != history)
        throw std::runtime_error("Studio history token is already registered");
}
}
