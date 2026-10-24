#pragma once
#include "renium_teleport_gui_windows.h"

// Separate Studio processes cannot share Reflection::Variant allocations.
// Each runtime materializes its own values through the privileged relay.
namespace renium_test_join {
using namespace renium_play_teleport;
struct Request {
    std::uint32_t version, size, action, client;
    std::uint64_t token;
    renium_play_owner::Binding model;
    std::uint64_t relay, relayOwner, invoke;
    std::uint64_t joinSlot, joinOriginal, joinField, localSlot, localOriginal, localField;
    std::uint64_t place, universe, hostCreation;
    std::uint32_t hostPid, reserved;
    std::int64_t userId;
    std::uint64_t identityCalls[3], identityWriters[3];
    char name[64], displayName[128];
    char error[256];
};
static_assert(sizeof(Request) == 760);
using Join = int(*)(void*, void*);
using Local = void*(*)(void*, void*);
struct Lease {
    Request request{};
    Values values;
    std::vector<Instance> existing;
    void* selected = nullptr;
    void* local = nullptr;
    HANDLE host = nullptr;
    unsigned identityInstalled = 0;
    std::uint64_t deadline = GetTickCount64() + 60000;
    ~Lease() { if (host) CloseHandle(host); }
    bool Alive() const { return GetTickCount64() < deadline && WaitForSingleObject(host, 0) == WAIT_TIMEOUT; }
};
inline std::mutex mutex;
inline std::shared_ptr<Lease> current;
inline std::atomic<Join> originalJoin{nullptr};
inline std::atomic<Local> originalLocal{nullptr};
// Roblox's JSON writer takes non-owning views. Preserve its allocator and
// serialization, replacing only the three identity fields in the local join.
struct TextView { const char* data; std::size_t size; };
using WriteNumber = void(*)(void*, const TextView*, std::int64_t);
using WriteText = void(*)(void*, const TextView*, const TextView*);
inline std::atomic<WriteNumber> originalNumber{nullptr};
inline std::atomic<WriteText> originalText{nullptr};
// Reuse these process-lifetime jump islands. A watchdog may restore a call
// while its thread is crossing the island; its executable bytes must survive.
inline void* identityThunks[3]{};
inline std::uint64_t identitySites[3]{};
inline bool Key(const TextView* key, const char* text) {
    return key && key->size == std::strlen(text) && std::memcmp(key->data, text, key->size) == 0;
}
inline void IdentityNumber(void* writer, const TextView* key, std::int64_t value) {
    std::shared_ptr<Lease> lease;
    { std::lock_guard lock(mutex); lease = current; }
    if (lease && lease->request.client && lease->Alive() && Key(key, "UserId")) value = lease->request.userId;
    originalNumber.load()(writer, key, value);
}
inline void IdentityText(void* writer, const TextView* key, const TextView* value) {
    std::shared_ptr<Lease> lease;
    { std::lock_guard lock(mutex); lease = current; }
    TextView replacement{};
    if (lease && lease->request.client && lease->Alive()) {
        if (Key(key, "UserName")) replacement = { lease->request.name, std::strlen(lease->request.name) };
        else if (Key(key, "DisplayName")) replacement = { lease->request.displayName, std::strlen(lease->request.displayName) };
        if (replacement.data) value = &replacement;
    }
    originalText.load()(writer, key, value);
}
inline bool Owns(const Lease& lease, void* instance) {
    const auto& r = lease.request;
    const auto model = renium_play_owner::Model(instance, r.model);
    return model && (r.client ? model != reinterpret_cast<void*>(r.model.sourceModel)
        : model == reinterpret_cast<void*>(r.model.sourceModel))
        && renium_play_loader::Word(model, r.model.placeField) == r.place
        && renium_play_loader::Word(model, r.model.universeField) == r.universe;
}
inline int GetJoin(void* player, void* lua) {
    {
        std::lock_guard lock(mutex);
        const auto lease = current;
        if (lease && lease->Alive() && Owns(*lease, player)
            && (!lease->selected || lease->selected == player)
            && std::none_of(lease->existing.begin(), lease->existing.end(), [player](const auto& item) { return item.get() == player; })) {
            lease->selected = player;
            auto& field = *reinterpret_cast<std::shared_ptr<const void>*>(static_cast<char*>(player) + lease->request.joinField);
            field = *reinterpret_cast<const std::shared_ptr<const void>*>(lease->values->values[0].payload);
        }
    }
    return originalJoin.load()(player, lua);
}
inline void* GetLocal(void* service, void* output) {
    {
        std::lock_guard lock(mutex);
        const auto lease = current;
        if (lease && lease->request.client && lease->Alive() && Owns(*lease, service) && lease->local != service) {
            auto field = static_cast<char*>(service) + lease->request.localField;
            const auto operations = renium_play_loader::Word(field, 8);
            if (operations) reinterpret_cast<void(**)(void*)>(operations)[2](field + 16);
            const auto& value = lease->values->values[1];
            if (value.operations) reinterpret_cast<void(**)(const void*, void*)>(value.operations)[0](value.payload, field + 16);
            std::memcpy(field, &value.type, 8);
            std::memcpy(field + 8, &value.operations, 8);
            lease->local = service;
        }
    }
    return originalLocal.load()(service, output);
}
inline void Restore(const std::shared_ptr<Lease>& lease) {
    if (current != lease) return;
    const auto& r = lease->request;
    renium_play_loader::Exchange(r.joinSlot, reinterpret_cast<void*>(&GetJoin), reinterpret_cast<void*>(r.joinOriginal));
    if (r.client) renium_play_loader::Exchange(r.localSlot, reinterpret_cast<void*>(&GetLocal), reinterpret_cast<void*>(r.localOriginal));
    while (lease->identityInstalled) {
        const auto i = --lease->identityInstalled;
        try { renium_test_client::Patches::Call(r.identityCalls[i], reinterpret_cast<std::uintptr_t>(identityThunks[i]), r.identityWriters[i]); }
        catch (...) {} // A replacement owner must not be overwritten; the island remains callable.
    }
    current.reset();
}
inline void Handle(Request& r) {
    if (r.version != 2 || r.size != sizeof(r) || !r.token || r.reserved || r.client > 1)
        throw std::runtime_error("Incompatible routed arrival binding");
    if (r.action == 2) {
        std::lock_guard lock(mutex);
        if (current && current->request.token == r.token) Restore(current);
        return;
    }
    if (r.action != 1 || !r.relay || !r.relayOwner || !r.invoke || !r.joinSlot || !r.joinOriginal
        || !r.localSlot || !r.localOriginal || r.joinField >= 65536 || r.localField >= 65536)
        throw std::runtime_error("Incomplete routed arrival binding");
    auto lease = std::make_shared<Lease>();
    if (r.client && (r.userId >= 0 || !r.name[0] || r.name[63] || !r.displayName[0] || r.displayName[127]
        || !r.identityCalls[0] || !r.identityCalls[1] || !r.identityCalls[2]
        || !r.identityWriters[0] || !r.identityWriters[1] || r.identityWriters[1] != r.identityWriters[2]))
        throw std::runtime_error("Invalid routed test-player identity");
    lease->request = r;
    lease->host = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, r.hostPid);
    FILETIME created{}, exited{}, kernel{}, user{};
    if (!lease->host || !GetProcessTimes(lease->host, &created, &exited, &kernel, &user)
        || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != r.hostCreation)
        throw std::runtime_error("Routed arrival host ended");
    auto task = std::make_shared<renium_teleport_gui::Task>();
    task->request.model = r.model;
    auto model = reinterpret_cast<void*>(r.model.sourceModel);
    task->modelHold = renium_play_owner::Hold(model, r.model);
    if (renium_play_owner::Model(reinterpret_cast<void*>(r.relay), r.model) != model
        || !Class(reinterpret_cast<void*>(r.relay), r.model.classOffset, "BindableFunction")
        || !AddOwnerReference(reinterpret_cast<void*>(r.relayOwner)))
        throw std::runtime_error("Routed arrival relay changed ownership");
    task->relayHold = std::shared_ptr<void>(reinterpret_cast<void*>(r.relayOwner), ReleaseOwnerReference);
    if (!renium_play_owner::Submit(model, r.model, [lease, task, model] {
        if (!task->active.load()) { task->Finish(); return; }
        try {
            const auto& r = lease->request;
            if (!r.client) {
                const auto roots = Children(model, r.model.childrenOffset, 256);
                for (const auto& root : *roots) if (Class(root.get(), r.model.classOffset, "Players")) {
                    if (auto players = Children(root.get(), r.model.childrenOffset, 256))
                        for (const auto& player : *players) if (Class(player.get(), r.model.classOffset, "Player"))
                            lease->existing.push_back(player);
                }
            }
            reinterpret_cast<Invoke>(r.invoke)(reinterpret_cast<void*>(r.relay), std::make_shared<Tuple>(),
                [lease, task](Values values) {
                    if (!task->active.load()) { task->Finish(); return; }
                    if (!values || values->values.size() != 2 || values->values[0].type != lease->request.model.tableType) {
                        task->Finish("Routed arrival values changed"); return;
                    }
                    lease->values = values;
                    task->Finish();
                }, [task](std::string error) { task->Finish(error); });
        } catch (const std::exception& error) { task->Finish(error.what()); }
    })) throw std::runtime_error("Routed arrival queue ended");
    const auto waited = WaitForSingleObject(task->finished, 3000);
    task->active.store(false);
    if (waited != WAIT_OBJECT_0) throw std::runtime_error("Routed arrival data timed out");
    if (!task->error.empty()) throw std::runtime_error(task->error);
    task.reset();
    {
        std::lock_guard lock(mutex);
        if (current) throw std::runtime_error("Another client is joining this server");
        originalJoin.store(reinterpret_cast<Join>(r.joinOriginal));
        originalLocal.store(reinterpret_cast<Local>(r.localOriginal));
        current = lease;
        try {
            if (r.client) {
                originalNumber.store(reinterpret_cast<WriteNumber>(r.identityWriters[0]));
                originalText.store(reinterpret_cast<WriteText>(r.identityWriters[1]));
                for (unsigned i = 0; i < 3; ++i) {
                    if (identityThunks[i] && identitySites[i] != r.identityCalls[i]) throw std::runtime_error("Local-test identity writer moved");
                    if (!identityThunks[i]) {
                        identityThunks[i] = renium_test_client::Patches::Island(&r.identityCalls[i], 1,
                            i == 0 ? reinterpret_cast<std::uintptr_t>(&IdentityNumber) : reinterpret_cast<std::uintptr_t>(&IdentityText));
                        identitySites[i] = r.identityCalls[i];
                    }
                    renium_test_client::Patches::Call(r.identityCalls[i], r.identityWriters[i], reinterpret_cast<std::uintptr_t>(identityThunks[i]));
                    ++lease->identityInstalled;
                }
            }
            if (!renium_play_loader::Exchange(r.joinSlot, reinterpret_cast<void*>(r.joinOriginal), reinterpret_cast<void*>(&GetJoin)))
                throw std::runtime_error("Routed player getter changed");
            if (r.client && !renium_play_loader::Exchange(r.localSlot, reinterpret_cast<void*>(r.localOriginal), reinterpret_cast<void*>(&GetLocal)))
                throw std::runtime_error("Routed local getter changed");
        } catch (...) { Restore(lease); throw; }
    }
    std::thread([lease] {
        while (WaitForSingleObject(lease->host, 100) == WAIT_TIMEOUT) {
            std::lock_guard lock(mutex);
            if (current != lease) return;
            if (!lease->Alive()) break;
        }
        std::lock_guard lock(mutex);
        Restore(lease);
    }).detach();
}
}
extern "C" __declspec(dllexport) DWORD WINAPI ReniumTestJoin(void* input) {
    auto& request = *static_cast<renium_test_join::Request*>(input);
    try { renium_test_join::Handle(request); return 0; }
    catch (const std::exception& error) { strncpy_s(request.error, error.what(), _TRUNCATE); return 1; }
}
