#include <condition_variable>
#include <mutex>

namespace renium_functions {
struct Input {
    std::uint64_t descriptor, table;
    std::uint32_t field, mode;
    std::uint64_t function;
    std::uint32_t enums[3], firstSize, secondSize, reserved;
};
static_assert(sizeof(Input) == 56);

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
    if (input.mode < 1 || input.mode > 3 || input.reserved || input.field < 0x40 || input.field > 0x200
        || input.field % 8 || size != sizeof(input) + static_cast<std::uint64_t>(input.firstSize) + input.secondSize)
        throw std::runtime_error("Invalid function request");
    std::uint64_t table = 0, function = 0;
    if (!read(input.descriptor, &table, sizeof(table)) || table != input.table
        || !read(input.descriptor + input.field, &function, sizeof(function)) || function != input.function)
        throw std::runtime_error("Function binding was replaced before execution");
    const std::string first(bytes + sizeof(input), input.firstSize);
    const std::string second(bytes + sizeof(input) + input.firstSize, input.secondSize);
    auto result = std::make_shared<Completion>();
    using Callback = std::function<void(std::string)>;
    Callback success{[result, hold](std::string value) { result->Finish(std::move(value), true); }};
    Callback failure{[result, hold](std::string error) { result->Finish(std::move(error), false); }};
    if (input.mode == 1) {
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
