#include <cassert>
#include <cstring>
#include <string>
#include "../renium_task_context_macos.h"
#include "../renium_tagged_callback.h"
#include "macos-task-context-fixture.h"

using namespace renium_task_context;
int main() {
    assert(TaggedMove(Move));
    assert(TaggedDestroy(Destroy));
    auto dispatch = ManagerDispatch(Manager, 0x104b03e30);
    assert(dispatch && dispatch->modelOffset == 0x20 && dispatch->move == 0x100010f6c && dispatch->destroy == 0x100008080);
    assert(ModelDispatch(Queue, *dispatch));
    auto relocated = Manager;
    relocated[5] = (relocated[5] & ~0x3ffc00u) | (0x180 / 8 << 10);
    auto changed = ManagerDispatch(relocated, 0x204b03e30);
    assert(changed && changed->modelOffset == 0x180 && changed->move == 0x200010f6c);
    // Saved-register allocation is incidental; rename x19/x20 throughout.
    auto rename = [](std::vector<std::uint32_t> code) {
        for (auto& w : code) {
            for (const unsigned shift : {0u, 5u, 10u, 16u}) {
                // Only fields actually used as registers by these instructions.
                const bool pair = (w & 0x3a000000) == 0x28000000;
                const bool mov = (w & 0xffe0ffe0) == 0xaa0003e0;
                if ((shift == 10 && !pair) || (shift == 16 && !mov)) continue;
                const auto reg = (w >> shift) & 31;
                if (reg == 19 || reg == 20) w = (w & ~(31u << shift)) | ((reg + 2) << shift);
            }
        }
        return code;
    };
    assert(ManagerDispatch(rename(Manager), 0x104b03e30));
    assert(TaggedMove(rename(Move)));
    assert(TaggedDestroy(rename(Destroy)));
    auto bad = Manager;
    bad[11] = 0xaa1303e1; // queue receives mode in callback argument
    assert(!ManagerDispatch(bad, 0x104b03e30));
    bad = Move;
    for (auto& w : bad) if ((w & 0xffc003ff) == 0xf9400108 && ((w >> 10) & 4095) == 2) w += 1 << 10;
    assert(!TaggedMove(bad)); // copy in place of move would leak the capture
    bad = Destroy;
    for (auto& w : bad) if ((w & 0xffc00000) == 0xf9000000 && (w & 31) == 31) w = 0xd503201f;
    assert(!TaggedDestroy(bad)); // source tag must be cleared before destroy
    bad = Manager; bad[7] = 0xd61f0000; assert(!ManagerDispatch(bad, 0));
    std::map<std::uintptr_t, std::uintptr_t> memory{{0x1050, 0x2003}, {0x2040, 0x3000}, {0x3000, 0x4000},
        {0x4080, 0x104b03e30}, {0x3020, 0x800}, {0x3028, 0x900}};
    std::map<std::uintptr_t, std::string> types{{0x2000, "N3RBX13EngineContextE"}, {0x3000, "N3RBX16DataModelManagerE"}};
    const auto find = [&]() {
        return ContextForModel(0x1000, 0x800, 0x900, 0x70, 0x40, 0x80,
            [&](auto at, auto& value) { auto i = memory.find(at); if (i == memory.end()) return false; value = i->second; return true; },
            [&](auto at, const char* type) { return types[at] == type; },
            [&](auto at, auto& words) { if (at == 0x104b03e30) words = Manager; else if (at == dispatch->move) words = Move;
                else if (at == dispatch->queue) words = Queue;
                else if (at == dispatch->destroy) words = Destroy; else return false; return true; });
    };
    assert(find() == 0x2000);
    memory[0x3020] = 0x808; assert(!find()); // another DataModel/subobject
    memory[0x3020] = 0x800; memory[0x3028] = 0x908; assert(!find()); // stale/shared owner mismatch
    memory[0x3028] = 0x900; types[0x2000] = "secondary subobject"; assert(!find());
    types[0x2000] = "N3RBX13EngineContextE";
    memory[0x1060] = 0x5000; memory[0x5040] = 0x3000; types[0x5000] = types[0x2000]; assert(!find());
    memory.erase(0x1050); assert(find() == 0x5000); // relocated field, no fixed 0x50 locator
    memory.erase(0x1060); memory[0x1070] = 0x5000; assert(!find()); // outside proven Instance prefix
    // Emulate Studio moving, invoking, then releasing an inline capture after
    // its initiating caller has returned (also covers cancellation/no invoke).
    for (bool invoke : {true, false}) {
        unsigned calls = 0; auto hold = std::make_shared<int>(7); std::weak_ptr<int> weak = hold;
        alignas(8) unsigned char queued[32]{};
        struct Table { void(*invoke)(void*); void(*copy)(void*, const void*); void(*move)(void*, void*); void(*destroy)(void*); };
        const Table* table = nullptr;
        {
            renium_tagged::Callback work{[hold, &calls] { ++calls; }}; hold.reset();
            auto raw = reinterpret_cast<unsigned char*>(&work);
            std::uintptr_t tag; std::memcpy(&tag, raw + 24, 8); assert((tag & 7) == 6);
            table = reinterpret_cast<const Table*>(tag & ~std::uintptr_t{7});
            table->move(queued, raw); table->destroy(raw); std::memset(raw + 24, 0, 8);
        }
        assert(!weak.expired()); if (invoke) table->invoke(queued); table->destroy(queued);
        assert(weak.expired() && calls == (invoke ? 1u : 0u));
    }
}
