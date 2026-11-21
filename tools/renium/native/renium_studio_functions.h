#include <array>
#include <condition_variable>
#include <mutex>
#include <unordered_map>
#include <vector>
#ifdef _WIN32
#include <Windows.h>
#include <intrin.h>
#endif

namespace renium_functions {
struct PackageFlagRefused : std::runtime_error { using std::runtime_error::runtime_error; };
struct StaleDataModel : std::runtime_error { using std::runtime_error::runtime_error; };
constexpr std::size_t MaxResponse = 65536; // Fixed property-response transport capacity.
constexpr std::uint32_t TimeoutCode = 0xE412, DetectorCode = 0xE413, SizeCode = 0xE414;
constexpr std::uint32_t PayloadCode = 0xE415, LimitCode = 0xE416, InvocationCode = 0xE417;
struct Failure : std::runtime_error {
    std::uint32_t code;
    std::uint32_t sent; // 0: not entered; 1: answer received; 2: unknown.
    Failure(std::uint32_t code, const std::string& text, std::uint32_t sent = 0) : std::runtime_error(text), code(code), sent(sent) {}
};
inline std::string TooLargeText(std::size_t size) {
    return "http-response-size: response too large (" + std::to_string(size) + " bytes; limit " + std::to_string(MaxResponse) + " bytes)";
}
struct Input {
    std::uint64_t descriptor, table;
    std::uint32_t field, mode;
    std::uint64_t function;
    std::uint32_t enums[3], firstSize, secondSize, reserved;
};
static_assert(sizeof(Input) == 56);

inline std::uint32_t NativeStringContract() {
    const std::string inlineValue("abc"), heapValue(80, 'x');
    auto field = [&](const std::string& value, std::size_t at) {
        std::size_t out; std::memcpy(&out, reinterpret_cast<const char*>(&value) + at, sizeof(out)); return out;
    };
    std::size_t size = 64, capacity = 64;
    for (std::size_t at = 0; at + sizeof(std::size_t) <= sizeof(std::string); ++at) {
        if (field(inlineValue, at) == inlineValue.size() && field(heapValue, at) == heapValue.size()) {
            if (size != 64) return 0; size = at;
        }
        if (field(inlineValue, at) == inlineValue.capacity() && field(heapValue, at) == heapValue.capacity()) {
            if (capacity != 64) return 0; capacity = at;
        }
    }
    auto data = reinterpret_cast<std::uintptr_t>(inlineValue.data()) - reinterpret_cast<std::uintptr_t>(&inlineValue);
    if (data + sizeof(void*) > sizeof(std::string) || size >= 64 || capacity >= 64 || inlineValue.capacity() >= 64
        || field(heapValue, data) != reinterpret_cast<std::uintptr_t>(heapValue.data())) return 0;
    return 2 | static_cast<std::uint32_t>(size << 2 | capacity << 8 | data << 14 | inlineValue.capacity() << 20);
}

// A yield request carries the proved string layout in the low bits of
// `reserved` and, above them, the number of proved call sites that follow the
// arguments. A site is the return address into the engine's invoker, then the
// return address into the function that hands that invoker a fresh string.
constexpr std::uint32_t LayoutBits = 26;
using Sites = std::vector<std::array<std::uint64_t, 2>>;
inline bool ReadYieldSites(const Input& input, const char* bytes, std::size_t size, Sites& sites) {
    const std::uint64_t count = input.reserved >> LayoutBits;
    const auto arguments = sizeof(input) + std::uint64_t{input.firstSize} + input.secondSize;
    if (!count || size != arguments + count * sizeof(Sites::value_type)) return false;
    sites.resize(count);
    std::memcpy(sites.data(), bytes + arguments, count * sizeof(Sites::value_type));
    return true;
}

// Called on the transport worker before queue submission, and again by Begin.
// No Studio pointers are read and no callbacks constructed for an unqualified
// yield ABI. Reflection's public signature and the queue's callable ABI do not
// establish the member's hidden continuations or required execution context.
inline void ValidateCallContract(const char* bytes, std::size_t size) {
    Input input{};
    if (size < sizeof(input)) throw std::runtime_error("Truncated function request");
    std::memcpy(&input, bytes, sizeof(input));
    if (input.mode == 1 || input.mode == 2) {
#ifdef _WIN32
        Sites sites;
        const auto layout = input.reserved & ((1u << LayoutBits) - 1);
        if (layout && layout == NativeStringContract() && ReadYieldSites(input, bytes, size, sites)) return;
#endif
        throw Failure(DetectorCode, "yield-call-contract: hidden success/error continuation ABI, ownership and execution context unverified; no call executed");
    }
}

#ifdef _WIN32
// No C++ objects in this frame: /EHsc does not unwind their destructors on SEH.
// The finally block runs on a refused read, a C++ exception and a hardware fault.
__declspec(noinline) inline bool ConfirmPackageByte(volatile char* byte, bool(*get)(void*), void* context) {
    bool confirmed = false;
    char written = -1;
    __try {
        if (_InterlockedCompareExchange8(byte, 1, 0) != 0) __leave;
        written = 1;
        if (!get(context)) __leave;
        if (_InterlockedCompareExchange8(byte, 0, 1) != 1) __leave;
        written = 0;
        if (get(context)) __leave;
        if (_InterlockedCompareExchange8(byte, 1, 0) != 0) __leave;
        written = 1;
        confirmed = get(context);
    } __finally {
        if (!confirmed && written >= 0) _InterlockedCompareExchange8(byte, 0, written);
    }
    return confirmed;
}

// The worker derives one unique byte before submitting this synchronous task.
template<class Getter, class Writable>
bool ConfirmPackageFlag(std::uintptr_t address, Getter get, Writable writable) {
    if (get()) return true;
    if (!writable(address) || *reinterpret_cast<volatile unsigned char*>(address) != 0)
        throw PackageFlagRefused("Package flag candidate is not writable zero-valued image data; use the dialog watcher");
    if (ConfirmPackageByte(reinterpret_cast<volatile char*>(address),
            [](void* context) { return (*static_cast<Getter*>(context))(); }, &get)) return true;
    throw PackageFlagRefused("GetFastFlag did not confirm the package flag toggle; restored the byte, use the dialog watcher");
}

#endif

struct Completion {
    std::mutex mutex;
    std::condition_variable changed;
    bool done = false, httpError = false;
    std::uint32_t failure = 0;
    std::string value, error;
    void Complete(std::string result, bool http, std::uint32_t code) {
        std::lock_guard lock(mutex);
        if (done) return;
        failure = code;
        httpError = http;
        if (code) error = std::move(result);
        else value = std::move(result);
        done = true;
        changed.notify_all();
    }
    void Fail(std::string text, std::uint32_t code = PayloadCode) { Complete(std::move(text), false, code); }
    void Finish(std::string result, bool success) {
        if (result.size() > MaxResponse) Fail(TooLargeText(result.size()), SizeCode);
        else Complete(std::move(result), !success, 0);
    }

