#pragma once
#include <algorithm>
#include <functional>
#include <vector>
#include "renium_play_loader_windows.h"
#include "renium_play_owner_windows.h"

namespace renium_play_teleport {
using Variant = renium_play_loader::VariantBuffer;
using Instance = std::shared_ptr<void>;
struct Tuple { std::vector<Variant> values; };
using Values = std::shared_ptr<const Tuple>;
using Players = std::shared_ptr<const std::vector<Instance>>;
using Success = std::function<void(Instance)>;
using Failure = std::function<void(std::string)>;
using Teleport = void(*)(void*, std::int64_t, Players, Instance, Success, Failure);
using Invoke = void(*)(void*, Values, std::function<void(Values)>, Failure);

struct Request {
    std::uint32_t version, size, action, reserved;
    std::uint64_t token, slot, original, parentOffset, childrenOffset, classOffset, unused;
    std::uint64_t invoke, getAttribute, instanceType, stringTypeTable, hostCreation;
    std::uint32_t hostPid, lifetimeMs;
    char error[256];
    char runtime[64];
    std::uint64_t tableType;
    std::uint64_t resultFactory, resultCreate;
    renium_play_owner::Binding launch;
    std::uint64_t executeSlot, executeOriginal, launchPlace, launchUniverse;
    char launchNonce[128];
};
static_assert(sizeof(Request) == 752);

struct JoinRecord {
    std::uint64_t id;
    std::int64_t place;
    Variant join, data;
    JoinRecord(std::uint64_t id, std::int64_t place, const Variant& join, const Variant& data)
        : id(id), place(place), join(join), data(data) {}
};

struct Pending;
struct Lease {
    Request request{};
    HANDLE host = nullptr;
    std::atomic<bool> active{true};
    std::atomic<std::uint64_t> deadline{0};
    std::shared_ptr<JoinRecord> arrival;
    std::atomic<std::shared_ptr<renium_play_owner::Scope>> scope;
    std::mutex requestsMutex;
    std::vector<std::shared_ptr<Pending>> requests;
    ~Lease() { if (host) CloseHandle(host); }
    bool Alive() const {
        return active.load() && GetTickCount64() < deadline.load()
            && host && WaitForSingleObject(host, 0) == WAIT_TIMEOUT;
    }
};
inline std::mutex mutex;
inline std::shared_ptr<Lease> current;
inline std::atomic<Teleport> original{nullptr};
inline std::atomic<renium_play_owner::Execute> executeOriginal{nullptr};

inline void ExecuteEntry(void* receiver, Variant input, std::function<void(Variant)> success, Failure failure) {
    std::shared_ptr<Lease> lease;
    { std::lock_guard lock(mutex); if (current && current->Alive()) lease = current; }
    if (lease) renium_play_owner::Capture(lease->scope.load(), receiver, input);
    executeOriginal.load()(receiver, std::move(input), std::move(success), std::move(failure));
}

inline std::shared_ptr<renium_play_owner::Scope> NewScope(const Request& r) {
    if (!r.launchNonce[0] || r.launchNonce[127] || !r.launchPlace || !r.launchUniverse || !r.executeSlot || !r.executeOriginal
        || !r.launch.sourceModel || !r.launch.sourceTest || !r.launch.getArgs || !r.launch.submitTask
        || !r.launch.contextMask || !r.launch.contextTable || r.launch.contextField > 65536
        || r.launch.parentOffset != r.parentOffset || r.launch.childrenOffset != r.childrenOffset
        || r.launch.classOffset != r.classOffset || r.launch.selfOffset > 512 || r.launch.selfOffset % 8
        || r.launch.placeField > 65536 || r.launch.placeField % 8
        || r.launch.universeField > 65536 || r.launch.universeField % 8
        || r.launch.tableType != r.tableType || r.launch.getAttribute != r.getAttribute
        || r.launch.stringTypeTable != r.stringTypeTable)
        throw std::runtime_error("Incomplete native teleport launch scope");
    auto scope = std::make_shared<renium_play_owner::Scope>();
    scope->binding = r.launch; scope->nonce = r.launchNonce; scope->place = r.launchPlace; scope->universe = r.launchUniverse;
    return scope;
}

inline void CopyInstance(const void* source, void* output) { new(output) Instance(*static_cast<const Instance*>(source)); }
inline void MoveInstance(void* source, void* output) { new(output) Instance(std::move(*static_cast<Instance*>(source))); }
inline void DestroyInstance(void* value) { static_cast<Instance*>(value)->~shared_ptr(); }
struct Operations {
    void(*copy)(const void*, void*);
    void(*move)(void*, void*);
    void(*destroy)(void*);
};
inline const Operations instanceOperations{&CopyInstance, &MoveInstance, &DestroyInstance};

inline bool Class(void* instance, std::uint64_t offset, const char* expected) {
    const auto descriptor = renium_play_loader::Word(instance, offset);
    const auto name = renium_play_loader::Word(reinterpret_cast<void*>(descriptor), 8);
    return *reinterpret_cast<const std::string*>(name) == expected;
}

inline const std::vector<Instance>* Children(void* instance, std::uint64_t offset, std::size_t maximum) {
    auto children = reinterpret_cast<const std::vector<Instance>*>(renium_play_loader::Word(instance, offset));
    if (children && children->size() > maximum) throw std::runtime_error("Teleport ownership hierarchy exceeds its bound");
    return children;
}

inline Instance Relay(void* service, const Request& r, const renium_play_owner::Scope* scope = nullptr) {
    // These are the same structurally derived Instance offsets used by the
    // snapshot reader. Resolve from the actual dispatch receiver, including
    // the hidden solo server which is absent from Studio's selected-model list.
    auto model = reinterpret_cast<void*>(renium_play_loader::Word(service, r.parentOffset));
    if (!model || !Class(model, r.classOffset, "DataModel")) return {};
    auto roots = Children(model, r.childrenOffset, 256);
    if (!roots) return {};
    Instance result;
    for (const auto& root : *roots) {
        if (!root || !Class(root.get(), r.classOffset, "ServerStorage")) continue;
        auto children = Children(root.get(), r.childrenOffset, 65536);
        if (!children) continue;
        for (const auto& child : *children) {
            if (!child || !Class(child.get(), r.classOffset, "BindableFunction")) continue;
            Variant marker;
            using Attribute = void*(*)(void*, void*, renium_play_loader::StringView);
            constexpr char key[] = "__ReniumNative";
            reinterpret_cast<Attribute>(r.getAttribute)(child.get(), &marker, {key, sizeof(key) - 1});
            if (marker.type && renium_play_loader::Word(reinterpret_cast<void*>(marker.type), 0) == r.stringTypeTable
                && *reinterpret_cast<const std::string*>(marker.payload) == (scope ? scope->nonce : std::string(r.runtime))) {
                if (result) throw std::runtime_error("Teleport runtime has duplicate native relays");
                result = child;
            }
        }
    }
    return result;
}

inline void AppendInstance(Tuple& tuple, const Instance& value, std::uint64_t type) {
    auto& result = tuple.values.emplace_back();
    result.type = type;
    new(result.payload) Instance(value);
    result.operations = reinterpret_cast<std::uintptr_t>(&instanceOperations);
}

inline Instance ResultInstance(const Request& r) {
    // Use the same engine class factory and creation ABI as the qualified
    // binary reader. A local ordinary-server result has empty reservation
    // fields; no production reservation, ticket or job identity is invented.
    const auto factory = reinterpret_cast<void*>(r.resultFactory);
    using Create = Instance*(*)(void*, Instance*, void*, bool);
    const auto create = reinterpret_cast<Create>(r.resultCreate);
    if (**static_cast<void***>(factory) != reinterpret_cast<void*>(create))
        throw std::runtime_error("Studio's native teleport result creator changed");
    Instance result;
    if (!create || create(factory, &result, nullptr, true) != &result || !result
        || !Class(result.get(), r.classOffset, "TeleportAsyncResult"))
        throw std::runtime_error("Studio did not create a native teleport result");
    return result;
}

inline void InvokeRelay(const std::shared_ptr<Lease>& lease, Instance relay, std::int64_t place,
    Players players, Instance options, Success success, Failure failure) {
    if (!players || players->empty() || players->size() > 8) throw std::runtime_error("Teleport requires 1 through 8 connected players");
    auto result = options ? ResultInstance(lease->request) : Instance{};
    auto tuple = std::make_shared<Tuple>();
    tuple->values.reserve(players->size() + 2);
    // Use an engine-created string Variant as the transport template.
    // The plugin created this attribute on this private server relay.
    auto& destination = tuple->values.emplace_back();
    using Attribute = void*(*)(void*, void*, renium_play_loader::StringView);
    constexpr char key[] = "__ReniumNative";
    reinterpret_cast<Attribute>(lease->request.getAttribute)(relay.get(), &destination, {key, sizeof(key) - 1});
    if (!destination.operations || !destination.type
        || renium_play_loader::Word(reinterpret_cast<void*>(destination.type), 0) != lease->request.stringTypeTable)
        throw std::runtime_error("Teleport relay has no string transport template");
    *reinterpret_cast<std::string*>(destination.payload) = std::to_string(place);
    AppendInstance(*tuple, options, lease->request.instanceType);
    for (const auto& player : *players) AppendInstance(*tuple, player, lease->request.instanceType);
    // BindableFunction preserves the caller's async engine lifecycle. Its
    // plugin handler validates live players/options and waits for host
    // acceptance before allowing the original TeleportAsync to complete.
    std::function<void(Values)> accepted{[lease, scope = lease->scope.load(), place, result = std::move(result), success = std::move(success), error = failure](Values values) {
        try {
            // OnInvoke exceptions can leave native callers suspended. The
            // private relay returns failures as one engine-owned string.
            if (values && values->values.size() == 1 && values->values[0].type
                && renium_play_loader::Word(reinterpret_cast<void*>(values->values[0].type), 0) == lease->request.stringTypeTable)
                throw std::runtime_error(*reinterpret_cast<const std::string*>(values->values[0].payload));
            if (!values || values->values.size() != 3 || values->values[0].type != lease->request.tableType
                || !values->values[2].type || renium_play_loader::Word(reinterpret_cast<void*>(values->values[2].type), 0) != lease->request.stringTypeTable)
                throw std::runtime_error("Native teleport arrival data has an incompatible value type");
            const auto& id = *reinterpret_cast<const std::string*>(values->values[2].payload);
            std::size_t used = 0;
            const auto sequence = std::stoull(id, &used);
            if (used != id.size() || !sequence || sequence > 9007199254740991ull)
                throw std::runtime_error("Native teleport arrival sequence is invalid");
            auto record = std::make_shared<JoinRecord>(sequence, place, values->values[0], values->values[1]);
            {
                std::lock_guard lock(mutex);
                if (current != lease || !lease->Alive() || lease->scope.load() != scope)
                    throw std::runtime_error("Native teleport ended before acceptance");
                lease->arrival = std::move(record);
            }
            success(result);
        } catch (const std::exception& message) { error(message.what()); }
    }};
    reinterpret_cast<Invoke>(lease->request.invoke)(relay.get(), std::move(tuple), std::move(accepted), failure);
}

struct Pending {
    std::weak_ptr<Lease> lease;
    std::shared_ptr<renium_play_owner::Scope> scope;
    std::shared_ptr<void> hold;
    void* service = nullptr;
    void* model = nullptr;
    std::int64_t place = 0;
    Players players;
    Instance options;
    Success success;
    Failure failure;
    std::uint64_t deadline = 0;
    std::atomic<bool> queued{false};
    std::atomic<unsigned> state{0}; // waiting, invoked, completed
    void Fail(const std::string& error) { if (state.exchange(2) != 2 && failure) failure(error); }
};

inline void Poll(const std::shared_ptr<Pending>& request) {
    const auto lease = request->lease.lock();
    if (!lease || !lease->Alive() || lease->scope.load() != request->scope || !request->scope->active.load()) {
        request->Fail("The owned teleport launch ended"); return;
    }
    if (GetTickCount64() >= request->deadline) { request->Fail("The teleport server did not become ready"); return; }
    if (request->state.load() != 0) return;
    try {
        auto relay = Relay(request->service, lease->request, request->scope.get());
        if (!relay) return;
        unsigned expected = 0;
        if (!request->state.compare_exchange_strong(expected, 1)) return;
        InvokeRelay(lease, std::move(relay), request->place, request->players, request->options,
            [request](Instance result) { if (request->state.exchange(2) != 2) request->success(std::move(result)); },
            [request](std::string error) { request->Fail(error); });
    } catch (const std::exception& error) { request->Fail(error.what()); }
}

inline void Pump(const std::shared_ptr<Lease>& lease) {
    std::vector<std::shared_ptr<Pending>> requests;
    {
        std::lock_guard lock(lease->requestsMutex);
        std::erase_if(lease->requests, [](const auto& request) { return request->state.load() == 2; });
        requests = lease->requests;
    }
    for (const auto& request : requests) {
        if (!lease->Alive() || GetTickCount64() >= request->deadline || !request->scope->active.load()) {
            request->Fail("The owned teleport launch ended or timed out"); continue;
        }
        if (request->state.load() != 0 || request->queued.exchange(true)) continue;
        try {
            // The owning reference stays outside the DataModel's own queue.
            // A cancelled queued closure therefore cannot keep a stopped model alive.
            const std::weak_ptr<Pending> weak = request;
            if (!renium_play_owner::Submit(request->model, request->scope->binding, [weak] {
                if (auto pending = weak.lock()) { Poll(pending); pending->queued.store(false); }
            })) throw std::runtime_error("Studio rejected the teleport dispatch task");
        } catch (const std::exception& error) { request->queued.store(false); request->Fail(error.what()); }
    }
}

inline void Entry(void* service, std::int64_t place, Players players, Instance options, Success success, Failure failure) {
    std::shared_ptr<Lease> lease;
    { std::lock_guard lock(mutex); if (current && current->Alive()) lease = current; }
    if (lease) {
        try {
            const auto scope = lease->scope.load();
            if (scope) {
                auto model = renium_play_owner::Owns(*scope, service);
                if (model) {
                    // TeleportAsync is server-only. An owned client still follows
                    // the engine's normal validation, without entering this queue.
                    bool client = false;
                    if (auto roots = Children(model, scope->binding.childrenOffset, 256))
                        for (const auto& root : *roots) if (Class(root.get(), scope->binding.classOffset, "NetworkClient")) client = true;
                    if (!client) {
                        auto request = std::make_shared<Pending>();
                        request->lease = lease; request->scope = scope;
                        request->hold = renium_play_owner::Hold(model, scope->binding);
                        request->model = model; request->service = service; request->place = place;
                        request->players = players; request->options = options;
                        request->success = success; request->failure = failure;
                        request->deadline = GetTickCount64() + 180000;
                        {
                            std::lock_guard lock(lease->requestsMutex);
                            if (std::count_if(lease->requests.begin(), lease->requests.end(), [](const auto& pending) { return pending->state.load() != 2; }) >= 8)
                                throw std::runtime_error("The Studio teleport request queue is full");
                            lease->requests.push_back(request);
                        }
                        Poll(request);
                        return;
                    }
                }
            } else if (auto relay = Relay(service, lease->request)) {
                // Keep the original error continuation available to the outer
                // dispatch catch: synchronous engine failures throw through it.
                InvokeRelay(lease, std::move(relay), place, std::move(players), std::move(options), std::move(success), failure);
                return;
            }
        } catch (const std::exception& error) { if (failure) failure(error.what()); return; }
    }
    original.load()(service, place, std::move(players), std::move(options), std::move(success), std::move(failure));
}

inline void Restore(const std::shared_ptr<Lease>& lease) {
    if (current != lease || !lease->active.exchange(false)) return;
    if (auto scope = lease->scope.load()) scope->active.store(false);
    const auto slot = reinterpret_cast<void**>(lease->request.slot);
    if (*slot == reinterpret_cast<void*>(&Entry))
        renium_play_loader::Exchange(lease->request.slot, reinterpret_cast<void*>(&Entry), reinterpret_cast<void*>(lease->request.original));
    if (lease->request.executeSlot && *reinterpret_cast<void**>(lease->request.executeSlot) == reinterpret_cast<void*>(&ExecuteEntry))
        renium_play_loader::Exchange(lease->request.executeSlot, reinterpret_cast<void*>(&ExecuteEntry), reinterpret_cast<void*>(lease->request.executeOriginal));
}

inline void Handle(Request& r) {
    if (r.version != 6 || r.size != sizeof(r) || r.reserved || !r.token)
        throw std::runtime_error("Incompatible teleport dispatch protocol; restart Studio with this Renium build");
    std::lock_guard lock(mutex);
    if (r.action == 1) {
        if (!r.slot || !r.original || r.parentOffset > 512 || r.childrenOffset > 512 || r.classOffset > 512
            || r.parentOffset % 8 || r.childrenOffset % 8 || r.classOffset % 8 || r.unused || (!r.runtime[0] && !r.launchNonce[0]) || r.runtime[63]
            || !r.invoke || !r.getAttribute || !r.instanceType || !r.stringTypeTable || !r.tableType
            || !r.resultFactory || !r.resultCreate || r.lifetimeMs < 1000 || r.lifetimeMs > 180000)
            throw std::runtime_error("Incomplete owned teleport dispatch binding");
        if (current && current->Alive()) throw std::runtime_error("Teleport dispatch already has an owner");
        if (current) Restore(current);
        const auto bound = original.load();
        if (bound && bound != reinterpret_cast<Teleport>(r.original)) throw std::runtime_error("Teleport dispatch binding changed");
        auto lease = std::make_shared<Lease>();
        lease->request = r;
        if (r.launchNonce[0]) {
            lease->scope.store(NewScope(r));
        }
        lease->host = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, r.hostPid);
        FILETIME created{}, exited{}, kernel{}, user{};
        if (!lease->host || !GetProcessTimes(lease->host, &created, &exited, &kernel, &user)
            || (static_cast<std::uint64_t>(created.dwHighDateTime) << 32 | created.dwLowDateTime) != r.hostCreation)
            throw std::runtime_error("Teleport dispatch host process changed");
        lease->deadline.store(GetTickCount64() + r.lifetimeMs);
        original.store(reinterpret_cast<Teleport>(r.original));
        current = lease;
        try {
            if (!renium_play_loader::Exchange(r.slot, reinterpret_cast<void*>(r.original), reinterpret_cast<void*>(&Entry)))
                throw std::runtime_error("Teleport dispatch was replaced before arming");
            if (lease->scope.load()) {
                executeOriginal.store(reinterpret_cast<renium_play_owner::Execute>(r.executeOriginal));
                if (!renium_play_loader::Exchange(r.executeSlot, reinterpret_cast<void*>(r.executeOriginal), reinterpret_cast<void*>(&ExecuteEntry)))
                    throw std::runtime_error("Play launch binding was replaced before arming");
            }
            std::thread([lease] {
                while (WaitForSingleObject(lease->host, 100) == WAIT_TIMEOUT && lease->Alive()) Pump(lease);
                { std::lock_guard lock(mutex); Restore(lease); }
                Pump(lease);
            }).detach();
        } catch (...) { Restore(lease); throw; }
    } else if (r.action == 5) {
        if (!current || current->request.token != r.token || !current->Alive())
            throw std::runtime_error("Native launch scope ended before transition");
        if (!r.launchNonce[0] || r.launchNonce[127]) throw std::runtime_error("Invalid native destination nonce");
        auto next = current->request;
        if (!current->scope.load()) {
            next.launch = r.launch; next.executeSlot = r.executeSlot; next.executeOriginal = r.executeOriginal;
            next.launchUniverse = r.launchUniverse;
        } else if (r.launchUniverse != next.launchUniverse) throw std::runtime_error("Native destination universe changed");
        next.launchPlace = r.launchPlace; strcpy_s(next.launchNonce, r.launchNonce);
        auto scope = NewScope(next);
        if (!current->scope.load()) {
            executeOriginal.store(reinterpret_cast<renium_play_owner::Execute>(next.executeOriginal));
            if (!renium_play_loader::Exchange(next.executeSlot, reinterpret_cast<void*>(next.executeOriginal), reinterpret_cast<void*>(&ExecuteEntry)))
                throw std::runtime_error("Play launch binding changed during reconnection");
            current->request.launch = next.launch;
            current->request.executeSlot = next.executeSlot;
            current->request.executeOriginal = next.executeOriginal;
            current->request.launchUniverse = next.launchUniverse;
        }
        if (auto old = current->scope.exchange(std::move(scope))) old->active.store(false);
    } else if (r.action == 4) {
        r.reserved = current && current->request.token == r.token && current->Alive() && current->arrival
            && current->arrival->id == r.slot && current->arrival->place == static_cast<std::int64_t>(r.original);
    } else if (r.action == 2 || r.action == 3) {
        if (!current || current->request.token != r.token) {
            if (r.action == 3) throw std::runtime_error("Owned teleport dispatch was replaced");
            return;
        }
        if (r.action == 2) Restore(current);
        else if (current->Alive()) {
            if (*reinterpret_cast<void**>(current->request.slot) != reinterpret_cast<void*>(&Entry)
                || (current->request.executeSlot && *reinterpret_cast<void**>(current->request.executeSlot) != reinterpret_cast<void*>(&ExecuteEntry)))
                throw std::runtime_error("Owned teleport dispatch is no longer installed");
            current->deadline.store(GetTickCount64() + current->request.lifetimeMs);
        }
        else throw std::runtime_error("Owned teleport dispatch expired");
    } else throw std::runtime_error("Unknown teleport dispatch operation");
}

inline std::shared_ptr<renium_play_owner::Scope> LaunchScope(std::uint64_t token, const char* nonce) {
    std::lock_guard lock(mutex);
    if (!current || current->request.token != token || !current->Alive()) throw std::runtime_error("The native launch owner ended");
    auto scope = current->scope.load();
    if (scope && scope->nonce != nonce) throw std::runtime_error("The native arrival launch was replaced");
    return scope;
}

inline std::shared_ptr<JoinRecord> Arrival(std::uint64_t token, std::uint64_t id, std::int64_t place) {
    std::lock_guard lock(mutex);
    if (!current || current->request.token != token || !current->Alive() || !current->arrival
        || current->arrival->id != id || current->arrival->place != place)
        throw std::runtime_error("The owned native teleport has no matching accepted arrival data");
    return current->arrival;
}
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumPlayTeleport(void* input) {
    auto& request = *static_cast<renium_play_teleport::Request*>(input);
    try { renium_play_teleport::Handle(request); return 0; }
    catch (const std::exception& error) { strncpy_s(request.error, error.what(), _TRUNCATE); return 1; }
}
