#include <condition_variable>
#include <mutex>
#ifdef _WIN32
#include <Windows.h>
#include <intrin.h>
#endif

namespace renium_functions {
struct PackageFlagRefused : std::runtime_error { using std::runtime_error::runtime_error; };
struct StaleDataModel : std::runtime_error { using std::runtime_error::runtime_error; };
struct Input {
    std::uint64_t descriptor, table;
    std::uint32_t field, mode;
    std::uint64_t function;
    std::uint32_t enums[3], firstSize, secondSize, reserved;
};
static_assert(sizeof(Input) == 56);

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
    bool done = false;
    std::string value, error;

    void Finish(std::string result, bool success) {
        std::lock_guard lock(mutex);
        if (done) return;
        if (result.size() > 65536) error = "Function response exceeds 64 KiB; request a smaller page";
        else if (success) value = std::move(result);
        else error = result.empty() ? "Studio function failed without an error message" : std::move(result);
        done = true;
        changed.notify_all();
    }

    std::string Wait(std::chrono::milliseconds timeout) {
        std::unique_lock lock(mutex);
        if (!changed.wait_for(lock, timeout, [&] { return done; }))
            throw std::runtime_error("Function response timed out; the call may still complete. Inspect the affected state before retrying");
        if (!error.empty()) throw std::runtime_error(error);
        return value;
    }
};

template<class Read>
std::shared_ptr<Completion> Begin(void* target, std::shared_ptr<void> hold,
    const char* bytes, std::size_t size, Read read) {
    Input input{};
    if (size < sizeof(input)) throw std::runtime_error("Truncated function request");
    std::memcpy(&input, bytes, sizeof(input));
    if (input.mode < 1 || input.mode > 4 || (input.reserved && input.mode != 4) || input.field < 8 || input.field > 65520
        || input.field % 8 || size != sizeof(input) + static_cast<std::uint64_t>(input.firstSize) + input.secondSize)
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
    using Callback = std::function<void(std::string)>;
    Callback success{[result, hold](std::string value) { result->Finish(std::move(value), true); }};
    Callback failure{[result, hold](std::string error) { result->Finish(std::move(error), false); }};
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
    } else if (input.mode == 1) {
        using Function = void(*)(void*, std::string, int, int, Callback, Callback);
        reinterpret_cast<Function>(input.function)(target, first, input.enums[0], input.enums[1], std::move(success), std::move(failure));
    } else if (input.mode == 2) {
        using Function = void(*)(void*, std::string, std::string, int, int, int, Callback, Callback);
        reinterpret_cast<Function>(input.function)(target, first, second, input.enums[0], input.enums[1], input.enums[2], std::move(success), std::move(failure));
    } else {
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
