#include <array>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <functional>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>
#include <thread>
#include "../renium_studio_functions.h"
#include "../renium_main_thread_release.h"

// Checks must execute in release/NDEBUG builds too.
static void CheckImpl(bool condition, unsigned line) { if (!condition) { std::fprintf(stderr, "check failed at line %u\n", line); std::abort(); } }
#define Check(condition) CheckImpl((condition), __LINE__)

// Independent raw engine-side ABI. No C++ Callback methods or engine pointers.
struct RawCallback { std::uint64_t capture[3]; std::uintptr_t tagged; };
static RawCallback pending[2]{};
static unsigned calls;
static void KeepCallback(RawCallback* input, RawCallback& copy) {
    Check((input->tagged & 7) == 6);
    const auto table = reinterpret_cast<std::uintptr_t*>(input->tagged & ~std::uintptr_t{7});
    reinterpret_cast<void(*)(void*, const void*)>(table[1])(&copy, input);
    copy.tagged = input->tagged;
    RawCallback moved{};
    reinterpret_cast<void(*)(void*, void*)>(table[2])(&moved, &copy);
    reinterpret_cast<void(*)(void*)>(table[3])(&copy);
    std::memcpy(copy.capture, moved.capture, sizeof(copy.capture));
    reinterpret_cast<void(*)(void*)>(table[3])(input);
    input->tagged = 0;
}
// The engine's invoker and the one function that hands it an answer: the two
// return addresses a callback sees through them are the proved call site.
__declspec(noinline) static void Invoker(RawCallback& callback, const void* payload) {
    const auto table = reinterpret_cast<std::uintptr_t*>(callback.tagged & ~std::uintptr_t{7});
    reinterpret_cast<void(*)(void*, const void*)>(table[0])(&callback, payload);
    reinterpret_cast<void(*)(void*)>(table[3])(&callback);
}
__declspec(noinline) static void Answer(RawCallback& callback, const void* payload) {
    Invoker(callback, payload);
    callback.tagged = 0;
}
static void Deliver(RawCallback& callback, std::string text) { Answer(callback, &text); }
static renium_functions::Sites proved;
__declspec(noinline) static void ProbeInvoke(void*, const void*) {
    void* frames[2]{};
    Check(RtlCaptureStackBackTrace(1, 2, frames, nullptr) == 2);
    proved = {{reinterpret_cast<std::uintptr_t>(frames[0]), reinterpret_cast<std::uintptr_t>(frames[1])}};
}
static void ProbeDestroy(void*) {}
static void Calibrate() {
    alignas(8) static const std::uintptr_t table[4]{reinterpret_cast<std::uintptr_t>(&ProbeInvoke), 0, 0,
        reinterpret_cast<std::uintptr_t>(&ProbeDestroy)};
    RawCallback probe{{}, reinterpret_cast<std::uintptr_t>(table) | 6};
    Answer(probe, nullptr);
    Check(proved.size() == 1 && proved[0][0] && proved[0][1] && proved[0][0] != proved[0][1]);
}
// The proved invoker entered from another function, and another invoker.
static unsigned strays;
__declspec(noinline) static void OtherCaller(RawCallback& callback, const void* payload) {
    Invoker(callback, payload);
    callback.tagged = 0; ++strays;
}
__declspec(noinline) static void OtherInvoker(RawCallback& callback, const void* payload) {
    const auto table = reinterpret_cast<std::uintptr_t*>(callback.tagged & ~std::uintptr_t{7});
    strays += 2;
    reinterpret_cast<void(*)(void*, const void*)>(table[0])(&callback, payload);
    reinterpret_cast<void(*)(void*)>(table[3])(&callback);
    callback.tagged = 0;
}
static void UnprovedSites() {
    const std::string text("answer");
    for (auto deliver : {&OtherCaller, &OtherInvoker}) {
        auto result = std::make_shared<renium_functions::Completion>();
        auto request = renium_functions::RegisterCompletion(result, {}, proved);
        renium_functions::Callback callback(request, true);
        deliver(*reinterpret_cast<RawCallback*>(&callback), &text);
        bool refused = false;
        try { result->Wait(std::chrono::milliseconds(0)); }
        catch (const renium_functions::Failure& error) {
            refused = error.code == renium_functions::PayloadCode && error.sent == 1 && std::strstr(error.what(), "yield-callback-site detector") != nullptr;
        }
        Check(refused && renium_functions::completions.empty());
    }
    Check(strays == 3);
}
static void Get(void* target, std::string url, int priority, int request, RawCallback* success, RawCallback* failure) {
    Check(target == &calls && url == std::string(2048, 'u') && priority == 1 && request == 2);
    ++calls; KeepCallback(success, pending[0]); KeepCallback(failure, pending[1]);
}
static void Post(void* target, std::string url, std::string body, int priority, int content, int request, RawCallback* success, RawCallback* failure) {
    Check(body == std::string(4096, 'b') && content == 3);
    Get(target, std::move(url), priority, request, success, failure);
}
static void GuardedPayloads() {
    auto rejected = [](const void* payload) {
        auto result = std::make_shared<renium_functions::Completion>();
        auto request = renium_functions::RegisterCompletion(result, {}, proved);
        renium_functions::Callback callback(request, true);
        Answer(*reinterpret_cast<RawCallback*>(&callback), payload);
        bool refused = false;
        try { result->Wait(std::chrono::milliseconds(0)); }
        catch (const renium_functions::Failure& error) {
            refused = error.code == renium_functions::PayloadCode && error.sent == 1 && std::strstr(error.what(), "yield-callback-string detector") != nullptr;
        }
        Check(refused);
    };
    rejected(nullptr);
    rejected(reinterpret_cast<void*>(1)); // Changed callback: scalar in RDX.
    std::array<unsigned char, sizeof(std::string)> bytes{};
    rejected(bytes.data()); // A readable tuple is not a valid small string.
    std::string source(80, 'x'); source.reserve(256);
    std::memcpy(bytes.data(), &source, sizeof(source));
    source.resize(81);
    // Discover the helper's size field by changing only size, not by an offset.
    std::size_t fields = 0;
    for (std::size_t at = 0; at + sizeof(std::size_t) <= bytes.size(); at += sizeof(std::size_t)) {
        std::size_t before = 0, after = 0;
        std::memcpy(&before, bytes.data() + at, sizeof(before));
        std::memcpy(&after, reinterpret_cast<const char*>(&source) + at, sizeof(after));
        if (before != 80 || after != 81) continue;
        const auto invalid = source.capacity() + 1;
        std::memcpy(bytes.data() + at, &invalid, sizeof(invalid));
        ++fields;
    }
    Check(fields == 1);
    rejected(bytes.data());
    std::memcpy(bytes.data(), &source, sizeof(source));
    fields = 0;
    for (std::size_t at = 0; at + sizeof(void*) <= bytes.size(); at += sizeof(void*)) {
        const void* pointer = nullptr;
        std::memcpy(&pointer, bytes.data() + at, sizeof(pointer));
        if (pointer != source.data()) continue;
        pointer = reinterpret_cast<void*>(1);
        std::memcpy(bytes.data() + at, &pointer, sizeof(pointer));
        ++fields;
    }
    Check(fields == 1);
    rejected(bytes.data()); // Valid size/capacity, inaccessible heap data.
    for (const auto& text : {std::string(), std::string("HTTP 404"), std::string(65536, 'r')}) {
        std::string copy;
        std::size_t size = 0;
        const auto copied = renium_functions::CopyCallbackString(&text, copy, size);
        Check(copied == renium_functions::StringCopy::Copied && copy == text && size == text.size());
    }
    const std::string tooLarge(65537, 'r');
    auto result = std::make_shared<renium_functions::Completion>();
    auto request = renium_functions::RegisterCompletion(result, {}, proved);
    renium_functions::Callback callback(request, true);
    Answer(*reinterpret_cast<RawCallback*>(&callback), &tooLarge);
    bool sized = false;
    try { result->Wait(std::chrono::milliseconds(0)); }
    catch (const renium_functions::Failure& error) {
        sized = error.code == renium_functions::SizeCode && std::strstr(error.what(), "65537 bytes") != nullptr && std::strstr(error.what(), "too large") != nullptr;
    }
    Check(sized);
}

