#include <cassert>
#include <cstring>
#include <vector>
#include "../renium_property_task.h"

using namespace renium_property_task;
using Clock = std::chrono::steady_clock;

// The same tagged move/invoke/destroy ABI used by the engine. Hold the callback
// without running it to model a loaded Studio queue, with no sleeps or Studio.
struct Queued {
    alignas(8) unsigned char bytes[32]{};
    struct Operations {
        void(*invoke)(void*);
        void(*copy)(void*, const void*);
        void(*move)(void*, void*);
        void(*destroy)(void*);
    };
    const Operations* operations;
    explicit Queued(renium_tagged::Callback* callback) {
        auto raw = reinterpret_cast<unsigned char*>(callback);
        std::uintptr_t tag;
        std::memcpy(&tag, raw + 24, 8);
        assert((tag & 7) == 6);
        operations = reinterpret_cast<const Operations*>(tag & ~std::uintptr_t{7});
        operations->move(bytes, raw);
        operations->destroy(raw);
        std::memset(raw + 24, 0, 8);
    }
    ~Queued() { operations->destroy(bytes); }
    void run() { operations->invoke(bytes); }
};

struct Task {
    bool prepareOnly;
    Clock::time_point deadline = Clock::now() + std::chrono::seconds(30);
    unsigned executions = 0;
};

int main() {
    std::vector<std::unique_ptr<Queued>> queue;
    const auto execute = [](const std::shared_ptr<Task>& task) {
        CheckDeadline(task->deadline, "expired before execution");
        ++task->executions;
    };
    const auto submit = [&](renium_tagged::Callback* callback) {
        queue.push_back(std::make_unique<Queued>(callback));
        return true;
    };

    auto preparation = std::make_shared<Task>(Task{true});
    Dispatch(preparation, execute, submit);
    assert(preparation->executions == 1 && queue.empty());
    // Actual identity capture, property reads, and mutations all use this path.
    auto capture = std::make_shared<Task>(Task{false});
    Dispatch(capture, execute, submit);
    assert(capture->executions == 0 && queue.size() == 1);
    // A blocked capture cannot delay another context-only probe.
    Dispatch(preparation, execute, submit);
    assert(preparation->executions == 2 && queue.size() == 1);
    queue.front()->run();
    assert(capture->executions == 1);
    std::weak_ptr<Task> retained = capture;
    capture.reset();
    assert(!retained.expired());
    queue.clear();
    assert(retained.expired());

    for (bool prepareOnly : {true, false}) {
        auto expired = std::make_shared<Task>(Task{prepareOnly, Clock::time_point::min()});
        bool refused = false;
        try { Dispatch(expired, execute, submit); }
        catch (const DeadlineExceeded&) { refused = true; }
        assert(refused && !expired->executions && queue.empty());
    }
    auto late = std::make_shared<Task>(Task{false});
    Dispatch(late, execute, submit);
    late->deadline = Clock::time_point::min();
    bool refused = false;
    try { queue.front()->run(); }
    catch (const DeadlineExceeded&) { refused = true; }
    assert(refused && !late->executions);
    queue.clear();

    auto rejected = std::make_shared<Task>(Task{false});
    refused = false;
    try { Dispatch(rejected, execute, [](renium_tagged::Callback*) { return false; }); }
    catch (const std::runtime_error&) { refused = true; }
    assert(refused && !rejected->executions);
}
