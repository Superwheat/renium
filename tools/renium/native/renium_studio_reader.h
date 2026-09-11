// MSVC interop for the native binary reader. Address discovery, payload planning
// and transaction authorization belong to Rust; this scope owns engine objects
// and restores its factory interception before returning to the caller.
#include <algorithm>
#include <intrin.h>
#include <memory_resource>
#include <unordered_map>
#include <unordered_set>

namespace renium_reader {
using Create = SharedInstance*(__fastcall*)(void*, SharedInstance*, void*, bool);
using Load = void(__fastcall*)(void*, std::istream*, void*, unsigned, void*, void*);

struct Binding
{
    SharedInstance target;
    std::uint32_t classIndex, ordinal;
};
struct Alias { std::uint32_t classIndex, ordinal, sourceOrdinal; };
struct Class
{
    std::string name;
    std::uint32_t count;
    bool script;
};
struct Created
{
    SharedInstance target;
    std::uint32_t classIndex, ordinal;
};
struct CreatedWire
{
    std::uint32_t classIndex, ordinal;
    char debugId[48];
};
struct Response
{
    std::uint32_t magic = 0x52494E52, version = 6, status = 1, count = 0;
    std::uint64_t queueMicros = 0, readMicros = 0, identityMicros = 0;
    std::uint64_t historySkipped = 0;
    std::uint32_t state = 0, batchCount = 0;
    char error[256]{};
};
static_assert(sizeof(CreatedWire) == 56);
static_assert(sizeof(Response) == 312);
enum class ReaderPhase : unsigned {
    Setup, DispatchQueue, TargetAndHookSetup, EngineRead, ValidateRestore,
    IdentityRelease, ResponseWrite, CompletionHandoff, Pacing, Producer, Count
};
struct ReaderTiming {
    std::uint64_t frequency = 0, total = 0;
    std::uint64_t ticks[static_cast<unsigned>(ReaderPhase::Count)]{};
    std::uint64_t factoryTicks = 0, constructorTicks = 0;
    std::uint64_t framesDelivered = 0, frameWaits = 0, frameTimeouts = 0;
};
static_assert(sizeof(ReaderTiming) == 136);
// One ownership timeline: producer -> queued Studio task -> completion ->
// producer. The completion event orders access; a wait is never added to the
// engine work it already contains. Raw QPC ticks avoid cumulative rounding gaps.
struct ReaderClock {
    ReaderTiming value;
    LARGE_INTEGER last{};
    ReaderPhase phase = ReaderPhase::Setup;
    bool enabled;
    ReaderClock(bool trace, LARGE_INTEGER started) : last(started), enabled(trace) {
        LARGE_INTEGER frequency{};
        QueryPerformanceFrequency(&frequency);
        value.frequency = frequency.QuadPart;
    }
    void Next(ReaderPhase next) {
        if (enabled) {
            LARGE_INTEGER now{};
            QueryPerformanceCounter(&now);
            const auto elapsed = static_cast<std::uint64_t>(now.QuadPart - last.QuadPart);
            value.ticks[static_cast<unsigned>(phase)] += elapsed;
            value.total += elapsed;
            last = now;
        }
        phase = next;
    }
};
struct Contract
{
    std::uintptr_t loader, lookup, intern, origin;
    std::uint32_t contextBytes, classOffset;
};

struct HistoryContract
{
    std::uintptr_t table;
    std::uint32_t target, slots, insertion, record, pending, playback;
};

// This shadow table and forwarding callback outlive a fetched vptr. Only the
// active reader's first owned, non-script insertion avoids Studio's undo copy.
// Other insertions and every property/removal callback retain normal behavior.
namespace history {
using Insert = void(__fastcall*)(void*, const SharedInstance*);
struct State
{
    SRWLOCK lock = SRWLOCK_INIT;
    void* object = nullptr;
    void* record = nullptr;
    void** original = nullptr;
    void* shadow[513]{};
    Insert forward = nullptr;
    HistoryContract contract{};
    // Entries live only until the first insertion. Reuse their storage across
    // bounded reader batches instead of allocating once for every instance.
    std::pmr::unsynchronized_pool_resource pendingStorage;
    std::pmr::unordered_map<void*, void*> pending{&pendingStorage};
    std::uint64_t skipped = 0;
    bool enabled = false, restored = true;
};
inline State state;
struct Lock
{
    Lock() { AcquireSRWLockExclusive(&state.lock); }
    ~Lock() { ReleaseSRWLockExclusive(&state.lock); }
};
template<class T> T Field(void* object, std::uint32_t offset)
{
    return *reinterpret_cast<T*>(static_cast<unsigned char*>(object) + offset);
}
inline void __fastcall OnInsert(void* object, const SharedInstance* input)
{
    Insert forward;
    bool skip = false;
    {
        Lock lock;
        forward = state.forward;
        if (state.enabled && object == state.object && input
            && !Field<unsigned char>(object, state.contract.playback)
            && Field<void*>(object, state.contract.record) == state.record
            && !Field<void*>(object, state.contract.pending))
        {
            const auto found = state.pending.find(input->instance);
            if (found != state.pending.end() && found->second == input->owner)
            {
                state.pending.erase(found); // consume before any reentry
                ++state.skipped;
                skip = true;
            }
        }
    }
    if (skip) ReleaseOwnerReference(input->owner); // original consumes its shared_ptr argument
    else forward(object, input);
}
inline void Prepare(void* object, HistoryContract contract, std::size_t count)
{
    Lock lock;
    if (state.enabled || !state.restored || !contract.table || !contract.slots || contract.slots > 512
        || contract.insertion >= contract.slots || contract.record < 0x80 || contract.record >= 0x800
        || contract.pending < 0x80 || contract.pending >= 0x800 || contract.pending == contract.record
        || contract.record % 8 || contract.pending % 8 || contract.playback < 0x80 || contract.playback >= 0x800)
        throw std::runtime_error("Invalid or active native insertion history scope");
    const auto table = Field<void**>(object, 0);
    if (table != reinterpret_cast<void**>(contract.table)
        || (state.forward && state.original != table))
        throw std::runtime_error("Native insertion history identity changed");
    const auto record = Field<void*>(object, contract.record);
    if (!record || Field<void*>(object, contract.pending) || Field<unsigned char>(object, contract.playback))
        throw std::runtime_error("Studio history is not in its ordinary edit mode");
    state.pending.clear();
    state.pending.reserve(count);
    if (!state.forward)
    {
        for (std::size_t i = 0; i <= contract.slots; ++i)
            state.shadow[i] = table[static_cast<std::ptrdiff_t>(i) - 1];
        state.forward = reinterpret_cast<Insert>(table[contract.insertion]);
        state.shadow[contract.insertion + 1] = reinterpret_cast<void*>(&OnInsert);
    }
    state.object = object;
    state.original = table;
    state.record = record;
    state.contract = contract;
    state.skipped = 0;
    if (InterlockedCompareExchangePointer(reinterpret_cast<void* volatile*>(object), &state.shadow[1], table) != table)
        throw std::runtime_error("Studio history changed during native reader preparation");
    state.restored = false;
    state.enabled = true;
}
inline void Created(const SharedInstance& object)
{
    Lock lock;
    state.pending.emplace(object.instance, object.owner);
}
inline bool Restore(std::uint64_t& skipped) noexcept
{
    Lock lock;
    state.enabled = false;
    skipped = state.skipped;
    if (state.restored) return true;
    const auto previous = InterlockedCompareExchangePointer(reinterpret_cast<void* volatile*>(state.object),
        state.original, &state.shadow[1]);
    state.restored = previous == &state.shadow[1] || previous == state.original;
    return state.restored;
}
}

class Scope;
using Originals = std::unordered_map<void**, Create>;
inline std::atomic<std::shared_ptr<const Originals>> originals;
inline std::atomic<Scope*> active{nullptr};
inline std::atomic<unsigned> gate{0}; // idle, active, restoration failed

class Scope
{
    bool trace = false;
    std::uint64_t factoryTicks = 0, constructorTicks = 0;
    struct FactoryTimer {
        Scope* scope;
        LARGE_INTEGER started{};
        explicit FactoryTimer(Scope* value) : scope(value && value->trace ? value : nullptr) {
            if (scope) QueryPerformanceCounter(&started);
        }
        ~FactoryTimer() {
            if (scope) {
                LARGE_INTEGER end{};
                QueryPerformanceCounter(&end);
                scope->factoryTicks += end.QuadPart - started.QuadPart;
            }
        }
    };
    struct Bound { Binding value; bool used = false; };
    struct Entry
    {
        std::uint32_t expected = 0, calls = 0, classIndex = 0;
        std::vector<Bound> bound;
        bool script = false;
        Create create = nullptr;
        std::unordered_map<std::uint32_t, std::uint32_t> aliases;
        std::unordered_map<std::uint32_t, SharedInstance> anchors;
    };
    Contract contract;
    std::uint64_t context[64]{};
    std::unordered_map<void*, Entry> entries;
    std::vector<std::shared_ptr<void>> owners;
    std::vector<void**> installed;
    std::vector<Created> created;
    std::shared_ptr<const Originals> forwarding;
    bool ownsGate = false, restored = true;
    std::uint64_t historySkipped = 0;