    std::string Wait(std::chrono::milliseconds timeout) {
        std::unique_lock lock(mutex);
        const auto deadline = std::chrono::steady_clock::now() + timeout;
        while (!done) {
            if (std::chrono::steady_clock::now() >= deadline)
                throw Failure(TimeoutCode, "Function response timed out; requestSent=unknown; inspect before retrying", 2);
            changed.wait_until(lock, deadline);
        }
        if (failure) throw Failure(failure, error, 1);
        return value;
    }
};

#ifdef _WIN32
// Read foreign bytes through Windows' fault-contained copy, never through a
// std::string method on the engine's pointer. Only helper-owned copies are read.
inline bool ReadCallbackBytes(const void* from, void* to, std::size_t size) noexcept {
    SIZE_T copied = 0;
    return ReadProcessMemory(GetCurrentProcess(), from, to, size, &copied) && copied == size;
}
__declspec(noinline) inline bool InspectCallbackString(const std::string* view,
    std::size_t* size, std::size_t* capacity, const char** data) noexcept {
    __try {
        *size = view->size(); *capacity = view->capacity(); *data = view->data();
        return true;
    } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}
enum class StringCopy { Copied, Invalid, TooLarge };
inline StringCopy CopyCallbackString(const void* source, std::string& result, std::size_t& actualSize) {
    // This is the helper toolchain's ABI, not an offset inferred from Studio.
    // The snapshot owns no engine allocation and is deliberately not destroyed.
    union Snapshot {
        std::string value;
        Snapshot() : value() {}
        ~Snapshot() {}
    } snapshot;
    static_assert(_ITERATOR_DEBUG_LEVEL == 0);
    const auto inlineCapacity = snapshot.value.capacity();
    const auto inlineData = snapshot.value.data();
    const auto maximum = snapshot.value.max_size();
    if (!ReadCallbackBytes(source, &snapshot.value, sizeof(snapshot.value))) return StringCopy::Invalid;
    std::size_t size = 0, capacity = 0;
    const char* data = nullptr;
    if (!InspectCallbackString(&snapshot.value, &size, &capacity, &data)
        || size > capacity || capacity > maximum) return StringCopy::Invalid;
    if (capacity <= inlineCapacity) {
        if (capacity != inlineCapacity || data != inlineData) return StringCopy::Invalid;
    } else {
        const auto pointer = reinterpret_cast<std::uintptr_t>(data);
        const auto object = reinterpret_cast<std::uintptr_t>(source);
        const auto local = reinterpret_cast<std::uintptr_t>(&snapshot.value);
        if (!pointer || pointer - object < sizeof(snapshot.value)
            || pointer - local < sizeof(snapshot.value)) return StringCopy::Invalid;
    }
    actualSize = size;
    if (size > MaxResponse) return StringCopy::TooLarge;
    result.resize(size + 1);
    if (!ReadCallbackBytes(data, result.data(), result.size()) || result[size] != '\0') return StringCopy::Invalid;
    result.resize(size);
    return StringCopy::Copied;
}

// The command result and engine lifetime are separate. Timeout/task disposal
// may destroy Completion, but every engine callback copy owns RequestPins.
struct RequestPins;
inline std::mutex completionsMutex;
inline std::unordered_map<std::uint64_t, std::weak_ptr<RequestPins>> completions;
inline std::uint64_t nextCompletion = 1;
struct RequestPins {
    std::uint64_t token = 0;
    std::mutex mutex;
    std::shared_ptr<void> receiverHold;
    std::weak_ptr<Completion> result;
    Sites sites;
    bool Proved(void* const* frames) const {
        for (const auto& site : sites)
            if (site[0] == reinterpret_cast<std::uintptr_t>(frames[0]) && site[1] == reinterpret_cast<std::uintptr_t>(frames[1])) return true;
        return false;
    }
    void Unregister() {
        std::lock_guard lock(completionsMutex);
        completions.erase(token);
    }
    void Answered() {
        Unregister();
        std::lock_guard lock(mutex);
        receiverHold.reset();
    }
    ~RequestPins() { Unregister(); }
};
inline std::shared_ptr<RequestPins> RegisterCompletion(const std::shared_ptr<Completion>& result, std::shared_ptr<void> hold = {}, Sites sites = {}) {
    auto request = std::make_shared<RequestPins>();
    request->receiverHold = std::move(hold); request->result = result; request->sites = std::move(sites);
    std::lock_guard lock(completionsMutex);
    if (!nextCompletion || nextCompletion > UINT64_MAX / 2)
        throw Failure(LimitCode, "yield-completion-limit: token space exhausted; requestSent=false");
    if (completions.size() >= 128)
        throw Failure(LimitCode, "yield-completion-limit: 128 engine requests still pending; requestSent=false");
    request->token = nextCompletion++;
    completions.emplace(request->token, request);
    return request;
}
struct Callback {
    struct Capture { std::shared_ptr<RequestPins> request; std::uint64_t key; };
    alignas(Capture) unsigned char capture[24]{};
    std::uintptr_t tagged;
    struct alignas(8) Operations {
        void(*invoke)(const Callback*, const void*);
        void(*copy)(void*, const void*);
        void(*move)(void*, void*);
        void(*destroy)(void*);
    };
    static void Destroy(void* data) noexcept {
        std::uint64_t key = 0;
        std::memcpy(&key, static_cast<char*>(data) + offsetof(Capture, key), sizeof(key));
        if (key) { static_cast<Capture*>(data)->~Capture(); std::memset(data, 0, sizeof(Capture)); }
    }
    // The value is read as a string only when the engine came here through a
    // proved site; an answer from any other place is reported, never decoded.
    __declspec(noinline) static void Invoke(const Callback* data, const void* value) noexcept {
        void* frames[2]{};
        const bool located = RtlCaptureStackBackTrace(1, 2, frames, nullptr) == 2 && frames[0] == _ReturnAddress();
        try {
            std::uint64_t key = 0;
            if (!ReadCallbackBytes(reinterpret_cast<const char*>(data) + offsetof(Capture, key), &key, sizeof(key))) return;
            std::shared_ptr<RequestPins> request;
            { std::lock_guard lock(completionsMutex);
              const auto it = completions.find(key >> 1);
              if (it != completions.end()) request = it->second.lock(); }
            if (!request) return;
            if (auto result = request->result.lock()) {
                if (!located || !request->Proved(frames)) {
                    result->Fail("yield-callback-site detector: requestSent=true; the answer came from a call site whose string argument is unproved");
                    request->Answered();
                    return;
                }
                std::string text;
                std::size_t actualSize = 0;
                const auto copied = CopyCallbackString(value, text, actualSize);
                if (copied == StringCopy::Invalid)
                    result->Fail("yield-callback-string detector: requestSent=true; invalid or unreadable string payload");
                else if (copied == StringCopy::TooLarge) result->Fail(TooLargeText(actualSize), SizeCode);
                else result->Finish(std::move(text), (key & 1) != 0);
            }
            request->Answered();
        } catch (...) { /* Engine copies still own the pins on allocation failure. */ }
    }
    inline static const Operations operations{
        Invoke,
        [](void* to, const void* from) { new(to) Capture(*static_cast<const Capture*>(from)); },
        [](void* to, void* from) { new(to) Capture(std::move(*static_cast<Capture*>(from))); },
        Destroy};
    Callback(const std::shared_ptr<RequestPins>& request, bool success) :
        tagged(reinterpret_cast<std::uintptr_t>(&operations) | 6) {
        new(capture) Capture{request, (request->token << 1) | success};
    }
};
static_assert(sizeof(Callback) == 32 && sizeof(Callback::Capture) == 24);

struct YieldInvocation {
    const Input* input; void* target;
    const std::string* first; const std::string* second;
    Callback* success; Callback* error;
};
inline void InvokeYield(YieldInvocation* call) {
    const auto& input = *call->input;
    // MSVC passes these 32-byte, by-value aggregates indirectly. Pass our
    // explicit storage so engine destruction and fault cleanup share one copy.
    if (input.mode == 1) {
        using Function = void(*)(void*, std::string, int, int, Callback*, Callback*);
        reinterpret_cast<Function>(input.function)(call->target, *call->first, input.enums[0], input.enums[1], call->success, call->error);
    } else {
        using Function = void(*)(void*, std::string, std::string, int, int, int, Callback*, Callback*);
        reinterpret_cast<Function>(input.function)(call->target, *call->first, *call->second, input.enums[0], input.enums[1], input.enums[2], call->success, call->error);
    }
}
__declspec(noinline) inline DWORD GuardYield(YieldInvocation* call) {
    // Keep SEH below every helper RAII owner. C++ exceptions unwind normally;
    // hardware faults return here, then become an ordinary helper exception.
    __try { InvokeYield(call); return 0; }
    __except (GetExceptionCode() == 0xE06D7363 ? EXCEPTION_CONTINUE_SEARCH : EXCEPTION_EXECUTE_HANDLER) { return GetExceptionCode(); }
}
#endif

template<class Read>
std::shared_ptr<Completion> Begin(void* target, std::shared_ptr<void> hold,
    const char* bytes, std::size_t size, Read read) {
    ValidateCallContract(bytes, size);
    Input input{};
    if (size < sizeof(input)) throw std::runtime_error("Truncated function request");
    std::memcpy(&input, bytes, sizeof(input));
    Sites sites;
    const bool yields = input.mode == 1 || input.mode == 2;
    if (input.mode < 1 || input.mode > 4 || (input.reserved && input.mode == 3) || input.field < 8 || input.field > 65520
        || input.field % 8 || (yields ? !ReadYieldSites(input, bytes, size, sites)
            : size != sizeof(input) + static_cast<std::uint64_t>(input.firstSize) + input.secondSize))
        throw std::runtime_error("Invalid function request");
    std::uint64_t table = 0, function = 0;
#ifdef _WIN32
    std::int32_t adjustment = 0;
#else
    std::int64_t adjustment = 0;
#endif
    if (!read(input.descriptor, &table, sizeof(table)) || table != input.table
        || !read(input.descriptor + input.field, &function, sizeof(function)) || function != input.function
        || !read(input.descriptor + input.field + 8, &adjustment, sizeof(adjustment)) || adjustment != 0)
        throw std::runtime_error("Function binding was replaced before execution");
    const std::string first(bytes + sizeof(input), input.firstSize);
    const std::string second(bytes + sizeof(input) + input.firstSize, input.secondSize);
    auto result = std::make_shared<Completion>();
    if (input.mode == 4) {
        if (first != "RemovePackageModificationPopupDialog"
            || (input.secondSize != 0 && input.secondSize != sizeof(std::uintptr_t)))
            throw PackageFlagRefused("Invalid internal package flag request");
        const auto instance = reinterpret_cast<std::uintptr_t>(target);
        if (input.reserved > instance) throw PackageFlagRefused("Invalid DataModel receiver");
        std::uintptr_t reference = 0, live[2]{};
        std::memcpy(&reference, input.enums, sizeof(reference));
        if (!read(reference, live, sizeof(live)) || live[0] != instance - input.reserved
            || live[1] != reinterpret_cast<std::uintptr_t>(hold.get()))
            throw StaleDataModel("Package flag DataModel is no longer live; retry");
        using Function = bool(*)(void*, std::string);
        const auto get = [&] { return reinterpret_cast<Function>(input.function)(
            reinterpret_cast<void*>(instance - input.reserved), first); };
        bool value;
        if (second.empty()) value = get();
        else {
#ifdef _WIN32
            std::uintptr_t candidate = 0;
            std::memcpy(&candidate, second.data(), sizeof(candidate));
            value = ConfirmPackageFlag(candidate, get, [](std::uintptr_t address) {
                MEMORY_BASIC_INFORMATION region{};
                return VirtualQuery(reinterpret_cast<void*>(address), &region, sizeof(region)) == sizeof(region)
                    && region.State == MEM_COMMIT && region.Type == MEM_IMAGE
                    && region.AllocationBase == GetModuleHandleW(nullptr)
                    && (region.Protect == PAGE_READWRITE || region.Protect == PAGE_WRITECOPY);
            });
#else
            throw std::runtime_error("Package flag candidate verification is unavailable on this platform");
#endif
        }
        result->Finish(value ? "true" : "false", true);
    }
#ifdef _WIN32
    else if (input.mode == 1 || input.mode == 2) {
        auto request = RegisterCompletion(result, std::move(hold), std::move(sites));
        Callback success(request, true), error(request, false);
        struct Cleanup {
            Callback* success; Callback* error;
            ~Cleanup() { Callback::Destroy(success); Callback::Destroy(error); }
        } cleanup{&success, &error};
        YieldInvocation invocation{&input, target, &first, &second, &success, &error};
        try {
            if (GuardYield(&invocation))
                throw Failure(InvocationCode, "yield-invocation: engine fault; requestSent=unknown; inspect before retrying", 2);
        } catch (const Failure&) { request->Unregister(); throw; }
        catch (...) {
            request->Unregister();
            throw Failure(InvocationCode, "yield-invocation: engine exception; requestSent=unknown; inspect before retrying", 2);
        }

    }
#endif
    else {
#ifdef _WIN32
        struct Receiver {};
        using Function = std::string(Receiver::*)(std::string);
        Function function;
        static_assert(sizeof(function) == sizeof(input.function));
        std::memcpy(&function, &input.function, sizeof(function));
        result->Finish((reinterpret_cast<Receiver*>(target)->*function)(first), true);
#else
        using Function = std::string(*)(void*, std::string);
        result->Finish(reinterpret_cast<Function>(input.function)(target, first), true);
#endif
    }
    return result;
}
}
