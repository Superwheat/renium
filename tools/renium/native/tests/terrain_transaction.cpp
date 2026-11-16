#if defined(_WIN32)
#include <Windows.h>
#else
#include <mach/mach.h>
#include <pthread.h>
#endif
#include <cassert>
#include "../renium_studio_terrain.h"

struct Terrain { std::string smooth, physics, acquisition = "Custom"; };
using Field = std::string Terrain::*;
#if defined(_WIN32)
static void* Read(void* field, void* output, void* terrain) {
    return new (output) std::string(static_cast<Terrain*>(terrain)->*(*static_cast<Field*>(field)));
}
#else
static std::string Read(void* field, void* terrain) {
    return static_cast<Terrain*>(terrain)->*(*static_cast<Field*>(field));
}
#endif
static void Set(void* field, void* target, const std::string* value) {
    auto& terrain = *static_cast<Terrain*>(target);
    const auto member = *static_cast<Field*>(field);
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
}