static void DestroyPending();
static void MainThreadRetirement() {
    const auto mainThread = GetCurrentThreadId();
    std::mutex mutex;
    std::condition_variable changed;
    std::vector<std::function<void()>> tasks;
    bool reject = false;
    renium_functions::MainThreadRelease queue([&](std::function<void()>* work) {
        std::lock_guard lock(mutex);
        if (reject) return false;
        tasks.push_back(*work); changed.notify_all(); return true;
    });
    auto drain = [&] {
        std::vector<std::function<void()>> ready;
        { std::lock_guard lock(mutex); ready.swap(tasks); }
        for (auto& work : ready) work();
    };
    auto queued = [&] {
        std::unique_lock lock(mutex);
        Check(changed.wait_for(lock, std::chrono::seconds(2), [&] { return !tasks.empty(); }));
    };
    struct Owner { unsigned references = 0; DWORD releasedOn = 0; Owner* parent = nullptr; } model, receiver;
    const auto add = +[](void* p) { ++static_cast<Owner*>(p)->references; return true; };
    const auto release = +[](void* p) {
        auto& owner = *static_cast<Owner*>(p);
        Check(!owner.parent || owner.parent->references != 0);
        --owner.references; owner.releasedOn = GetCurrentThreadId();
    };
    // Each submitting thread really exits before Studio runs its queued work.
    for (unsigned call = 0; call < 2; ++call) {
        model = {}; receiver = {}; receiver.parent = &model;
        std::thread worker([&] {
            auto modelHold = queue.Hold(&model, add, release);
            auto receiverHold = queue.Hold(&receiver, add, release, modelHold);
            auto result = std::make_shared<renium_functions::Completion>();
            auto request = renium_functions::RegisterCompletion(result, receiverHold);
            renium_functions::Callback success(request, true), error(request, false);
            KeepCallback(reinterpret_cast<RawCallback*>(&success), pending[0]);
            KeepCallback(reinterpret_cast<RawCallback*>(&error), pending[1]);
        });
        worker.join();
        Check(model.references == 1 && receiver.references == 1);
        std::thread engineFinished([&] {
            if (call == 0) Deliver(pending[0], "late completion");
            DestroyPending(); // Also exercise destruction without an answer.
        });
        engineFinished.join();
        Check(model.references == 1 && receiver.references == 1);
        queued(); drain();
        Check(model.references == 0 && receiver.references == 0);
        Check(model.releasedOn == mainThread && receiver.releasedOn == mainThread);
    }
    // Pins retired from many threads at once all leave on the engine thread.
    std::array<Owner, 16> crowd{};
    { std::vector<std::thread> retiring;
      for (auto& owner : crowd) retiring.emplace_back([&] { auto hold = queue.Hold(&owner, add, release); });
      for (auto& thread : retiring) thread.join(); }
    const auto crowdDeadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    auto held = [&] { unsigned total = 0; for (auto& owner : crowd) total += owner.references; return total; };
    Check(held() == crowd.size());
    while (held()) {
        Check(std::chrono::steady_clock::now() < crowdDeadline);
        { std::unique_lock lock(mutex); changed.wait_for(lock, std::chrono::milliseconds(10), [&] { return !tasks.empty(); }); }
        drain();
    }
    for (auto& owner : crowd) Check(owner.releasedOn == mainThread);
    // A closing/discarded queue cannot run a destructor on the worker. The next
    // valid engine task (even through another queue) drains the retained pins.
    { std::lock_guard lock(mutex); reject = true; }
    model = {};
    std::thread closing([&] { auto hold = queue.Hold(&model, add, release); });
    closing.join(); Check(model.references == 1 && model.releasedOn == 0);
    // A task that meets the pin while its refused submission is still running
    // leaves it for the next task.
    for (unsigned attempt = 0; model.references && attempt < 2000; ++attempt) {
        { renium_functions::MainThreadRelease::TaskScope nextEngineTask; }
        if (model.references) Sleep(1);
    }
    Check(model.references == 0 && model.releasedOn == mainThread);
    { std::lock_guard lock(mutex); reject = false; }
    // Cleanup may be dequeued before the submitter returns: do not destroy the
    // queue context underneath SubmitTask. A thread-pool wake only requeues.
    bool first = true, ran = false; model = {};
    renium_functions::MainThreadRelease early([&](std::function<void()>* work) {
        std::unique_lock lock(mutex);
        const bool wait = first; first = false;
        tasks.push_back(*work); changed.notify_all();
        if (wait) changed.wait(lock, [&] { return ran; }); // The engine thread runs it first.
        return true;
    });
    std::thread racing([&] { auto hold = early.Hold(&model, add, release); });
    racing.join();
    queued(); drain();
    Check(model.references == 1 && model.releasedOn == 0);
    { std::lock_guard lock(mutex); ran = true; }
    changed.notify_all();
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (model.references) {
        { std::unique_lock lock(mutex); Check(changed.wait_until(lock, deadline, [&] { return !tasks.empty(); })); }
        drain();
    }
    Check(model.releasedOn == mainThread);
}
static void DestroyPending() {
    for (auto& callback : pending) if (callback.tagged) {
        const auto table = reinterpret_cast<std::uintptr_t*>(callback.tagged & ~std::uintptr_t{7});
        reinterpret_cast<void(*)(void*)>(table[3])(&callback); callback.tagged = 0;
    }
}
static bool copyBeforeFault;
static void FaultGet(void*, std::string, int, int, RawCallback* success, RawCallback* failure) {
    if (copyBeforeFault) { KeepCallback(success, pending[0]); KeepCallback(failure, pending[1]); }
    RaiseException(EXCEPTION_ACCESS_VIOLATION, 0, 0, nullptr);
}
static std::string Request(renium_functions::Input input, const std::string& first = {}, const std::string& second = {}) {
    input.reserved = renium_functions::NativeStringContract() | 1u << renium_functions::LayoutBits;
    input.firstSize = static_cast<std::uint32_t>(first.size()); input.secondSize = static_cast<std::uint32_t>(second.size());
    std::string bytes(reinterpret_cast<const char*>(&input), sizeof(input));
    bytes += first; bytes += second;
    bytes.append(reinterpret_cast<const char*>(proved.data()), sizeof(proved[0]));
    return bytes;
}
static int YieldCalls() {
    Calibrate();
    MainThreadRetirement();
    GuardedPayloads();
    UnprovedSites();
    for (unsigned mode : {1u, 2u}) for (unsigned outcome = 0; outcome < 4; ++outcome) {
        const std::string url(2048, 'u'), body = mode == 2 ? std::string(4096, 'b') : "";
        const auto function = mode == 1 ? reinterpret_cast<std::uintptr_t>(&Get) : reinterpret_cast<std::uintptr_t>(&Post);
        std::uint64_t descriptor[]{123, function, 0};
        renium_functions::Input input{};
        input.descriptor = reinterpret_cast<std::uintptr_t>(descriptor); input.table = 123;
        input.field = 8; input.function = function; input.mode = mode;
        input.enums[0] = 1; input.enums[1] = mode == 1 ? 2 : 3; input.enums[2] = 2;
        const auto bytes = Request(input, url, body);
        auto owner = std::make_shared<int>(1); std::weak_ptr<int> weak = owner;
        auto result = renium_functions::Begin(&calls, owner, bytes.data(), bytes.size(), [&](std::uintptr_t at, void* out, std::size_t size) {
            if (at < input.descriptor || at + size > input.descriptor + sizeof(descriptor)) return false;
            std::memcpy(out, reinterpret_cast<void*>(at), size); return true;
        });
        owner.reset(); Check(!weak.expired()); // Engine copies, not Wait, own the pins.
        if (outcome < 2) {
            std::thread delivery([outcome] { Deliver(pending[outcome], outcome ? "HTTP 403 " + std::string(60000, 'e') : std::string(8192, 'r')); });
            if (!outcome) {
                const auto response = result->Wait(std::chrono::seconds(1));
                Check(response == std::string(8192, 'r'));
            } else {
                const auto response = result->Wait(std::chrono::seconds(1));
                Check(result->httpError && response == "HTTP 403 " + std::string(60000, 'e'));
            }
            delivery.join();
        } else if (outcome == 2) {
            bool timedOut = false;
            try { result->Wait(std::chrono::milliseconds(0)); }
            catch (const renium_functions::Failure& error) { timedOut = error.code == renium_functions::TimeoutCode; }
            Check(timedOut);
        } // outcome 3 deliberately skips Wait (submission-timeout race).
        result.reset();
        if (outcome >= 2) {
            Check(!weak.expired() && !renium_functions::completions.empty());
            if (outcome == 3) {
                Deliver(pending[0], "late answer after task disposal");
                Check(weak.expired() && renium_functions::completions.empty());
            }
            // Callback destruction without an answer is sufficient, even after
            // the deadline and after the transport task/result have disappeared.
            DestroyPending();
        } else DestroyPending();
        Check(weak.expired() && renium_functions::completions.empty());
    }
    for (bool copy : {false, true}) {
        copyBeforeFault = copy;
        std::uint64_t descriptor[]{123, reinterpret_cast<std::uintptr_t>(&FaultGet), 0};
        renium_functions::Input input{};
        input.descriptor = reinterpret_cast<std::uintptr_t>(descriptor); input.table = 123;
        input.field = 8; input.function = descriptor[1]; input.mode = 1;
        const auto bytes = Request(input);
        auto owner = std::make_shared<int>(1); std::weak_ptr<int> weak = owner;
        bool faulted = false;
        try {
            renium_functions::Begin(&calls, std::move(owner), bytes.data(), bytes.size(),
                [](std::uintptr_t at, void* out, std::size_t size) { std::memcpy(out, reinterpret_cast<void*>(at), size); return true; });
        } catch (const renium_functions::Failure& error) { faulted = error.code == renium_functions::InvocationCode && error.sent == 2; }
        Check(faulted && renium_functions::completions.empty());
        Check(weak.expired() != copy); // Engine copies still require their pins.
        DestroyPending(); Check(weak.expired());
    }
    std::vector<std::shared_ptr<renium_functions::RequestPins>> full;
    for (unsigned i = 0; i < 128; ++i) full.push_back(renium_functions::RegisterCompletion(std::make_shared<renium_functions::Completion>()));
    bool limited = false;
    try { renium_functions::RegisterCompletion(std::make_shared<renium_functions::Completion>()); }
    catch (const renium_functions::Failure& error) { limited = error.code == renium_functions::LimitCode && error.sent == 0; }
    Check(limited); full.clear(); Check(renium_functions::completions.empty());
    Check(calls == 8);
    return 0;
}