    static void WriteSlot(void** slot, void* value)
    {
        DWORD previous = 0;
        if (!VirtualProtect(slot, sizeof(void*), PAGE_READWRITE, &previous))
            throw std::runtime_error("Cannot protect native reader factory slot");
        InterlockedExchangePointer(slot, value);
        DWORD ignored = 0;
        if (!VirtualProtect(slot, sizeof(void*), previous, &ignored))
            throw std::runtime_error("Cannot restore native reader factory protection");
    }

    __declspec(noinline) static SharedInstance* __fastcall CreateBound(
        void* factory, SharedInstance* result, void* creationContext, bool flag)
    {
        auto* scope = active.load(std::memory_order_acquire);
        FactoryTimer factoryTimer(scope && creationContext == scope->context ? scope : nullptr);
        Entry* createdEntry = nullptr;
        std::uint32_t createdOrdinal = 0;
        if (scope && creationContext == scope->context)
        {
            if (reinterpret_cast<std::uintptr_t>(_ReturnAddress()) != scope->contract.origin || !flag)
                throw std::runtime_error("Native reader creation context reached an unrecognized caller");
            const auto found = scope->entries.find(factory);
            if (found == scope->entries.end())
                throw std::runtime_error("Native reader created an unplanned class");
            auto& entry = found->second;
            const auto ordinal = entry.calls++;
            if (ordinal >= entry.expected)
                throw std::runtime_error("Native reader exceeded its planned class count");
            if (const auto alias = entry.aliases.find(ordinal); alias != entry.aliases.end())
            {
                const auto anchor = entry.anchors.at(alias->second);
                if (!anchor.instance || !AddOwnerReference(anchor.owner))
                    throw std::runtime_error("Native reader batch anchor is unavailable");
                *result = anchor;
                return result;
            }
            for (auto& binding : entry.bound)
                if (binding.value.ordinal == ordinal)
                {
                    if (binding.used || !AddOwnerReference(binding.value.target.owner))
                        throw std::runtime_error("Native reader binding expired or was reused");
                    binding.used = true;
                    *result = binding.value.target;
                    if (const auto anchor = entry.anchors.find(ordinal); anchor != entry.anchors.end())
                        anchor->second = *result;
                    return result;
                }
            // The context identifies our read across worker/fiber handoff. It
            // must never leak into ordinary constructors as a fake engine context.
            createdEntry = &entry;
            createdOrdinal = ordinal;
            creationContext = nullptr;
        }
        Create create = createdEntry ? createdEntry->create : nullptr;
        if (!createdEntry)
        {
            // Foreign creation can outlive a fetched interception slot. Its
            // forwarding table therefore retains independent shared ownership.
            const auto saved = originals.load(std::memory_order_acquire);
            const auto slot = *reinterpret_cast<void***>(factory);
            const auto found = saved->find(slot);
            if (found == saved->end()) throw std::runtime_error("Native reader factory changed");
            create = found->second;
        }
        LARGE_INTEGER constructorStart{}, constructorEnd{};
        if (createdEntry && scope->trace) QueryPerformanceCounter(&constructorStart);
        const auto returned = create(factory, result, creationContext, flag);
        if (createdEntry && scope->trace) {
            QueryPerformanceCounter(&constructorEnd);
            scope->constructorTicks += constructorEnd.QuadPart - constructorStart.QuadPart;
        }
        if (createdEntry)
        {
            if (returned != result || !result->instance || !AddOwnerReference(result->owner))
                throw std::runtime_error("Native reader factory did not return an owned instance");
            // Reserved before interception: no per-instance allocation, path
            // lookup or second hierarchy walk to identify our own creations.
            scope->created.push_back({*result, createdEntry->classIndex, createdOrdinal});
            if (const auto anchor = createdEntry->anchors.find(createdOrdinal); anchor != createdEntry->anchors.end())
                anchor->second = *result;
            if (!createdEntry->script) history::Created(*result);
        }
        return returned;
    }

public:
    explicit Scope(Contract value, bool timing = false) : trace(timing), contract(value) {}
    Scope(const Scope&) = delete;
    Scope& operator=(const Scope&) = delete;
    ~Scope() { Restore(); }

