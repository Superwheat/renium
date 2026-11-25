#define RENIUM_FPS_NO_DLLMAIN
#include "../renium_fps_windows.cpp"
#include <cstdlib>

#define CHECK(cond)                                                              \
    do {                                                                         \
        if (!(cond)) {                                                           \
            std::fprintf(stderr, "CHECK failed at %s:%d: %s\n", __FILE__,        \
                         __LINE__, #cond);                                       \
            std::exit(1);                                                        \
        }                                                                        \
    } while (0)

namespace {
struct FakeRenderSchedulerState {
    void** vtable = nullptr;
    std::uint8_t pad[0x60]{};
    std::int32_t frame_counter = 0;
    std::int32_t suppress_count = 0;
    std::int32_t skip_count = 0;
    std::int32_t throttle_count = 0;
};

void* g_last_a3 = nullptr;
void* g_last_a4 = nullptr;

bool __fastcall FakeShouldSkipRender(void* raw_self, void*, void* a3, void* a4) {
    g_last_a3 = a3;
    g_last_a4 = a4;
    auto* self = static_cast<FakeRenderSchedulerState*>(raw_self);
    self->frame_counter = (self->frame_counter + 1) % 4;
    if (self->suppress_count > 0 || self->skip_count > 0) {
        return true;
    }
    if (self->throttle_count > 0 && self->frame_counter >= 1) {
        return true;
    }
    return false;
}

void DummyFn() {}

void* g_fake_vtable[4] = {
    nullptr,
    reinterpret_cast<void*>(&FakeShouldSkipRender),
    nullptr,
    nullptr,
};

Parameters MakeParams(unsigned action) {
    Parameters params{};
    params.size = sizeof(Parameters);
    params.version = 1;
    params.action = action;
    params.frame_counter_offset = offsetof(FakeRenderSchedulerState, frame_counter);
    params.throttle_count_offset = offsetof(FakeRenderSchedulerState, throttle_count);
    params.reset_counter_value = 3;
    params.vtable_slot_addr = reinterpret_cast<std::uintptr_t>(&g_fake_vtable[1]);
    params.expected_should_skip_addr = reinterpret_cast<std::uintptr_t>(&FakeShouldSkipRender);
    return params;
}
}

int main() {
    // 1. Policy checks.
    CHECK(renium_fps::Unlocked(renium_fps::kActionUnlock, 5000, 1000));
    CHECK(!renium_fps::Unlocked(renium_fps::kActionUnlock, 5000, 5000));
    CHECK(!renium_fps::Unlocked(renium_fps::kActionDefault, 5000, 1000));

    FakeRenderSchedulerState rss{};
    rss.vtable = g_fake_vtable;
    rss.throttle_count = 1;

    // Baseline: with throttle_count == 1, 3 of every 4 frames are skipped.
    int rendered = 0;
    for (int i = 0; i < 8; ++i) {
        auto fn = reinterpret_cast<ShouldSkipRenderFn>(rss.vtable[1]);
        const bool skipped = fn(&rss, nullptr, nullptr, nullptr);
        if (!skipped) ++rendered;
    }
    CHECK(rendered == 2);

    // 2. Refusal tests: unexpected vtable entry or invalid modulus must change nothing.
    {
        auto bad = MakeParams(renium_fps::kActionUnlock);
        bad.expected_should_skip_addr = reinterpret_cast<std::uintptr_t>(&DummyFn);
        const DWORD rc = ReniumFpsStep(&bad);
        CHECK(rc == 0);
        CHECK(bad.status == 0);
        CHECK(g_fake_vtable[1] == reinterpret_cast<void*>(&FakeShouldSkipRender));
    }
    {
        auto bad = MakeParams(renium_fps::kActionUnlock);
        bad.reset_counter_value = 0;
        const DWORD rc = ReniumFpsStep(&bad);
        CHECK(rc == 0);
        CHECK(bad.status == 0);
        CHECK(g_fake_vtable[1] == reinterpret_cast<void*>(&FakeShouldSkipRender));
    }

    // 3. Unlock leaves unthrottled (focused) counter untouched and bypasses focus-loss throttle.
    {
        auto unlock = MakeParams(renium_fps::kActionUnlock);
        const DWORD rc = ReniumFpsStep(&unlock);
        CHECK(rc == 0);
        CHECK(unlock.status == 1 && unlock.unlocked == 1 && unlock.hook_installed == 1);
        CHECK(g_fake_vtable[1] == reinterpret_cast<void*>(&HookedShouldSkipRender));

        // When throttle_count == 0 (focused), the counter must advance normally (never reset)
        // and unthrottled_frames must stay 0.
        rss.throttle_count = 0;
        rss.frame_counter = 0;
        auto fn = reinterpret_cast<ShouldSkipRenderFn>(rss.vtable[1]);
        void* const sentinel3 = reinterpret_cast<void*>(0x1234);
        void* const sentinel4 = reinterpret_cast<void*>(0x5678);
        const bool s1 = fn(&rss, nullptr, sentinel3, sentinel4);
        CHECK(!s1 && rss.frame_counter == 1);
        CHECK(g_last_a3 == sentinel3 && g_last_a4 == sentinel4);
        const bool s2 = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(!s2 && rss.frame_counter == 2);
        const bool s3 = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(!s3 && rss.frame_counter == 3);
        {
            auto st = MakeParams(renium_fps::kActionStatus);
            const DWORD st_rc = ReniumFpsStep(&st);
            CHECK(st_rc == 0 && st.status == 1);
            CHECK(st.unthrottled_frames == 0);
            CHECK(st.last_throttle_count == 0);
        }

        // Throttled (unfocused): all 16 frames render, and unthrottled_frames counts all 16.
        rss.throttle_count = 1;
        rendered = 0;
        for (int i = 0; i < 16; ++i) {
            const bool skipped = fn(&rss, nullptr, nullptr, nullptr);
            if (!skipped) ++rendered;
        }
        CHECK(rendered == 16);

        // Late deactivation (throttle_count rising after unlock) stays unlocked.
        rss.throttle_count = 2;
        rendered = 0;
        for (int i = 0; i < 8; ++i) {
            const bool skipped = fn(&rss, nullptr, nullptr, nullptr);
            if (!skipped) ++rendered;
        }
        CHECK(rendered == 8);
        {
            auto st = MakeParams(renium_fps::kActionStatus);
            const DWORD st_rc = ReniumFpsStep(&st);
            CHECK(st_rc == 0 && st.status == 1);
            CHECK(st.unthrottled_frames == 24);
            CHECK(st.last_throttle_count == 2);
        }
        rss.throttle_count = 1;

        // suppress_count and skip_count remain honored while unlocked and must NOT increment
        // unthrottled_frames because no frame was rendered.
        rss.suppress_count = 1;
        const bool sup_skipped = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(sup_skipped);
        rss.suppress_count = 0;
        rss.skip_count = 1;
        const bool skip_skipped = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(skip_skipped);
        rss.skip_count = 0;
        {
            auto st = MakeParams(renium_fps::kActionStatus);
            const DWORD st_rc = ReniumFpsStep(&st);
            CHECK(st_rc == 0 && st.status == 1);
            CHECK(st.unthrottled_frames == 24);
        }
        const bool normal_skipped = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(!normal_skipped);
        {
            auto st = MakeParams(renium_fps::kActionStatus);
            const DWORD st_rc = ReniumFpsStep(&st);
            CHECK(st_rc == 0 && st.status == 1);
            CHECK(st.unthrottled_frames == 25);
        }

        // Multiple scheduler objects: a second unthrottled object must not clobber the active
        // throttle count reported by status.
        FakeRenderSchedulerState rss_other{};
        rss_other.vtable = g_fake_vtable;
        rss_other.throttle_count = 0;
        const bool other_skipped = fn(&rss_other, nullptr, nullptr, nullptr);
        CHECK(!other_skipped);
        {
            auto st = MakeParams(renium_fps::kActionStatus);
            const DWORD st_rc = ReniumFpsStep(&st);
            CHECK(st_rc == 0 && st.status == 1);
            CHECK(st.last_throttle_count == 1);
        }
        rss.throttle_count = 0;
        const bool unthrottled_again = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(!unthrottled_again);
        {
            auto st = MakeParams(renium_fps::kActionStatus);
            const DWORD st_rc = ReniumFpsStep(&st);
            CHECK(st_rc == 0 && st.status == 1);
            CHECK(st.last_throttle_count == 0);
        }
        rss.throttle_count = 1;
    }

    // 4. Default restores the vtable slot and focus-loss throttling completely.
    {
        auto restore = MakeParams(renium_fps::kActionDefault);
        const DWORD rc = ReniumFpsStep(&restore);
        CHECK(rc == 0);
        CHECK(restore.status == 1 && restore.unlocked == 0 && restore.hook_installed == 0);
        CHECK(g_fake_vtable[1] == reinterpret_cast<void*>(&FakeShouldSkipRender));

        rss.frame_counter = 0;
        rendered = 0;
        for (int i = 0; i < 8; ++i) {
            auto fn = reinterpret_cast<ShouldSkipRenderFn>(rss.vtable[1]);
            const bool skipped = fn(&rss, nullptr, nullptr, nullptr);
            if (!skipped) ++rendered;
        }
        CHECK(rendered == 2);
    }

    // 5. Lease expiry stops bypassing immediately; status is read-only (does NOT unhook),
    // and explicit default restores the vtable slot.
    {
        auto unlock = MakeParams(renium_fps::kActionUnlock);
        const DWORD rc = ReniumFpsStep(&unlock);
        CHECK(rc == 0);
        CHECK(unlock.status == 1 && unlock.unlocked == 1);

        GetState().deadline.store(0, std::memory_order_release);
        rss.frame_counter = 0;
        auto fn = reinterpret_cast<ShouldSkipRenderFn>(rss.vtable[1]);
        const bool expired_skipped = fn(&rss, nullptr, nullptr, nullptr);
        CHECK(expired_skipped);

        auto status = MakeParams(renium_fps::kActionStatus);
        const DWORD st_rc = ReniumFpsStep(&status);
        CHECK(st_rc == 0);
        CHECK(status.status == 1 && status.unlocked == 0 && status.hook_installed == 1);
        CHECK(g_fake_vtable[1] == reinterpret_cast<void*>(&HookedShouldSkipRender));

        auto restore = MakeParams(renium_fps::kActionDefault);
        const DWORD def_rc = ReniumFpsStep(&restore);
        CHECK(def_rc == 0);
        CHECK(restore.status == 1 && restore.unlocked == 0 && restore.hook_installed == 0);
        CHECK(g_fake_vtable[1] == reinterpret_cast<void*>(&FakeShouldSkipRender));
    }

    return 0;
}
