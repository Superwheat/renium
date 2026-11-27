#if defined(_WIN32)
#include <Windows.h>
#else
#include <mach/mach.h>
#include <pthread.h>
#endif
#include <cassert>
#include "../renium_studio_terrain.h"

struct Terrain {
    std::string smooth, physics, acquisition = "Custom";
    unsigned physicsReadsBeforeSet = 0;
    bool physicsSetCalled = false;
};
using Field = std::string Terrain::*;
#if defined(_WIN32)
static void* Read(void* field, void* output, void* terrain) {
    auto& state = *static_cast<Terrain*>(terrain);
    const auto member = *static_cast<Field*>(field);
    if (member == &Terrain::physics && !state.physicsSetCalled) ++state.physicsReadsBeforeSet;
    return new (output) std::string(state.*member);
}
#else
static std::string Read(void* field, void* terrain) {
    auto& state = *static_cast<Terrain*>(terrain);
    const auto member = *static_cast<Field*>(field);
    if (member == &Terrain::physics && !state.physicsSetCalled) ++state.physicsReadsBeforeSet;
    return state.*member;
}
#endif
static void Set(void* field, void* target, const std::string* value) {
    auto& terrain = *static_cast<Terrain*>(target);
    const auto member = *static_cast<Field*>(field);
    if (member == &Terrain::physics) terrain.physicsSetCalled = true;
    terrain.*member = *value;
    if (member == &Terrain::smooth) terrain.acquisition = "Legacy";
}
static bool SetAcquisition(void* field, void* terrain, const std::string* value) {
    Set(field, terrain, value); return true;
}
static void Clear(void* terrain) { static_cast<Terrain*>(terrain)->smooth.clear(); }

static renium_terrain::Binding Bind(Field* field, std::uintptr_t setter) {
    return {reinterpret_cast<std::uintptr_t>(field), 0, reinterpret_cast<std::uintptr_t>(&Read), setter, 0, 0};
}
static std::string Grid(unsigned char material) {
    auto cells = std::make_unique<renium_terrain::Cells>();
    (*cells)[0] = material; (*cells)[1] = 255;
    std::string result("\1\5", 2);
    renium_terrain::Coordinate previous{};
    renium_terrain::Append(result, {}, previous, renium_terrain::Encode(*cells));
    return result;
}
static bool Rejected(const std::function<void()>& operation) {
    try { operation(); } catch (const std::runtime_error&) { return true; }
    return false;
}

