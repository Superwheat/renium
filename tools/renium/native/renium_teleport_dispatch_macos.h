#pragma once
#include "renium_playtest_macos.h"

namespace renium_mac_teleport {
using namespace renium_mac_playtest;
using Instance = std::shared_ptr<void>;
struct Tuple { std::vector<Variant> values; };
using Values = std::shared_ptr<const Tuple>;
using Players = std::shared_ptr<const std::vector<Instance>>;
using Success = std::function<void(Instance)>;
using Failure = std::function<void(std::string)>;
using Teleport = void(*)(void*, std::int64_t, Players, Instance, Success, Failure);
using Invoke = void(*)(void*, Values, std::function<void(Values)>, Failure);
using Execute = void(*)(void*, Variant, std::function<void(Variant)>, Failure);

struct Request {
    std::uint32_t version, size, action, receipt;
    std::uint64_t token, slot, original, invoke, getAttribute;
    std::uint64_t instanceType, stringTypeTable, tableType, resultFactory, resultCreate;
    std::uint64_t parentOffset, childrenOffset, classOffset;
    std::uint32_t hostPid, lifetimeMs;
    std::uint64_t hostSeconds, hostMicros, attempt, destination;
    char runtime[64];
    std::uint64_t sourceModel, sourceOwner, sourceTest, getArgs;
    std::uint64_t executeSlot, executeOriginal, invokeSlot, selfOffset;
    std::uint64_t placeField, universeField, place, universe;
    char nonce[128];
};
static_assert(sizeof(Request) == 448);

struct JoinRecord {
    std::uint64_t id;
    std::int64_t place;
    Variant join, data;
    JoinRecord(std::uint64_t id, std::int64_t place, const Variant& join, const Variant& data)
        : id(id), place(place), join(join), data(data) {}
};

inline bool HostAlive(const Request& r) {
    LoadRequest host{};
    host.hostPid = r.hostPid; host.hostSeconds = r.hostSeconds; host.hostMicros = r.hostMicros;
    return renium_mac_playtest::HostAlive(host);
}

struct Pending {
    Instance service;
    void* model = nullptr;
    std::int64_t place = 0;
    Players players;
    Instance options;
    Success success;
    Failure failure;
};
struct Lease {
    Request request{};
    bool active = true;
    Clock::time_point deadline;
    std::shared_ptr<JoinRecord> arrival;
    std::shared_ptr<Variant> arguments;
    std::shared_ptr<renium_history::WeakOwner> root;
    std::vector<std::shared_ptr<Pending>> pending;
    bool Alive() const { return active && Clock::now() < deadline && HostAlive(request); }
};
inline std::mutex mutex;
inline std::condition_variable changed;
inline std::shared_ptr<Lease> current;
inline std::atomic<Teleport> original{nullptr};
inline std::atomic<Invoke> invokeOriginal{nullptr};
inline std::atomic<Execute> executeOriginal{nullptr};

inline bool Class(void* instance, std::uint64_t offset, const char* expected) {
    const auto descriptor = Word(instance, offset);
    const auto name = Word(reinterpret_cast<void*>(descriptor), 8);
    return *reinterpret_cast<const std::string*>(name) == expected;
}

inline const std::vector<Instance>* Children(void* instance, std::uint64_t offset, std::size_t maximum) {
    auto children = reinterpret_cast<const std::vector<Instance>*>(Word(instance, offset));
    if (children && children->size() > maximum) throw std::runtime_error("Teleport ownership hierarchy exceeds its bound");
    return children;
}

inline Variant Attribute(void* instance, const Request& r, const char* key) {
    return reinterpret_cast<Variant(*)(void*, StringView)>(r.getAttribute)(instance, {key, std::strlen(key)});
}

inline bool Marker(void* instance, const Request& r, const char* key, const char* expected) {
    const auto marker = Attribute(instance, r, key);
    return marker.type && Word(reinterpret_cast<void*>(marker.type), 0) == r.stringTypeTable
        && *reinterpret_cast<const std::string*>(marker.payload) == expected;
}

inline void* Model(void* instance, const Request& r) {
    for (unsigned depth = 0; instance && depth < 4; ++depth) {
        if (Class(instance, r.classOffset, "DataModel")) return instance;
        instance = reinterpret_cast<void*>(Word(instance, r.parentOffset));
    }
    return nullptr;
}

inline void* Owns(const std::shared_ptr<Lease>& lease, void* service, int client = 0) {
    Request r;
    { std::lock_guard lock(mutex); r = lease->request; }
    auto model = Model(service, r);
    if (!model || model == reinterpret_cast<void*>(r.sourceModel)
        || Word(model, r.placeField) != r.place || Word(model, r.universeField) != r.universe
        || !lease->root || !lease->root->Alive()) return nullptr;
    auto roots = Children(model, r.childrenOffset, 256);
    if (!roots) return nullptr;
    void* test = nullptr;
    bool networkClient = false;
    for (const auto& root : *roots) {
        if (!root) continue;
        if (Class(root.get(), r.classOffset, "NetworkClient")) networkClient = true;
        if (Class(root.get(), r.classOffset, "StudioTestService")) test = root.get();
    }
    if (!test || (client >= 0 && networkClient != static_cast<bool>(client))) return nullptr;
    std::shared_ptr<Variant> arguments;
    { std::lock_guard lock(mutex); arguments = lease->arguments; }
    if (!arguments)
        return Marker(reinterpret_cast<void*>(r.sourceModel), r, "__ReniumLaunchNonce", r.nonce)
            && Marker(model, r, "__ReniumLaunchNonce", r.nonce) ? model : nullptr;
    const auto actual = reinterpret_cast<Variant(*)(void*)>(r.getArgs)(test);
    return actual.type == r.tableType && arguments->type == r.tableType
        && Word(actual.payload, 0) == Word(arguments->payload, 0) ? model : nullptr;
}

inline Instance Relay(void* service, const Request& r) {
    auto model = reinterpret_cast<void*>(Word(service, r.parentOffset));
    if (!model || !Class(model, r.classOffset, "DataModel")) return {};
    const auto roots = Children(model, r.childrenOffset, 256);
    if (!roots) return {};
    Instance result;
    for (const auto& root : *roots) {
        if (!root || !Class(root.get(), r.classOffset, "ServerStorage")) continue;
        const auto children = Children(root.get(), r.childrenOffset, 65536);
        if (!children) continue;
        for (const auto& child : *children) {
            if (!child || !Class(child.get(), r.classOffset, "BindableFunction")) continue;
            const auto marker = Attribute(child.get(), r, "__ReniumNative");
            if (marker.type && Word(reinterpret_cast<void*>(marker.type), 0) == r.stringTypeTable
                && *reinterpret_cast<const std::string*>(marker.payload) == (r.nonce[0] ? r.nonce : r.runtime)) {
                if (result) throw std::runtime_error("Teleport runtime has duplicate native relays");
                result = child;
            }
        }
    }
    return result;
}

inline void CopyInstance(const void* source, void* output) { new(output) Instance(*static_cast<const Instance*>(source)); }
inline void MoveInstance(void* source, void* output) { new(output) Instance(std::move(*static_cast<Instance*>(source))); }
inline void DestroyInstance(void* value) { static_cast<Instance*>(value)->~shared_ptr(); }
struct Operations {
    void(*copy)(const void*, void*);
    void(*move)(void*, void*);
    void(*destroy)(void*);
};
inline const Operations instanceOperations{CopyInstance, MoveInstance, DestroyInstance};

inline void AppendInstance(Tuple& tuple, const Instance& value, std::uint64_t type) {
    auto& item = tuple.values.emplace_back();
    item.type = type;
    new(item.payload) Instance(value);
    item.operations = reinterpret_cast<std::uintptr_t>(&instanceOperations);
}

inline Instance Result(const Request& r) {
    auto factory = reinterpret_cast<void*>(r.resultFactory);
    if (Word(reinterpret_cast<void*>(Word(factory, 0)), 0) != r.resultCreate)
        throw std::runtime_error("Studio's native teleport result creator changed");
    // Apple ARM64 returns this nontrivial shared_ptr through the compiler's x8
    // result register. Do not copy the Windows explicit return-buffer ABI here.
    auto result = reinterpret_cast<Instance(*)(void*, void*, bool)>(r.resultCreate)(factory, nullptr, true);
    if (!result || !Class(result.get(), r.classOffset, "TeleportAsyncResult"))
        throw std::runtime_error("Studio did not create a native teleport result");
    return result;
}

inline void InvokeRelay(const std::shared_ptr<Lease>& lease, Instance relay, std::int64_t place,
    Players players, Instance options, Success success, Failure failure) {
    if (!players || players->empty() || players->size() > 8)
        throw std::runtime_error("Teleport requires 1 through 8 connected players");
    auto result = options ? Result(lease->request) : Instance{};
    auto tuple = std::make_shared<Tuple>();
    tuple->values.reserve(players->size() + 2);
    tuple->values.emplace_back(Attribute(relay.get(), lease->request, "__ReniumNative"));
    auto& destination = tuple->values.back();
    if (!destination.operations || !destination.type
        || Word(reinterpret_cast<void*>(destination.type), 0) != lease->request.stringTypeTable)
        throw std::runtime_error("Teleport relay has no string transport template");
    *reinterpret_cast<std::string*>(destination.payload) = std::to_string(place);
    AppendInstance(*tuple, options, lease->request.instanceType);
    for (const auto& player : *players) AppendInstance(*tuple, player, lease->request.instanceType);
    auto accepted = [lease, place, result = std::move(result), success = std::move(success), failure](Values values) {
        std::shared_ptr<JoinRecord> record;
        try {
            if (values && values->values.size() == 1 && values->values[0].type
                && Word(reinterpret_cast<void*>(values->values[0].type), 0) == lease->request.stringTypeTable)
                throw std::runtime_error(*reinterpret_cast<const std::string*>(values->values[0].payload));
            if (!values || values->values.size() != 3 || values->values[0].type != lease->request.tableType
                || !values->values[2].type || Word(reinterpret_cast<void*>(values->values[2].type), 0) != lease->request.stringTypeTable)
                throw std::runtime_error("Native teleport arrival data has an incompatible value type");
            const auto& id = *reinterpret_cast<const std::string*>(values->values[2].payload);
            std::size_t used = 0;
            const auto sequence = std::stoull(id, &used);
            if (used != id.size() || !sequence || sequence > 9007199254740991ull)
                throw std::runtime_error("Native teleport arrival sequence is invalid");
            record = std::make_shared<JoinRecord>(sequence, place, values->values[0], values->values[1]);
            std::lock_guard lock(mutex);
            if (current != lease || !lease->Alive())
                throw std::runtime_error("Native teleport ended before acceptance");
            lease->arrival = std::move(record);
        } catch (const std::exception& error) { failure(error.what()); return; }
        success(result);
    };
    reinterpret_cast<Invoke>(lease->request.invoke)(relay.get(), std::move(tuple), std::move(accepted), failure);
}

inline void Entry(void* service, std::int64_t place, Players players, Instance options, Success success, Failure failure) {
    std::shared_ptr<Lease> lease;
    Request binding{};
    { std::lock_guard lock(mutex); if (current && current->Alive()) { lease = current; binding = lease->request; } }
    Instance relay;
    try {
        if (lease && binding.nonce[0]) {
            if (auto model = Owns(lease, service)) {
                relay = Relay(service, binding);
                if (!relay) {
                    auto held = reinterpret_cast<const std::weak_ptr<void>*>(static_cast<char*>(service) + binding.selfOffset)->lock();
                    if (held.get() != service) throw std::runtime_error("Teleport service ended before dispatch");
                    auto pending = std::make_shared<Pending>();
                    pending->service = std::move(held); pending->model = model; pending->place = place;
                    pending->players = std::move(players); pending->options = std::move(options);
                    pending->success = std::move(success); pending->failure = failure;
                    std::lock_guard lock(mutex);
                    if (lease->pending.size() >= 8) throw std::runtime_error("The Studio teleport request queue is full");
                    lease->pending.push_back(std::move(pending));
                    return;
                }
            }
        } else if (lease) relay = Relay(service, binding);
    }
    catch (const std::exception& error) { failure(error.what()); return; }
    if (!relay) {
        original.load(std::memory_order_acquire)(service, place, std::move(players), std::move(options), std::move(success), std::move(failure));
        return;
    }
    // Error continuations may throw while the original Luau dispatch is active.
    // InvokeRelay's call is outside a catch which would replay that continuation.
    InvokeRelay(lease, std::move(relay), place, std::move(players), std::move(options), std::move(success), std::move(failure));
}

inline void ExecuteEntry(void* receiver, Variant input, std::function<void(Variant)> success, Failure failure) {
    std::shared_ptr<Lease> lease;
    Request binding{};
    { std::lock_guard lock(mutex); if (current && current->Alive()) { lease = current; binding = lease->request; } }
    if (lease && receiver == reinterpret_cast<void*>(binding.sourceTest) && input.type == binding.tableType
        && Marker(reinterpret_cast<void*>(binding.sourceModel), binding, "__ReniumLaunchNonce", binding.nonce)) {
        auto captured = std::make_shared<Variant>(input);
        std::lock_guard lock(mutex);
        if (current == lease && lease->Alive() && std::strcmp(binding.nonce, lease->request.nonce) == 0)
            lease->arguments = std::move(captured);
    }
    executeOriginal.load(std::memory_order_acquire)(receiver, std::move(input), std::move(success), std::move(failure));
}

inline void InvokeEntry(void* receiver, Values args, std::function<void(Values)> success, Failure failure) {
    std::shared_ptr<Lease> lease;
    Request binding{};
    { std::lock_guard lock(mutex); if (current && !current->pending.empty()) { lease = current; binding = lease->request; } }
    void* model = nullptr;
    if (lease && binding.nonce[0] && Marker(receiver, binding, "__ReniumNativePump", binding.nonce))
        model = Owns(lease, receiver);
    // This call originated in the owning plugin's coroutine, so queued work
    // resumes on the engine's actual model scheduler without a guessed context.
    invokeOriginal.load(std::memory_order_acquire)(receiver, std::move(args), std::move(success), std::move(failure));
    if (!model) return;
    std::vector<std::shared_ptr<Pending>> pending;
    bool alive = false;
    {
        std::lock_guard lock(mutex);
        alive = current == lease && lease->Alive();
        for (auto it = lease->pending.begin(); it != lease->pending.end();) {
            if ((*it)->model == model) { pending.push_back(std::move(*it)); it = lease->pending.erase(it); }
            else ++it;
        }
    }
    for (const auto& request : pending) {
        if (!alive) { request->failure("The owned teleport launch ended"); continue; }
        auto relay = Relay(request->service.get(), binding);
        if (!relay) { request->failure("The owned teleport relay ended"); continue; }
        InvokeRelay(lease, std::move(relay), request->place, request->players, request->options, request->success, request->failure);
    }
}

inline void Restore(const std::shared_ptr<Lease>& lease) {
    if (current != lease) return;
    lease->active = false;
    const auto& r = lease->request;
    if (__atomic_load_n(reinterpret_cast<void**>(r.slot), __ATOMIC_ACQUIRE) == reinterpret_cast<void*>(&Entry)
        && !Exchange(r.slot, reinterpret_cast<void*>(&Entry), reinterpret_cast<void*>(r.original)))
        throw std::runtime_error("Teleport dispatch changed during cleanup");
    if (r.executeSlot && __atomic_load_n(reinterpret_cast<void**>(r.executeSlot), __ATOMIC_ACQUIRE) == reinterpret_cast<void*>(&ExecuteEntry))
        Exchange(r.executeSlot, reinterpret_cast<void*>(&ExecuteEntry), reinterpret_cast<void*>(r.executeOriginal));
    if (r.invokeSlot && __atomic_load_n(reinterpret_cast<void**>(r.invokeSlot), __ATOMIC_ACQUIRE) == reinterpret_cast<void*>(&InvokeEntry))
        Exchange(r.invokeSlot, reinterpret_cast<void*>(&InvokeEntry), reinterpret_cast<void*>(r.invoke));
    lease->pending.clear();
    changed.notify_all();
}

inline void Handle(const std::string& payload, std::vector<unsigned char>& output) {
    Request r{};
    if (payload.size() != sizeof(r)) throw std::runtime_error("Truncated Mac teleport dispatch request");
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 2 || r.size != sizeof(r) || !r.token || r.action < 1 || r.action > 5)
        throw std::runtime_error("Incompatible Mac teleport dispatch request");
    std::lock_guard lock(mutex);
    if (r.action == 1) {
        if (!r.slot || !r.original || !r.invoke || !r.getAttribute || !r.instanceType || !r.stringTypeTable
            || !r.tableType || !r.resultFactory || !r.resultCreate || (!r.runtime[0] && !r.nonce[0]) || r.runtime[63] || r.nonce[127]
            || r.parentOffset > 65536 || r.childrenOffset > 65536 || r.classOffset > 65536
            || r.parentOffset % 8 || r.childrenOffset % 8 || r.classOffset % 8
            || !r.lifetimeMs || r.lifetimeMs > 180000 || !HostAlive(r))
            throw std::runtime_error("Invalid owned Mac teleport dispatch binding");
        if (current && current->Alive()) throw std::runtime_error("The teleport dispatch already has an owner");
        if (current) Restore(current);
        if (original.load() && original.load() != reinterpret_cast<Teleport>(r.original))
            throw std::runtime_error("The teleport dispatch changed in this process");
        auto lease = std::make_shared<Lease>();
        lease->request = r;
        lease->deadline = Clock::now() + std::chrono::milliseconds(r.lifetimeMs);
        if (r.nonce[0]) {
            if (!r.sourceModel || !r.sourceOwner || !r.sourceTest || !r.getArgs || !r.executeSlot || !r.executeOriginal
                || !r.invokeSlot || r.selfOffset > 65536 || r.selfOffset % 8 || !r.place || !r.universe
                || r.placeField > 65536 || r.universeField > 65536 || r.placeField % 8 || r.universeField % 8
                || Word(reinterpret_cast<void*>(r.sourceModel), r.selfOffset + 8) != r.sourceOwner)
                throw std::runtime_error("Incomplete owned Mac teleport launch");
            lease->root = std::make_shared<renium_history::WeakOwner>(reinterpret_cast<void*>(r.sourceOwner));
        }
        original.store(reinterpret_cast<Teleport>(r.original), std::memory_order_release);
        if (!Exchange(r.slot, reinterpret_cast<void*>(r.original), reinterpret_cast<void*>(&Entry)))
            throw std::runtime_error("Another owner changed the teleport dispatch");
        current = lease;
        try {
            if (r.nonce[0]) {
                invokeOriginal.store(reinterpret_cast<Invoke>(r.invoke), std::memory_order_release);
                executeOriginal.store(reinterpret_cast<Execute>(r.executeOriginal), std::memory_order_release);
                if (!Exchange(r.invokeSlot, reinterpret_cast<void*>(r.invoke), reinterpret_cast<void*>(&InvokeEntry))
                    || !Exchange(r.executeSlot, reinterpret_cast<void*>(r.executeOriginal), reinterpret_cast<void*>(&ExecuteEntry)))
                    throw std::runtime_error("Another owner changed the teleport launch bindings");
            }
        } catch (...) { Restore(lease); throw; }
        try { std::thread([lease] {
            std::unique_lock lock(mutex);
            while (current == lease && lease->active) {
                if (!lease->Alive()) { Restore(lease); break; }
                changed.wait_for(lock, std::chrono::milliseconds(100));
            }
        }).detach(); } catch (...) { Restore(lease); throw; }
    } else {
        if (!current || current->request.token != r.token)
            throw std::runtime_error("The Mac teleport dispatch lease was replaced");
        if (r.action == 2) Restore(current);
        else {
            if (!current->Alive()) throw std::runtime_error("The Mac teleport dispatch lease expired");
            if (r.action == 3) current->deadline = Clock::now() + std::chrono::milliseconds(current->request.lifetimeMs);
            else if (r.action == 4) r.receipt = current->arrival && current->arrival->id == r.attempt
                && current->arrival->place == static_cast<std::int64_t>(r.destination);
            else {
                if (!current->request.nonce[0] || !r.nonce[0] || r.nonce[127] || !r.place || !current->pending.empty())
                    throw std::runtime_error("The native teleport launch cannot advance while a caller is pending");
                current->request.place = r.place;
                std::memcpy(current->request.nonce, r.nonce, sizeof(r.nonce));
                current->arguments.reset();
            }
        }
    }
    output.resize(sizeof(r));
    std::memcpy(output.data(), &r, sizeof(r));
}
}
