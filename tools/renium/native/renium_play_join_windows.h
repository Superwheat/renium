#pragma once
#include "renium_play_teleport_windows.h"

namespace renium_play_join {
using Variant = renium_play_loader::VariantBuffer;
using Execute = void(*)(void*, Variant, std::function<void(Variant)>, std::function<void(std::string)>);
using Join = int(*)(void*, void*);
using Local = void*(*)(void*, void*);
struct Request {
    std::uint32_t version, size, action, reserved;
    std::uint64_t token, dispatchToken, attempt, place, universe, sourceModel, sourceTest;
    std::uint64_t executeSlot, executeOriginal, getArgs, joinSlot, joinOriginal, joinField;
    std::uint64_t localSlot, localOriginal, localField, parentOffset, childrenOffset, classOffset;
    std::uint64_t placeField, universeField, tableType, getAttribute, stringTypeTable, hostCreation;
    std::uint32_t hostPid, lifetimeMs;
    char nonce[128], error[256];
};
static_assert(sizeof(Request) == 608);
struct Lease {
    Request request{};
    std::shared_ptr<renium_play_teleport::JoinRecord> arrival;
    std::shared_ptr<renium_play_owner::Scope> scope;
    std::atomic<std::shared_ptr<Variant>> arguments;
    std::atomic<bool> active{true};
    mutable std::atomic<std::uint32_t> receipt{0};
    HANDLE host = nullptr;
    std::uint64_t deadline = 0;
    ~Lease() { if (host) CloseHandle(host); }
    bool Alive() const { return active.load() && GetTickCount64() < deadline
        && host && WaitForSingleObject(host, 0) == WAIT_TIMEOUT; }
};
inline std::mutex mutex;
inline std::shared_ptr<Lease> current;
inline std::atomic<Execute> executeOriginal{nullptr};
inline std::atomic<Join> joinOriginal{nullptr};
inline std::atomic<Local> localOriginal{nullptr};

inline std::shared_ptr<Lease> Active() {
    std::lock_guard lock(mutex);
    return current && current->Alive() ? current : nullptr;
}

template<class S, class F> inline void ExecuteCallback(void* receiver, Variant input, S success, F failure) {
    auto lease = Active();
    if (lease) lease->receipt.fetch_or(1);
    if (lease && receiver == reinterpret_cast<void*>(lease->request.sourceTest) && input.type == lease->request.tableType) {
        lease->receipt.fetch_or(2);
        const auto& r = lease->request;
        renium_play_loader::Request launch{};
        launch.getAttribute = r.getAttribute; launch.sourceInstance = r.sourceModel; launch.stringTypeTable = r.stringTypeTable;
        strcpy_s(launch.launchNonce, r.nonce);
        if (renium_play_loader::MatchesLaunch(launch)) {
            lease->receipt.fetch_or(4);
            lease->arguments.store(std::make_shared<Variant>(input));
        }
    }
    reinterpret_cast<void(*)(void*, Variant, S, F)>(executeOriginal.load())(receiver, std::move(input), std::move(success), std::move(failure));
}

inline void ExecuteEntry(void* receiver, Variant input, std::function<void(Variant)> success, std::function<void(std::string)> failure) {
    ExecuteCallback(receiver, std::move(input), std::move(success), std::move(failure));
}
inline void TaggedExecuteEntry(void* receiver, Variant input, renium_continuation::Function<void(Variant)> success, renium_continuation::Function<void(std::string)> failure) {
    ExecuteCallback(receiver, std::move(input), std::move(success), std::move(failure));
}
inline void* ExecuteHook() { return renium_continuation::Tagged() ? reinterpret_cast<void*>(&TaggedExecuteEntry) : reinterpret_cast<void*>(&ExecuteEntry); }

inline void* Model(void* instance, const Request& r) {
    for (unsigned depth = 0; instance && depth < 4; ++depth) {
        if (renium_play_teleport::Class(instance, r.classOffset, "DataModel")) return instance;
        instance = reinterpret_cast<void*>(renium_play_loader::Word(instance, r.parentOffset));
    }
    return nullptr;
}

inline bool Owns(const Lease& lease, void* instance, bool client) {
    if (lease.scope) return renium_play_owner::Owns(*lease.scope, instance, client) != nullptr;
    const auto arguments = lease.arguments.load();
    if (!arguments) return false;
    lease.receipt.fetch_or(8);
    const auto& r = lease.request;
    auto model = Model(instance, r);
    if (!model || model == reinterpret_cast<void*>(r.sourceModel)
        || renium_play_loader::Word(model, r.placeField) != r.place
        || renium_play_loader::Word(model, r.universeField) != r.universe) return false;
    lease.receipt.fetch_or(16);
    auto children = renium_play_teleport::Children(model, r.childrenOffset, 256);
    if (!children) return false;
    void* test = nullptr;
    bool networkClient = false;
    for (const auto& child : *children) {
        if (renium_play_teleport::Class(child.get(), r.classOffset, "StudioTestService")) test = child.get();
        if (renium_play_teleport::Class(child.get(), r.classOffset, "NetworkClient")) networkClient = true;
    }
    if (!test || (client && !networkClient)) return false;
    lease.receipt.fetch_or(32);
    Variant actual;
    reinterpret_cast<Local>(r.getArgs)(test, &actual);
    // Play copies this immutable native argument table through its controller.
    // Its retained object identity fences the new server and client before any
    // game callback, without matching a place name or trusting a mutable field.
    if (actual.type != r.tableType || arguments->type != r.tableType) return false;
    lease.receipt.fetch_or(64);
    const bool same = renium_play_loader::Word(actual.payload, 0) == renium_play_loader::Word(arguments->payload, 0);
    if (same) lease.receipt.fetch_or(128);
    return same;
}

inline int JoinEntry(void* player, void* lua) {
    auto lease = Active();
    if (lease) lease->receipt.fetch_or(256);
    if (lease && Owns(*lease, player, false)) {
        auto& value = *reinterpret_cast<std::shared_ptr<const void>*>(static_cast<char*>(player) + lease->request.joinField);
        if (value) lease->receipt.fetch_or(512);
        value = *reinterpret_cast<const std::shared_ptr<const void>*>(lease->arrival->join.payload);
        lease->receipt.fetch_or(1024);
    }
    return joinOriginal.load()(player, lua);
}

inline void* LocalEntry(void* service, void* output) {
    auto lease = Active();
    if (lease) lease->receipt.fetch_or(2048);
    if (lease && Owns(*lease, service, true)) {
        auto field = static_cast<char*>(service) + lease->request.localField;
        // Retain any data already initialized by the engine. Publish type and
        // manager only after the engine's payload copy succeeds.
        if (!renium_play_loader::Word(field, 8)) {
            const auto& incoming = lease->arrival->data;
            if (incoming.operations) {
                using Copy = void(*)(const void*, void*);
                reinterpret_cast<Copy*>(incoming.operations)[0](incoming.payload, field + 16);
            }
            std::memcpy(field, &incoming.type, 8);
            std::memcpy(field + 8, &incoming.operations, 8);
            lease->receipt.fetch_or(4096);
        }
    }
    return localOriginal.load()(service, output);
}

inline void Restore(const std::shared_ptr<Lease>& lease) {
    if (current != lease) return;
    lease->active.store(false);
    const auto& r = lease->request;
    const std::uint64_t slots[]{r.executeSlot, r.joinSlot, r.localSlot};
    const std::uint64_t originals[]{r.executeOriginal, r.joinOriginal, r.localOriginal};
    void* wrappers[]{ExecuteHook(), reinterpret_cast<void*>(&JoinEntry), reinterpret_cast<void*>(&LocalEntry)};
    for (unsigned i = 0; i < 3; ++i) {
        if (slots[i] && *reinterpret_cast<void**>(slots[i]) == wrappers[i])
            renium_play_loader::Exchange(slots[i], wrappers[i], reinterpret_cast<void*>(originals[i]));
    }
}

inline void Handle(Request& r) {
    if (r.version != 2 || r.size != sizeof(r) || r.reserved || !r.token)
        throw std::runtime_error("Incompatible native teleport arrival protocol");
    if (r.action == 1) {
        if (!r.dispatchToken || !r.attempt || !r.place || !r.universe || !r.sourceModel || !r.sourceTest
            || !r.getArgs || !r.joinSlot || !r.joinOriginal
            || !r.localSlot || !r.localOriginal || !r.tableType || !r.getAttribute || !r.stringTypeTable
            || r.joinField > 65536 || r.localField > 65536 || r.parentOffset > 512 || r.childrenOffset > 512 || r.classOffset > 512
            || r.placeField > 65536 || r.universeField > 65536 || !r.nonce[0] || r.nonce[127]
            || r.lifetimeMs < 1 || r.lifetimeMs > 90000)
            throw std::runtime_error("Incomplete native teleport arrival binding");
        auto lease = std::make_shared<Lease>();
        lease->request = r;
        lease->arrival = renium_play_teleport::Arrival(r.dispatchToken, r.attempt, r.place);
        lease->scope = renium_play_teleport::LaunchScope(r.dispatchToken, r.nonce);
        if (lease->scope) {
            if (r.executeSlot || r.executeOriginal || lease->scope->place != r.place || lease->scope->universe != r.universe
                || lease->scope->binding.sourceModel != r.sourceModel || lease->scope->binding.sourceTest != r.sourceTest)
                throw std::runtime_error("Native arrival does not match its retained launch scope");
        } else if (!r.executeSlot || !r.executeOriginal) throw std::runtime_error("Native arrival has no launch binding");
        if (lease->arrival->join.type != r.tableType || !renium_play_loader::Word(lease->arrival->join.payload, 0))
            throw std::runtime_error("Native arrival has no engine-owned join table");
        lease->host = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, r.hostPid);
        FILETIME created{}, exited{}, kernel{}, user{};
        if (!lease->host || !GetProcessTimes(lease->host, &created, &exited, &kernel, &user)
            || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != r.hostCreation)
            throw std::runtime_error("Native arrival host process changed");
        lease->deadline = GetTickCount64() + r.lifetimeMs;
        std::lock_guard lock(mutex);
        if (current && current->Alive()) throw std::runtime_error("Native arrival already has an owner");
        if (current) Restore(current);
        if (r.executeOriginal) executeOriginal.store(reinterpret_cast<Execute>(r.executeOriginal));
        joinOriginal.store(reinterpret_cast<Join>(r.joinOriginal));
        localOriginal.store(reinterpret_cast<Local>(r.localOriginal));
        current = lease;
        try {
            const std::uint64_t slots[]{r.executeSlot, r.joinSlot, r.localSlot};
            const std::uint64_t originals[]{r.executeOriginal, r.joinOriginal, r.localOriginal};
            void* wrappers[]{ExecuteHook(), reinterpret_cast<void*>(&JoinEntry), reinterpret_cast<void*>(&LocalEntry)};
            for (unsigned i = 0; i < 3; ++i)
                if (slots[i] && !renium_play_loader::Exchange(slots[i], reinterpret_cast<void*>(originals[i]), wrappers[i]))
                    throw std::runtime_error("Native arrival binding changed before arming");
            std::thread([lease] {
                while (WaitForSingleObject(lease->host, 100) == WAIT_TIMEOUT && lease->Alive()) {}
                std::lock_guard lock(mutex);
                Restore(lease);
            }).detach();
        } catch (...) { Restore(lease); throw; }
    } else if (r.action == 2) {
        std::lock_guard lock(mutex);
        if (current && current->request.token == r.token) Restore(current);
    } else if (r.action == 3) {
        std::lock_guard lock(mutex);
        if (current && current->request.token == r.token) r.reserved = current->receipt.load();
    } else throw std::runtime_error("Unknown native teleport arrival operation");
}
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumPlayJoin(void* input) {
    auto& request = *static_cast<renium_play_join::Request*>(input);
    try { renium_play_join::Handle(request); return 0; }
    catch (const std::exception& error) { strncpy_s(request.error, error.what(), _TRUNCATE); return 1; }
    catch (...) { strcpy_s(request.error, "Unknown native entry-point failure"); return 1; }
}