    const std::vector<Created>& CreatedInstances() const { return created; }
    std::uint64_t HistorySkipped() const { return historySkipped; }
    std::uint64_t FactoryTicks() const { return factoryTicks; }
    std::uint64_t ConstructorTicks() const { return constructorTicks; }
    void ReleaseCreated() noexcept
    {
        for (const auto& item : created) ReleaseOwnerReference(item.target.owner);
        created.clear();
    }
    void ReleaseBindings() { owners.clear(); }

    // Targets must already be held and identity-checked under the DataModel
    // task lock. This takes separate references for the reader's own lifetime.
    void Prepare(const std::vector<Class>& classes, const std::vector<Binding>& bindings,
        const std::vector<Alias>& aliases, void* historyObject, HistoryContract historyContract)
    {
        if (!contract.loader || !contract.lookup || !contract.intern || !contract.origin
            || contract.contextBytes < 8 || contract.contextBytes > sizeof(context)
            || contract.contextBytes % 8 || contract.classOffset > 0x100
            || contract.classOffset % 8 || classes.size() > 4096
            || bindings.size() > CaptureMaxRows)
            throw std::runtime_error("Invalid native reader contract");
        unsigned idle = 0;
        if (!gate.compare_exchange_strong(idle, 1))
            throw std::runtime_error(idle == 1 ? "Native reader is busy" : "Native reader restoration failed; restart Studio");
        ownsGate = true;
        entries.reserve(classes.size());
        std::vector<void*> factories;
        factories.reserve(classes.size());
        std::size_t count = 0;
        for (const auto& item : classes)
        {
            count += item.count;
            if (!item.count || count > CaptureMaxRows || item.name.empty() || item.name.size() > 255
                || item.name.find('\0') != std::string::npos)
                throw std::runtime_error("Invalid native reader class plan");
            const auto name = reinterpret_cast<void*(__fastcall*)(const std::string*)>(contract.intern)(&item.name);
            const auto factory = reinterpret_cast<void*(__fastcall*)(void*)>(contract.lookup)(name);
            if (!factory || !entries.emplace(factory, Entry{item.count, 0, static_cast<std::uint32_t>(factories.size()), {}, item.script}).second)
                throw std::runtime_error("Native reader class factory is missing or duplicated");
            factories.push_back(factory);
        }
        created.reserve(count);
        owners.reserve(bindings.size());
        for (const auto& binding : bindings)
        {
            if (binding.classIndex >= factories.size())
                throw std::runtime_error("Native reader binding class is outside the plan");
            auto& entry = entries.at(factories[binding.classIndex]);
            if (binding.ordinal >= entry.expected || !binding.target.instance
                || std::any_of(entry.bound.begin(), entry.bound.end(), [&](const Bound& other) {
                    return other.value.ordinal == binding.ordinal || other.value.target.instance == binding.target.instance;
                })) throw std::runtime_error("Native reader binding is invalid or duplicated");
            if (!AddOwnerReference(binding.target.owner))
                throw std::runtime_error("Native reader binding target expired");
            owners.emplace_back(binding.target.owner, ReleaseOwnerReference);
            const auto descriptor = *reinterpret_cast<unsigned char**>(
                static_cast<unsigned char*>(binding.target.instance) + contract.classOffset);
            const auto name = *reinterpret_cast<void**>(descriptor + 8);
            if (reinterpret_cast<void*(__fastcall*)(void*)>(contract.lookup)(name) != factories[binding.classIndex])
                throw std::runtime_error("Native reader binding changed class");
            entry.bound.push_back({binding});
        }
        for (const auto& alias : aliases)
        {
            if (alias.classIndex >= factories.size())
                throw std::runtime_error("Native batch anchor class is outside the plan");
            auto& entry = entries.at(factories[alias.classIndex]);
            if (alias.sourceOrdinal >= alias.ordinal || alias.ordinal >= entry.expected
                || !entry.aliases.emplace(alias.ordinal, alias.sourceOrdinal).second
                || std::any_of(entry.bound.begin(), entry.bound.end(), [&](const Bound& bound) {
                    return bound.value.ordinal == alias.ordinal;
                })) throw std::runtime_error("Invalid native batch anchor ordinal");
            entry.anchors.try_emplace(alias.sourceOrdinal, SharedInstance{});
        }
        for (const auto& [factory, entry] : entries)
            for (const auto& [source, anchor] : entry.anchors)
                if (entry.aliases.count(source)) throw std::runtime_error("Native batch anchors cannot form alias chains");
        const auto previous = originals.load(std::memory_order_acquire);
        auto next = std::make_shared<Originals>(previous ? *previous : Originals{});
        installed.reserve(entries.size());
        for (auto& [factory, entry] : entries)
        {
            const auto slot = *reinterpret_cast<void***>(factory);
            const auto original = reinterpret_cast<Create>(*slot);
            if (!original || original == CreateBound)
                throw std::runtime_error("Native reader factory is already intercepted");
            const auto [it, inserted] = next->emplace(slot, original);
            if (!inserted && it->second != original)
                throw std::runtime_error("Native reader factory changed");
            entry.create = original;
        }
        forwarding = std::move(next);
        originals.store(forwarding, std::memory_order_release);
        for (const auto& [factory, entry] : entries)
        {
            const auto slot = *reinterpret_cast<void***>(factory);
            if (std::find(installed.begin(), installed.end(), slot) != installed.end()) continue;
            installed.push_back(slot);
            restored = false;
            WriteSlot(slot, reinterpret_cast<void*>(CreateBound));
        }
        history::Prepare(historyObject, historyContract, count);
        active.store(this, std::memory_order_release);
    }

