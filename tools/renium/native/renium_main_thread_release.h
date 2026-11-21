#pragma once

namespace renium_functions {
// Use the same verified engine queue as the call. No window, message hook,
// installing thread, or arbitrary message-loop reentrancy participates.
class MainThreadRelease {
    struct Pin {
        std::function<bool(std::function<void()>*)> submit;
        void* owner;
        void(*release)(void*);
        std::shared_ptr<void> parent;
        bool held = false;
        bool submitting = false, early = false;
        Pin* next = nullptr;
    };
    struct Pending { std::mutex mutex; Pin* head = nullptr; };
    static Pending& PendingPins() {
        // A refused or discarded drain leaves its pins here until the helper's
        // next engine task of any kind (a property, function or import task)
        // or OS teardown. Engine destructors never run on a worker.
        static auto pending = new Pending;
        return *pending;
    }
    inline static thread_local unsigned inTask = 0;
    std::function<bool(std::function<void()>*)> submit;
    static void Release(Pin* pin) noexcept { pin->release(pin->owner); delete pin; }
    static void SubmitPending() noexcept {
        auto& pending = PendingPins();
        Pin* pin;
        { std::lock_guard lock(pending.mutex);
          pin = pending.head;
          while (pin && pin->submitting) pin = pin->next;
          if (!pin) return;
          pin->submitting = true; pin->early = false; }
        bool accepted = false;
        try {
            std::function<void()> drain = [] { TaskScope task; };
            accepted = pin->submit(&drain);
        } catch (...) {}
        bool retry;
        { std::lock_guard lock(pending.mutex);
          retry = accepted && pin->early;
          pin->submitting = false; }
        // An exceptionally early engine task cannot release the context while
        // SubmitTask still uses it. This worker only resubmits; it never frees.
        if (retry) QueueUserWorkItem([](void*) -> DWORD { SubmitPending(); return 0; }, nullptr, WT_EXECUTEDEFAULT);
    }
    static void Retire(Pin* pin) noexcept {
        if (inTask) { Release(pin); return; }
        auto& pending = PendingPins();
        { std::lock_guard lock(pending.mutex); pin->next = pending.head; pending.head = pin; }
        // The engine may be destroying its callbacks under its own queue lock:
        // enter that queue from a pool thread, never from this frame.
        QueueUserWorkItem([](void*) -> DWORD { SubmitPending(); return 0; }, nullptr, WT_EXECUTEDEFAULT);
    }
public:
    struct TaskScope {
        TaskScope() {
            ++inTask;
            auto& pending = PendingPins();
            Pin* list = nullptr;
            { std::lock_guard lock(pending.mutex);
              auto link = &pending.head;
              while (*link) {
                  auto pin = *link;
                  if (pin->submitting) { pin->early = true; link = &pin->next; }
                  else { *link = pin->next; pin->next = list; list = pin; }
              } }
            while (list) { auto pin = list; list = pin->next; Release(pin); }
        }
        ~TaskScope() { --inTask; }
    };
    explicit MainThreadRelease(std::function<bool(std::function<void()>*)> submit) : submit(std::move(submit)) {}
    std::shared_ptr<void> Hold(void* owner, bool(*add)(void*), void(*release)(void*), std::shared_ptr<void> parent = {}) {
        PendingPins(); // Allocate everything before AddRef.
        auto pin = new Pin{submit, owner, release, std::move(parent)};
        std::shared_ptr<void> hold(owner, [pin](void*) {
            if (pin->held) Retire(pin);
            else delete pin;
        });
        if (!add(owner)) return {};
        pin->held = true;
        return hold;
    }
};
}
