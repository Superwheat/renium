#pragma once
namespace continuation_test {
// Independent raw consumer, matching the machine-level contract observed in
// Studio. Keep captures after Submit returns to exercise deferred completion.
struct alignas(8) Raw { unsigned char capture[24]; std::uintptr_t operations; };
inline Raw queued{};
inline void Destroy(Raw& value) {
    if (!value.operations) return;
    auto table = reinterpret_cast<void(**)(void*)>(value.operations & ~std::uintptr_t{7});
    table[3](value.capture); value.operations = 0;
}
inline bool Submit(void*, void* incoming, std::uint32_t mode) {
    assert(mode == 1 && !queued.operations);
    const auto& value = *static_cast<Raw*>(incoming);
    assert((value.operations & 7) == 6);
    auto table = reinterpret_cast<void(**)(void*, const void*)>(value.operations & ~std::uintptr_t{7});
    table[1](queued.capture, value.capture);
    queued.operations = value.operations;
    return true;
}
inline void Run() {
    using namespace renium_continuation;
    taskAbi.store(Abi::Tagged);
    for (unsigned n = 0; n < 100; ++n) {
        auto owner = std::make_shared<std::string>(4096, 'q');
        std::weak_ptr<std::string> weak = owner;
        unsigned calls = 0;
        std::function<void()> work = [owner, &calls] { assert(owner->size() == 4096); ++calls; };
        assert(SubmitTask(reinterpret_cast<std::uintptr_t>(&Submit), nullptr, &work, 1));
        assert(!work);
        owner.reset(); assert(!weak.expired() && calls == 0);
        auto table = reinterpret_cast<void(**)(void*)>(queued.operations & ~std::uintptr_t{7});
        table[0](queued.capture);
        Destroy(queued);
        assert(weak.expired() && calls == 1);
        // Copy/move a large captured failure and preserve an independent copy
        // across source destruction and an exception thrown by the callback.
        Function<void(std::string)> error(std::function<void(std::string)>([text = std::string(4096, 'e')](std::string value) {
            assert(value == text); throw std::runtime_error("expected callback failure");
        }));
        auto copy = error; auto moved = std::move(error); assert(!error);
        moved = {};
        bool caught = false;
        try { copy(std::string(4096, 'e')); } catch (const std::runtime_error&) { caught = true; }
        assert(caught);
    }
    taskAbi.store(Abi::Unknown);
    std::function<void()> work = [] { assert(false); };
    bool refused = false;
    try { SubmitTask(reinterpret_cast<std::uintptr_t>(&Submit), nullptr, &work, 1); }
    catch (const std::runtime_error&) { refused = true; }
    assert(refused && !queued.operations);
    // A forged heap tag must fail before reading its pointer or size.
    Function<void()> heap;
    Raw raw{}; raw.operations = reinterpret_cast<std::uintptr_t>(&queued) | 4;
    std::memset(raw.capture, 0xff, sizeof(raw.capture));
    std::memcpy(&heap, &raw, sizeof(raw));
    refused = false;
    try { auto copy = heap; } catch (const std::runtime_error&) { refused = true; }
    std::memset(&heap, 0, sizeof(heap));
    assert(refused);
    taskAbi.store(Abi::Msvc);
}
}