    void Read(void* model, std::istream& stream)
    {
        // Full-place loader's validated flag value. Properties and parenting are
        // performed by the engine, including retained services and containers.
        reinterpret_cast<Load>(contract.loader)(context, &stream, model, 4, nullptr, nullptr);
    }

    void Verify() const
    {
        if (!restored) throw std::runtime_error("Native reader factory restoration failed");
        const std::uint64_t zero[64]{};
        if (memcmp(context, zero, sizeof(context)))
            throw std::runtime_error("Native reader altered its empty creation context");
        for (const auto& [factory, entry] : entries)
            if (entry.calls != entry.expected || std::any_of(entry.bound.begin(), entry.bound.end(),
                [](const Bound& binding) { return !binding.used; }))
                throw std::runtime_error("Native reader result does not match its class/binding plan");
    }

    void Restore() noexcept
    {
        if (!ownsGate) return;
        active.store(nullptr, std::memory_order_release);
        restored = history::Restore(historySkipped);
        for (const auto slot : installed)
        {
            DWORD previous = 0;
            if (!VirtualProtect(slot, sizeof(void*), PAGE_READWRITE, &previous)) { restored = false; continue; }
            InterlockedCompareExchangePointer(slot, reinterpret_cast<void*>(forwarding->at(slot)),
                reinterpret_cast<void*>(CreateBound));
            if (*slot != reinterpret_cast<void*>(forwarding->at(slot))) restored = false;
            DWORD ignored = 0;
            if (!VirtualProtect(slot, sizeof(void*), previous, &ignored)) restored = false;
        }
        gate.store(restored ? 0 : 2, std::memory_order_release);
        ownsGate = false;
    }
};

// Structured faults do not unwind C++ objects with /EHsc. Restore the narrow
// interception in a finally boundary before the task's fault handler runs.
inline void ReadFinally(Scope* scope, void* model, std::istream* stream)
{
    __try { scope->Read(model, *stream); }
    __finally { scope->Restore(); }
}

struct Parameters
{
    std::uint64_t dataModel, owner, taskContext, submit;
    Contract contract;
    std::uint64_t debugIdGetter, deallocator;
    std::uint32_t childrenOffset, selfOffset, targetCount, classCount;
    std::uint64_t inputLength;
    std::uint32_t timeoutMs, status;
    std::uint64_t queueMicros, readMicros;
    char error[256];
    wchar_t outputPath[520];
    std::uint32_t batchCount, reserved;
    HistoryContract history;
    std::uint32_t aliasCount, batchPauseMs;
};
struct ClassWire { char name[256]; std::uint32_t count, script; };
struct TargetWire
{
    SharedInstance target;
    std::uint32_t parentIndex, classIndex; // UINT32_MAX: DataModel / not a factory binding.
    char debugId[48];
    std::uint32_t ordinal, roles; // bit 0: retained snapshot container.
};
static_assert(sizeof(Parameters) == 1480);
static_assert(sizeof(Alias) == 12);
static_assert(sizeof(ClassWire) == 264);
static_assert(sizeof(TargetWire) == 80);

// Sleep(1) rounds to the process timer interval and can consume nearly two
// milliseconds for every batch. Keep the scheduling gap without changing the
// system timer resolution or occupying a worker with a spin wait.
struct BatchPacer
{
    HANDLE timer = nullptr;
    explicit BatchPacer(bool enabled)
    {
        if (enabled) timer = CreateWaitableTimerExW(nullptr, nullptr,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_MODIFY_STATE | SYNCHRONIZE);
    }
    ~BatchPacer() { if (timer) CloseHandle(timer); }
    void Wait(DWORD milliseconds)
    {
        LARGE_INTEGER due{};
        due.QuadPart = -static_cast<LONGLONG>(milliseconds) * 10000;
        if (timer && SetWaitableTimer(timer, &due, 0, nullptr, nullptr, FALSE)
            && WaitForSingleObject(timer, milliseconds + 100) == WAIT_OBJECT_0) return;
        // Older Windows versions can lack high-resolution waitable timers.
        Sleep(milliseconds);
    }
};

struct Task
{
    Parameters result;
    ReaderClock clock;
    std::vector<Class> classes;
    std::vector<TargetWire> targets;
    std::vector<Binding> bindings;
    std::vector<Alias> aliases;
    std::vector<std::shared_ptr<void>> owners;
    std::vector<std::istringstream> inputs;
    std::vector<std::uint64_t> batchMicros;
    std::size_t inputIndex = 0;
    Scope scope;
    ReniumSerializerParams identityResult{};
    RunState identity{};
    std::vector<CreatedWire> receipt;
    HANDLE output = INVALID_HANDLE_VALUE;
    std::uint64_t identityMicros = 0;
    std::uint32_t phaseState = 0;
    std::uint64_t deadline;
    HANDLE completed = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    explicit Task(const Parameters& value, LARGE_INTEGER started) : result(value),
        clock(value.reserved != 0, started), scope(value.contract, value.reserved != 0),
        deadline(GetTickCount64() + value.timeoutMs)
    {
        identity.deallocate = reinterpret_cast<Deallocator>(value.deallocator);
        identity.params = &identityResult;
        if (!completed) throw std::runtime_error("Cannot create native import completion event");
    }
    ~Task()
    {
        DestroyDebugStringCaught(&identity);
        if (identityResult.status) gate.store(2, std::memory_order_release);
        if (output != INVALID_HANDLE_VALUE) CloseHandle(output);
        if (completed) CloseHandle(completed);
    }
    void ReadIdentity(void* instance, char (&destination)[48])
    {
        using Getter = void*(__fastcall*)(void*, void*, std::int32_t);
        const auto returned = reinterpret_cast<Getter>(result.debugIdGetter)(instance, &identity.debugString, 32);
        identity.debugStringBuilt = true;
        const auto& text = identity.debugString;
        if (returned != &text || !text.size || text.size >= sizeof(destination) || text.size > text.capacity)
            throw std::runtime_error("Native import identity getter returned invalid data");
        const auto data = text.capacity < 16 ? text.storage.inlineBytes : text.storage.heap;
        if (memchr(data, 0, text.size)) throw std::runtime_error("Native import identity contains a terminator");
        memcpy(destination, data, text.size);
        destination[text.size] = 0;
        DestroyDebugString(&identity);
    }
    void CaptureCreated()
    {
        LARGE_INTEGER begin{}, end{}, frequency{};
        QueryPerformanceFrequency(&frequency);
        QueryPerformanceCounter(&begin);
        receipt.reserve(scope.CreatedInstances().size());
        for (const auto& created : scope.CreatedInstances())
        {
            CreatedWire row{created.classIndex, created.ordinal, {}};
            ReadIdentity(created.target.instance, row.debugId);
            receipt.push_back(row);
        }
        QueryPerformanceCounter(&end);
        identityMicros = ElapsedMicros(begin, end, frequency);
        phaseState |= 8; // Every native creation has an identity receipt.
    }
    void Publish()
    {
        Response response;
        response.status = result.status;
        response.count = static_cast<std::uint32_t>(receipt.size());
        response.queueMicros = result.queueMicros;
        response.readMicros = result.readMicros;
        response.identityMicros = identityMicros;
        response.historySkipped = scope.HistorySkipped();
        response.state = phaseState;
        response.batchCount = static_cast<std::uint32_t>(batchMicros.size());
        strncpy_s(response.error, result.error, _TRUNCATE);
        DWORD written = 0;
        if (!WriteFile(output, &response, sizeof(response), &written, nullptr) || written != sizeof(response))
            throw std::runtime_error("Cannot publish native import result");
        const auto bytes = static_cast<DWORD>(receipt.size() * sizeof(CreatedWire));
        if (bytes && (!WriteFile(output, receipt.data(), bytes, &written, nullptr) || written != bytes))
            throw std::runtime_error("Cannot publish native import identities");
        const auto timingBytes = static_cast<DWORD>(batchMicros.size() * sizeof(std::uint64_t));
        if (!WriteFile(output, batchMicros.data(), timingBytes, &written, nullptr) || written != timingBytes)
            throw std::runtime_error("Cannot publish native import batch timings");
    }
    void ValidateTargets()
    {
        const auto model = reinterpret_cast<void*>(result.dataModel);
        owners.reserve(targets.size());
        bindings.reserve(targets.size());
        for (std::size_t index = 0; index < targets.size(); ++index)
        {
            const auto& target = targets[index];
            if (target.roles > 1 || (target.parentIndex != UINT32_MAX && target.parentIndex >= index)
                || !memchr(target.debugId, 0, sizeof(target.debugId)))
                throw std::runtime_error("Invalid native import target chain");
            const auto parent = target.parentIndex == UINT32_MAX ? model : targets[target.parentIndex].target.instance;
            const auto children = *reinterpret_cast<const SharedVector* const*>(
                static_cast<const unsigned char*>(parent) + result.childrenOffset);
            const auto count = CaptureChildCount(children);
            bool found = false;
            for (std::size_t child = 0; child < count; ++child)
                if (children->begin[child].instance == target.target.instance
                    && children->begin[child].owner == target.target.owner) { found = true; break; }
            if (!found || !AddOwnerReference(target.target.owner))
                throw std::runtime_error("Native import target changed before execution");
            owners.emplace_back(target.target.owner, ReleaseOwnerReference);
            const auto self = reinterpret_cast<const SharedInstance*>(
                static_cast<const unsigned char*>(target.target.instance) + result.selfOffset);
            if (self->instance != target.target.instance || self->owner != target.target.owner)
                throw std::runtime_error("Native import target ownership changed");
            if (target.debugId[0])
            {
                char actual[48]{};
                ReadIdentity(target.target.instance, actual);
                if (strcmp(actual, target.debugId))
                    throw std::runtime_error("Native import target differs from the editor transaction");
            }
            if (target.classIndex != UINT32_MAX)
                bindings.push_back({target.target, target.classIndex, target.ordinal});
        }
    }
};

inline void ExecuteFinally(Task* task)
{
    __try
    {
        if (task->inputIndex == 0)
        {
            task->ValidateTargets();
            task->scope.Prepare(task->classes, task->bindings, task->aliases,
                task->targets[task->result.history.target].target.instance, task->result.history);
        }
        LARGE_INTEGER begin{}, end{}, frequency{};
        QueryPerformanceFrequency(&frequency);
        task->phaseState |= 2;
        task->clock.Next(ReaderPhase::EngineRead);
        QueryPerformanceCounter(&begin);
        task->scope.Read(reinterpret_cast<void*>(task->result.dataModel), task->inputs[task->inputIndex]);
        QueryPerformanceCounter(&end);
        task->batchMicros[task->inputIndex] = ElapsedMicros(begin, end, frequency);
        if (task->inputIndex + 1 == task->inputs.size()) task->phaseState |= 4;
    }
    __finally
    {
        task->clock.Next(ReaderPhase::ValidateRestore);
        if (AbnormalTermination() || (task->phaseState & 4)) task->scope.Restore();
    }
}
inline void CaptureCreatedFinally(Task* task)
{
    __try { task->CaptureCreated(); }
    __finally
    {
        task->scope.ReleaseCreated();
        task->scope.ReleaseBindings();
        task->owners.clear();
    }
}
inline int ReaderFault(EXCEPTION_POINTERS* fault, Parameters* result)
{
    const auto code = fault->ExceptionRecord->ExceptionCode;
    if (code == 0xE06D7363) return EXCEPTION_CONTINUE_SEARCH;
    result->status = 0xE50A;
    sprintf_s(result->error, "Native import fault 0x%08X; transaction recovery is required", code);
    return EXCEPTION_EXECUTE_HANDLER;
}
inline void ExecuteCaught(Task* task)
{
    __try { ExecuteFinally(task); }
    __except (ReaderFault(GetExceptionInformation(), &task->result)) {}
}
inline void CaptureCreatedCaught(Task* task)
{
    __try { CaptureCreatedFinally(task); }
    __except (ReaderFault(GetExceptionInformation(), &task->result)) {}
}
inline DWORD Queue(Parameters* parameters)
{
    LARGE_INTEGER callStarted{};
    QueryPerformanceCounter(&callStarted);
    if (!parameters || !parameters->dataModel || !parameters->owner || !parameters->taskContext || !parameters->submit
        || !parameters->debugIdGetter || !parameters->deallocator
        || !parameters->batchCount || parameters->batchCount > 4096 || parameters->reserved > 1
        || !parameters->timeoutMs || parameters->timeoutMs > 20000
        || !parameters->targetCount || parameters->targetCount > CaptureMaxRows
        || parameters->history.target >= parameters->targetCount
        || parameters->batchPauseMs > 16 || parameters->aliasCount > CaptureMaxRows
        || parameters->classCount > 4096
        || parameters->inputLength < 32 || parameters->inputLength > CaptureMaxBytes
        || parameters->childrenOffset > 0x200 || parameters->childrenOffset % 8
        || parameters->selfOffset > 0x80 || parameters->selfOffset % 8
        || !parameters->outputPath[0]
        || wmemchr(parameters->outputPath, 0, 520) == nullptr)
        throw std::runtime_error("Invalid native import parameters");
    const auto owner = reinterpret_cast<void*>(parameters->owner);
    if (!AddOwnerReference(owner)) throw std::runtime_error("Native import DataModel expired");
    const auto modelHold = std::shared_ptr<void>(owner, ReleaseOwnerReference);
    auto task = std::make_shared<Task>(*parameters, callStarted);
    task->output = CreateFileW(parameters->outputPath, GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
        OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (task->output == INVALID_HANDLE_VALUE)
        throw std::runtime_error("Native import response transport is unavailable");
    const auto classes = reinterpret_cast<const ClassWire*>(parameters + 1);
    task->classes.reserve(parameters->classCount);
    for (std::size_t index = 0; index < parameters->classCount; ++index)
    {
        const auto& item = classes[index];
        if (item.script > 1 || !memchr(item.name, 0, sizeof(item.name)))
            throw std::runtime_error("Invalid native import class name");
        task->classes.push_back({item.name, item.count, item.script != 0});
    }
    const auto targets = reinterpret_cast<const TargetWire*>(classes + parameters->classCount);
    task->targets.assign(targets, targets + parameters->targetCount);
    const auto aliases = reinterpret_cast<const Alias*>(targets + parameters->targetCount);
    task->aliases.assign(aliases, aliases + parameters->aliasCount);
    const auto lengths = reinterpret_cast<const std::uint64_t*>(aliases + parameters->aliasCount);
    const auto bytes = reinterpret_cast<const char*>(lengths + parameters->batchCount);
    std::uint64_t offset = 0;
    task->inputs.reserve(parameters->batchCount);
    task->batchMicros.resize(parameters->batchCount);
    for (std::size_t index = 0; index < parameters->batchCount; ++index)
    {
        const auto length = lengths[index];
        if (length < 32 || length > parameters->inputLength - offset)
            throw std::runtime_error("Invalid native import batch boundary");
        // Own the bytes: a timed-out caller can release its remote parameters
        // while the queued Studio task still holds these streams.
        task->inputs.emplace_back(std::string(bytes + offset, static_cast<std::size_t>(length)));
        offset += length;
    }
    if (offset != parameters->inputLength)
        throw std::runtime_error("Native import batches do not cover the payload");
    task->result.status = 1;
    LARGE_INTEGER frequency{};
    QueryPerformanceFrequency(&frequency);
    BatchPacer pacer(parameters->batchPauseMs != 0 && task->inputs.size() > 1);
    const auto frames = renium_observation::FindFrames(parameters->dataModel);
    auto lastFrame = frames ? frames->counter.load(std::memory_order_acquire) : 0;
    std::uint64_t workSincePauseMicros = 0;
    // Return the DataModel lock between bounded subtrees. The exact
    // creation context and strong anchors span batches; other constructors forward.
    for (task->inputIndex = 0; task->inputIndex < task->inputs.size(); ++task->inputIndex)
    {
        LARGE_INTEGER queued{};
        QueryPerformanceCounter(&queued);
        ResetEvent(task->completed);
        std::function<void()> work{[task, modelHold, queued, frequency]() {
            LARGE_INTEGER begin{}, end{};
            QueryPerformanceCounter(&begin);
            task->result.queueMicros += ElapsedMicros(queued, begin, frequency);
            task->clock.Next(ReaderPhase::TargetAndHookSetup);
            try
            {
                if (!RemainingMilliseconds(task->deadline))
                    throw std::runtime_error("Native import expired before execution");
                ExecuteCaught(task.get());
                if (task->result.status == 1 && (task->phaseState & 4))
                {
                    task->scope.Verify();
                    task->result.status = 4;
                }
            }
            catch (const std::exception& error)
            {
                task->result.status = 0xE508;
                strncpy_s(task->result.error, error.what(), _TRUNCATE);
            }
            catch (...)
            {
                task->result.status = 0xE509;
                strncpy_s(task->result.error, "Unknown native import failure; transaction recovery is required", _TRUNCATE);
            }
            if (task->result.status != 1) task->scope.Restore();
            QueryPerformanceCounter(&end);
            task->result.readMicros += ElapsedMicros(begin, end, frequency);
            try { if (task->result.status != 1) {
                task->clock.Next(ReaderPhase::IdentityRelease);
                CaptureCreatedCaught(task.get());
                task->clock.Next(ReaderPhase::ResponseWrite);
                task->Publish();
            } }
            catch (const std::exception& error)
            {
                task->result.status = 0xE50E;
                strncpy_s(task->result.error, error.what(), _TRUNCATE);
            }
            task->clock.Next(ReaderPhase::CompletionHandoff);
            SetEvent(task->completed);
        }};
        task->clock.Next(ReaderPhase::DispatchQueue);
        if (!reinterpret_cast<SubmitDataModelTask>(parameters->submit)(reinterpret_cast<void*>(parameters->taskContext), &work, 1))
            throw std::runtime_error("Studio rejected the native import task");
        const auto remaining = RemainingMilliseconds(task->deadline);
        if (!remaining || WaitForSingleObject(task->completed, remaining) != WAIT_OBJECT_0)
            throw std::runtime_error("Native import response expired; inspect the transaction outcome before retrying");
        task->clock.Next(ReaderPhase::Producer);
        if (task->result.status != 1) break;
        // Return the write lock after every batch and leave an explicit scheduling
        // gap after a bounded amount of reader work. Pausing after every small
        // batch adds hundreds of redundant timer waits to a full-place import.
        workSincePauseMicros += task->batchMicros[task->inputIndex];
        if (frames && frames->active.load(std::memory_order_acquire)) {
            const auto current = frames->counter.load(std::memory_order_acquire);
            if (current != lastFrame) {
                task->clock.value.framesDelivered += current - lastFrame;
                lastFrame = current;
                workSincePauseMicros = 0;
            }
        }
        if (parameters->batchPauseMs && workSincePauseMicros >= 16000) {
            task->clock.Next(ReaderPhase::Pacing);
            if (frames && frames->active.load(std::memory_order_acquire)) {
                ++task->clock.value.frameWaits;
                // Resume as soon as an actual Heartbeat runs. Fixed OS sleeps
                // can both waste already-delivered frames and miss starved ones.
                // Bound this wait when a background Studio stops emitting frames.
                const auto until = GetTickCount64() + 16;
                while (frames->active.load(std::memory_order_acquire)
                    && frames->counter.load(std::memory_order_acquire) == lastFrame) {
                    const auto now = GetTickCount64();
                    if (now >= until || WaitForSingleObject(frames->changed,
                        static_cast<DWORD>(until - now)) != WAIT_OBJECT_0) break;
                }
                const auto current = frames->counter.load(std::memory_order_acquire);
                if (current == lastFrame) ++task->clock.value.frameTimeouts;
                task->clock.value.framesDelivered += current - lastFrame;
                lastFrame = current;
            } else pacer.Wait(parameters->batchPauseMs);
            workSincePauseMicros = 0;
            task->clock.Next(ReaderPhase::Producer);
        }
    }
    parameters->status = task->result.status;
    task->clock.value.factoryTicks = task->scope.FactoryTicks();
    task->clock.value.constructorTicks = task->scope.ConstructorTicks();
    task->clock.Next(ReaderPhase::Producer);
    DWORD timingWritten = 0;
    if (!WriteFile(task->output, &task->clock.value, sizeof(ReaderTiming), &timingWritten, nullptr)
        || timingWritten != sizeof(ReaderTiming))
        throw std::runtime_error("Cannot publish native import accounting");
    return parameters->status == 4 ? 0 : parameters->status;
}
inline DWORD Boundary(Parameters* parameters)
{
    try { return Queue(parameters); }
    catch (const std::exception& error)
    {
        if (parameters) {
            parameters->status = 0xE50B;
            strncpy_s(parameters->error, error.what(), _TRUNCATE);
        }
        return 0xE50B;
    }
    catch (...) { return 0xE50C; }
}
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumReadServices(renium_reader::Parameters* parameters)
{
    __try
    {
        __try { return renium_reader::Boundary(parameters); }
        __except (EXCEPTION_EXECUTE_HANDLER) { return 0xE50D; }
    }
    __finally { if (parameters) VirtualFree(parameters, 0, MEM_RELEASE); }
}