int main() {
    Field smooth = &Terrain::smooth, physics = &Terrain::physics, acquisition = &Terrain::acquisition;
    Terrain terrain;
    auto write = std::make_shared<renium_terrain::Write>();
    write->terrain = &terrain;
    write->smooth = Bind(&smooth, reinterpret_cast<std::uintptr_t>(&Set));
    write->physics = Bind(&physics, reinterpret_cast<std::uintptr_t>(&Set));
    write->acquisition = Bind(&acquisition, reinterpret_cast<std::uintptr_t>(&SetAcquisition));
    write->clear.function = reinterpret_cast<std::uintptr_t>(&Clear);
    write->beforeSmooth = Grid(1); write->writtenSmooth = Grid(2);
    write->beforePhysics = "original cache"; write->writtenPhysics = "imported cache";
    terrain.smooth = write->writtenSmooth; terrain.physics = write->writtenPhysics;
    write->Check();
    // Deferred cooking changes only the derived grid, without another voxel edit.
    terrain.physics = "rebuilt cache";
    write->Check();
    write->Rollback();
    assert(terrain.smooth == write->beforeSmooth && terrain.physics == write->beforePhysics);
    assert(terrain.acquisition == "Custom");

    // A physics-only write has no voxel rebuild to account for a changed cache.
    auto physicsOnly = *write;
    physicsOnly.writtenSmooth = physicsOnly.beforeSmooth;
    terrain.physics = physicsOnly.writtenPhysics;
    physicsOnly.Check();
    terrain.physics = "outside cache";
    assert(Rejected([&] { physicsOnly.Check(); }));
    physicsOnly.Rollback();
    assert(terrain.physics == "outside cache");

    // A concurrent voxel edit must reject commit and survive its cancellation.
    terrain.smooth = Grid(3); terrain.physics = "outside voxel cache";
    const auto outsideSmooth = terrain.smooth;
    assert(Rejected([&] { write->Check(); }));
    auto model = std::make_shared<int>(0);
    struct Shared { void* instance; void* owner; } shared{};
    static_assert(sizeof(shared) == sizeof(model));
    std::memcpy(&shared, &model, sizeof(shared));
    auto weak = std::make_shared<renium_history::WeakOwner>(shared.owner);
    int history = 0;
    std::string token = "terrain-test";
    unsigned rollbacks = 0;
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, reinterpret_cast<std::uintptr_t>(shared.instance), weak,
        [&] { ++rollbacks; write->Rollback(); }, [write] { write->Check(); }, true});
    renium_history::originalFinish = [](void* target, std::string*, std::uint32_t, renium_history::Options*) {
        ++*static_cast<int*>(target);
    };
    renium_history::Options options;
    assert(Rejected([&] { renium_history::ReniumHistoryFinish(&history, &token, 1, &options); }));
    assert(history == 0 && rollbacks == 0 && renium_history::registrations.contains(token));
    renium_history::ReniumHistoryFinish(&history, &token, 0, &options);
    assert(history == 1 && rollbacks == 1 && !renium_history::registrations.contains(token));
    assert(terrain.smooth == outsideSmooth && terrain.physics == "outside voxel cache");

    terrain.smooth = write->writtenSmooth; terrain.physics = "rebuilt cache";
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, reinterpret_cast<std::uintptr_t>(shared.instance), weak,
        [&] { ++rollbacks; write->Rollback(); }, [write] { write->Check(); }, true});
    renium_history::ReniumHistoryFinish(&history, &token, 1, &options);
    assert(history == 2 && rollbacks == 1 && !renium_history::registrations.contains(token));
    assert(terrain.smooth == write->writtenSmooth && terrain.physics == "rebuilt cache");

    // Restoring the written PhysicsGrid at commit writes directly without
    // reading the invalidated cache first, then verifies the accepted bytes.
    write->restorePhysicsOnCommit = true;
    terrain.smooth = write->writtenSmooth;
    terrain.physics = "dirty cache";
    terrain.physicsReadsBeforeSet = 0;
    terrain.physicsSetCalled = false;
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, reinterpret_cast<std::uintptr_t>(shared.instance), weak,
        [&] { ++rollbacks; write->Rollback(); }, [write] { write->Check(); }, true});
    renium_history::ReniumHistoryFinish(&history, &token, 1, &options);
    assert(history == 3 && rollbacks == 1 && !renium_history::registrations.contains(token));
    assert(terrain.physicsSetCalled && terrain.physicsReadsBeforeSet == 0);
    assert(terrain.physics == write->writtenPhysics);

    // Two Terrain writes under one token with a physics cache rebuild between
    // them: the second write's pre-check restores the first write's PhysicsGrid
    // before reading its baseline, so the second write matches the first write's
    // fingerprint and rolling back restores the restored grid and then the
    // original pre-transaction state.
    struct FakeVtable { std::uintptr_t getter; std::uintptr_t setter; };
    struct FakeProp { FakeVtable* vtable; Field member; };
    struct FakeClearObj { FakeClearObj* vtable; std::uintptr_t clear; };
#if defined(_WIN32)
    auto fakeRead = +[](void* obj, void* output, void* target) -> void* {
        return Read(&static_cast<FakeProp*>(obj)->member, output, target);
    };
#else
    auto fakeRead = +[](void* obj, void* target) -> std::string {
        return Read(&static_cast<FakeProp*>(obj)->member, target);
    };