struct Receiver {
    std::array<std::uint64_t, 4> identity{11, 22, 33, 44};
    __declspec(noinline) std::string Documentation(std::string name) {
        return "https://developer.roblox.com/" + name;
    }
};

static bool catches_package_getter_fault(unsigned char* cell, unsigned* reads, unsigned at) {
    const std::uintptr_t address = reinterpret_cast<std::uintptr_t>(cell);
    __try {
        renium_functions::ConfirmPackageFlag(address, [=] {
            if (++*reads == at) return *reinterpret_cast<volatile unsigned char*>(1) != 0;
            return *cell == 1;
        }, [](std::uintptr_t) { return true; });
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return GetExceptionCode() == EXCEPTION_ACCESS_VIOLATION;
    }
    return false;
}

bool package_flag_confirmation() {
    unsigned char cell = 0;
    const auto candidate = reinterpret_cast<std::uintptr_t>(&cell);
    auto writable = [](std::uintptr_t) { return true; };
    auto rejects = [&](auto get, auto allowed, const auto& inputs) {
        try { renium_functions::ConfirmPackageFlag(inputs, get, allowed); }
        catch (const std::runtime_error&) { return true; }
        return false;
    };
    unsigned reads = 0;
    if (!rejects([&] { ++reads; return false; }, writable, candidate) || cell != 0 || reads != 2) return false;
    reads = 0;
    if (!rejects([&]() -> bool { if (++reads == 2) throw std::runtime_error("getter refused"); return false; }, writable, candidate) || cell != 0) return false;
    reads = 0;
    if (!rejects([&] { ++reads; return false; }, [](std::uintptr_t) { return false; }, candidate) || cell != 0 || reads != 1) return false;
    reads = 0;
    cell = 1;
    if (!rejects([] { return false; }, writable, candidate) || cell != 1) return false;
    cell = 0;
    reads = 0;
    // One coincidental true answer must not confirm the candidate.
    if (!rejects([&] { return ++reads > 1; }, writable, candidate) || cell != 0 || reads != 3) return false;
    reads = 0;
    if (!rejects([&] { ++reads; return reads == 2; }, writable, candidate) || cell != 0 || reads != 4) return false;
    // A concurrent change must not be overwritten by restoration.
    reads = 0;
    if (!rejects([&] { if (++reads == 2) cell = 2; return false; }, writable, candidate) || cell != 2) return false;
    for (unsigned fault : {2, 3, 4}) {
        cell = 0; reads = 0;
        if (!catches_package_getter_fault(&cell, &reads, fault) || cell != 0 || reads != fault) return false;
    }
    cell = 0; reads = 0;
    if (!renium_functions::ConfirmPackageFlag(candidate, [&] { ++reads; return cell == 1; }, writable) || cell != 1 || reads != 4) return false;
    return true;
}

