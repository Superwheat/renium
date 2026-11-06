#pragma once
#include <chrono>
#include <memory>
#include <stdexcept>
#include "renium_tagged_callback.h"

namespace renium_property_task {
struct DeadlineExceeded : std::runtime_error {
    using std::runtime_error::runtime_error;
};

inline void CheckDeadline(std::chrono::steady_clock::time_point deadline, const char* reason) {
    if (std::chrono::steady_clock::now() >= deadline) throw DeadlineExceeded(reason);
}

// The caller has validated the queue ABI and retained its exact DataModel.
// Context preparation only validates that retained owner: putting an empty
// callback on the engine queue adds a whole scheduler turn to every probe.
// Reads and writes still enter the queue and revalidate on execution.
template<class Task, class Execute, class Submit>
void Dispatch(const std::shared_ptr<Task>& task, Execute execute, Submit submit) {
    CheckDeadline(task->deadline, "Protected property expired before submission");
    if (task->prepareOnly) {
        execute(task);
        return;
    }
    renium_tagged::Callback callback{[task, execute]() { execute(task); }};
    if (!submit(&callback)) throw std::runtime_error("Studio rejected the protected-property task");
}
}