#endif
    auto fakeSet = +[](void* obj, void* target, const std::string* value) {
        Set(&static_cast<FakeProp*>(obj)->member, target, value);
    };
    auto fakeSetAcq = +[](void* obj, void* target, const std::string* value) -> bool {
        return SetAcquisition(&static_cast<FakeProp*>(obj)->member, target, value);
    };
    FakeVtable smoothTable{reinterpret_cast<std::uintptr_t>(fakeRead), reinterpret_cast<std::uintptr_t>(fakeSet)};
    FakeVtable physicsTable{reinterpret_cast<std::uintptr_t>(fakeRead), reinterpret_cast<std::uintptr_t>(fakeSet)};
    FakeVtable acqTable{reinterpret_cast<std::uintptr_t>(fakeRead), reinterpret_cast<std::uintptr_t>(fakeSetAcq)};
    FakeProp smoothProp{&smoothTable, &Terrain::smooth};
    FakeProp physicsProp{&physicsTable, &Terrain::physics};
    FakeProp acqProp{&acqTable, &Terrain::acquisition};
    FakeClearObj clearObj{nullptr, reinterpret_cast<std::uintptr_t>(&Clear)};
    clearObj.vtable = &clearObj;
    const auto makeBinding = [](FakeProp* prop) {
        return renium_terrain::Binding{
            reinterpret_cast<std::uintptr_t>(prop),
            reinterpret_cast<std::uintptr_t>(prop->vtable),
            prop->vtable->getter,
            prop->vtable->setter,
            0,
            sizeof(std::uintptr_t),
        };
    };
    const auto makePayload = [&](const renium_terrain::Fingerprint& fp, std::uint32_t flags,
                                 const std::string& sm, const std::string& ph) {
        std::string buf;
        auto append = [&](const void* ptr, std::size_t n) {
            buf.append(reinterpret_cast<const char*>(ptr), n);
        };
        const auto sb = makeBinding(&smoothProp);
        const auto pb = makeBinding(&physicsProp);
        const auto cb = renium_terrain::ClearBinding{
            reinterpret_cast<std::uintptr_t>(&clearObj),
            reinterpret_cast<std::uintptr_t>(&clearObj),
            sizeof(std::uintptr_t),
            clearObj.clear,
        };
        const auto ab = makeBinding(&acqProp);
        append(&sb, sizeof(sb));
        append(&pb, sizeof(pb));
        append(&cb, sizeof(cb));
        append(&ab, sizeof(ab));
        append(fp.data(), fp.size());
        const std::uint32_t lens[4] = {
            static_cast<std::uint32_t>(token.size()),
            static_cast<std::uint32_t>(sm.size()),
            static_cast<std::uint32_t>(ph.size()),
            flags,
        };
        append(lens, sizeof(lens));
        buf.append(token);
        buf.append(sm);
        buf.append(ph);
        return buf;
    };
    const auto readMem = [](std::uintptr_t addr, void* out, std::size_t size) {
        std::memcpy(out, reinterpret_cast<const void*>(addr), size);
        return true;
    };
    terrain.smooth = Grid(1);
    terrain.physics = "pre-tx physics";
    terrain.acquisition = "Custom";
    const auto modelAddr = reinterpret_cast<std::uintptr_t>(shared.instance);
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, modelAddr, weak, {}, {}, false});
    const auto baseProbe = renium_terrain::Apply(
        &terrain, model, modelAddr, makePayload({}, 4, "", ""), readMem);
    assert(!baseProbe.changed);
    const auto first = renium_terrain::Apply(
        &terrain, model, modelAddr, makePayload(baseProbe.fingerprint, 3, Grid(2), "first physics"), readMem);
    assert(first.changed && terrain.smooth == Grid(2) && terrain.physics == "first physics");
    // Studio's background voxel cook overwrites PhysicsGrid before the second write.
    terrain.physics = "rebuilt cache after first write";
    const auto second = renium_terrain::Apply(
        &terrain, model, modelAddr, makePayload(first.fingerprint, 3, Grid(3), "second physics"), readMem);
    assert(second.changed && terrain.smooth == Grid(3) && terrain.physics == "second physics");
    // Cancel the transaction: rollback unwinds write 2 (to write 1's restored
    // grid, not the rebuilt cache) and then write 1 (to the original state).
    renium_history::ReniumHistoryFinish(&history, &token, 0, &options);
    assert(history == 4 && !renium_history::registrations.contains(token));
    assert(terrain.smooth == Grid(1) && terrain.physics == "pre-tx physics");
    assert(terrain.acquisition == "Custom");

    // Also verify write 2's standalone rollback restores write 1's physics grid
    // (never the rebuilt cache) when write 1 has no prior rollback.
    terrain.smooth = Grid(1);
    terrain.physics = "pre-tx physics";
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, modelAddr, weak, {}, {}, false});
    const auto base2 = renium_terrain::Apply(
        &terrain, model, modelAddr, makePayload({}, 4, "", ""), readMem);
    const auto w1 = renium_terrain::Apply(
        &terrain, model, modelAddr, makePayload(base2.fingerprint, 3, Grid(2), "first physics"), readMem);
    terrain.physics = "rebuilt cache after first write";
    // Strip write 1's rollback so cancelling runs ONLY write 2's rollback.
    renium_history::registrations.at(token).rollbackTerrain = {};
    const auto w2 = renium_terrain::Apply(
        &terrain, model, modelAddr, makePayload(w1.fingerprint, 3, Grid(3), "second physics"), readMem);
    assert(w2.changed);
    renium_history::ReniumHistoryFinish(&history, &token, 0, &options);
    assert(terrain.smooth == Grid(2) && terrain.physics == "first physics");

    // Lock-baseline + external chunk clear + post-clear write: no Clear/Set
    // when cleared grid already matches target, outside edits are rejected,
    // and rollback restores the pre-clear original smooth grid.
    auto makeMulti = [](std::initializer_list<std::pair<renium_terrain::Coordinate, unsigned char>> items) {
        std::string out("\1\5", 2);
        renium_terrain::Coordinate prev{};
        for (const auto& [coord, mat] : items) {
            auto c = std::make_unique<renium_terrain::Cells>();
            (*c)[0] = mat; (*c)[1] = 255;
            renium_terrain::Append(out, coord, prev, renium_terrain::Encode(*c));
        }
        return out;
    };
    const auto gOne = makeMulti({{{0, 0, 0}, 1}});
    const auto gTwo = makeMulti({{{0, 0, 0}, 1}, {{1, 0, 0}, 2}});
    const auto gOutside = makeMulti({{{0, 0, 0}, 3}});
    bool hasRemoved = false;
    assert(renium_terrain::BuildWriteDelta(gTwo, gOne, &hasRemoved).empty() && hasRemoved);
    assert(!renium_terrain::BuildWriteDelta(gOne, gTwo, &hasRemoved).empty() && !hasRemoved);
    assert(renium_terrain::CheckPostClear(gTwo, gOne, gOne));
    assert(!renium_terrain::CheckPostClear(gTwo, gOutside, gOne));

    terrain.smooth = gTwo;
    terrain.physics = "two physics";
    terrain.acquisition = "Custom";
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, modelAddr, weak, {}, {}, false});
    const auto p3 = renium_terrain::Apply(&terrain, model, modelAddr, makePayload({}, 4, "", ""), readMem);
    renium_terrain::Apply(&terrain, model, modelAddr, makePayload(p3.fingerprint, 8, "", ""), readMem);
    // Simulate Luau FillRegion removing chunk (1,0,0).
    terrain.smooth = gOne;
    const auto post = renium_terrain::Apply(&terrain, model, modelAddr, makePayload({}, 16 | 3, gOne, "one physics"), readMem);
    assert(post.changed && terrain.smooth == gOne && terrain.physics == "one physics" && terrain.acquisition == "Custom");
    // Rollback must restore gTwo (original before lock_baseline), not gOne.
    renium_history::ReniumHistoryFinish(&history, &token, 0, &options);
    assert(terrain.smooth == gTwo && terrain.physics == "two physics");

    // Outside edit between lock_baseline and post_clear must be rejected.
    terrain.smooth = gTwo;
    terrain.physics = "two physics";
    renium_history::registrations.emplace(token, renium_history::Registration{
        &history, modelAddr, weak, {}, {}, false});
    const auto p4 = renium_terrain::Apply(&terrain, model, modelAddr, makePayload({}, 4, "", ""), readMem);
    renium_terrain::Apply(&terrain, model, modelAddr, makePayload(p4.fingerprint, 8, "", ""), readMem);
    terrain.smooth = gOutside;
    assert(Rejected([&] {
        renium_terrain::Apply(&terrain, model, modelAddr, makePayload({}, 16 | 3, gOne, "one physics"), readMem);
    }));
    renium_history::ReniumHistoryFinish(&history, &token, 0, &options);
}