// What the helper reads from a request the daemon encoded.
static int Decode(const char* hex) {
    std::string bytes;
    for (; hex[0] && hex[1]; hex += 2) {
        unsigned value = 0;
        if (sscanf_s(hex, "%2x", &value) != 1) return 5;
        bytes.push_back(static_cast<char>(value));
    }
    renium_functions::Input input{};
    if (bytes.size() < sizeof(input)) return 5;
    std::memcpy(&input, bytes.data(), sizeof(input));
    try { renium_functions::ValidateCallContract(bytes.data(), bytes.size()); }
    catch (const std::exception&) { std::puts("refused"); return 0; }
    renium_functions::Sites sites;
    if (!renium_functions::ReadYieldSites(input, bytes.data(), bytes.size(), sites)) return 5;
    std::printf("mode=%u first=%.*s second=%.*s sites=", input.mode,
        static_cast<int>(input.firstSize), bytes.data() + sizeof(input),
        static_cast<int>(input.secondSize), bytes.data() + sizeof(input) + input.firstSize);
    for (const auto& site : sites) std::printf("%llx:%llx,", static_cast<unsigned long long>(site[0]), static_cast<unsigned long long>(site[1]));
    std::puts("");
    return 0;
}

int main(int argc, char** argv) {
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    _set_error_mode(_OUT_TO_STDERR);
    _set_abort_behavior(0, _CALL_REPORTFAULT);
    if (argc > 1 && !std::strcmp(argv[1], "--contract")) { std::printf("%u\n", renium_functions::NativeStringContract()); return 0; }
    if (argc > 2 && !std::strcmp(argv[1], "--decode")) return Decode(argv[2]);
    if (argc > 1) return YieldCalls();
    // Refuse even a stale/direct helper request before any engine memory read:
    // a wrong layout, and the right layout with no proved site or a missing one.
    const auto layout = renium_functions::NativeStringContract();
    for (unsigned mode : {1u, 2u}) for (auto contract : {0u, 2u, layout ^ (1u << 8), layout, layout | 1u << renium_functions::LayoutBits,
            (layout ^ (1u << 8)) | 1u << renium_functions::LayoutBits}) {
        renium_functions::Input input{};
        input.mode = mode;
        input.reserved = contract;
        input.function = 1;
        unsigned reads = 0;
        bool refused = false;
        try {
            renium_functions::Begin(nullptr, {}, reinterpret_cast<const char*>(&input), sizeof(input),
                [&](std::uintptr_t, void*, std::size_t) { ++reads; return false; });
        } catch (const std::runtime_error& error) {
            refused = std::strstr(error.what(), "yield-call-contract") != nullptr;
        }
        if (!refused || reads) return 4;
    }
    if (!package_flag_confirmation()) return 3;
    Receiver receiver;
    const auto identity = receiver.identity;
    auto member = &Receiver::Documentation;
    std::uint64_t address = 0;
    static_assert(sizeof(member) == sizeof(address));
    std::memcpy(&address, &member, sizeof(address));
    std::array<std::uint64_t, 128> descriptor{};
    descriptor[0] = 123;
    for (unsigned field : {16, 64, 768}) {
    descriptor[field / 8] = address;
    for (unsigned index = 0; index < 32; ++index) {
        const std::string name(index % 2 ? 100 : 3, 'x');
        renium_functions::Input input{};
        input.descriptor = reinterpret_cast<std::uintptr_t>(descriptor.data());
        input.table = descriptor[0];
        input.field = field;
        input.mode = 3;
        input.function = address;
        input.firstSize = static_cast<std::uint32_t>(name.size());
        std::vector<char> bytes(sizeof(input) + name.size());
        std::memcpy(bytes.data(), &input, sizeof(input));
        std::memcpy(bytes.data() + sizeof(input), name.data(), name.size());
        auto read = [&](std::uint64_t pointer, void* output, std::size_t size) {
                if (pointer < input.descriptor || pointer + size > input.descriptor + sizeof(descriptor)) return false;
                std::memcpy(output, reinterpret_cast<void*>(pointer), size);
                return true;
            };
        auto completion = renium_functions::Begin(&receiver, {}, bytes.data(), bytes.size(), read);
        if (completion->Wait(std::chrono::milliseconds(1)) != "https://developer.roblox.com/" + name
            || receiver.identity != identity) {
            std::fprintf(stderr, "Function return changed the receiver or response at call %u\n", index);
            return 1;
        }
        descriptor[field / 8 + 1] = 1;
        bool refused = false;
        try { renium_functions::Begin(&receiver, {}, bytes.data(), bytes.size(), read); }
        catch (const std::runtime_error&) { refused = true; }
        descriptor[field / 8 + 1] = 0;
        if (!refused || receiver.identity != identity) return 2;
    }
    }
    return 0;
}
