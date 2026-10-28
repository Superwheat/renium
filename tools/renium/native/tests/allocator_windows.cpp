#include <Windows.h>
#include <cassert>
#include <array>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include "../renium_engine_allocator_windows.h"
#include "../renium_studio_history.h"
#include "../renium_continuation_windows.h"

extern "C" __declspec(dllimport) void* mi_malloc(std::size_t);
extern "C" __declspec(dllimport) void* mi_malloc_aligned(std::size_t, std::size_t);
extern "C" __declspec(dllimport) void mi_free(void*);
extern "C" __declspec(dllimport) void EngineFinish(void*, std::string, std::uint32_t, renium_history::Options);
extern "C" __declspec(dllimport) void EngineGetString(void*);
extern "C" __declspec(dllimport) void EngineStoreCallback(std::function<void(std::string)>);
extern "C" __declspec(dllimport) void EngineInvokeCallback();
extern "C" __declspec(dllimport) std::uintptr_t EngineHistoryPredecessor(renium_history::Binding, bool);

// Exercise allocation during executable static initialization too.
static const std::string initial(512, 'i');

#include "continuation_windows.h"
static bool ThrowingSubmit(void*, void*, std::uint32_t mode) {
    if (mode == 1) throw std::bad_alloc();
    throw 42;
}
static bool UnknownThrowingSubmit(void*, void*, std::uint32_t) { throw 42; }
static void PackageBoundary(const char* path) {
    const auto helper = LoadLibraryA(path);
    assert(helper);
    const auto call = reinterpret_cast<DWORD(WINAPI*)(void*)>(GetProcAddress(helper, "ReniumPackageAction"));
    const auto configure = reinterpret_cast<DWORD(WINAPI*)(void*)>(GetProcAddress(helper, "ReniumTaskContinuations"));
    assert(call && configure);
    // Exercise the real exported remote-thread entry in this disposable host.
    // A valid owner remains retained by the test, and no engine task runs.
    alignas(8) std::uint64_t owner[2]{0, 2};
    for (const auto failure : {0, 1, 2}) for (const auto mode : {6, 7, 8}) {
        alignas(8) unsigned char params[688]{};
        const auto put64 = [&](unsigned at, std::uint64_t value) { std::memcpy(params + at, &value, 8); };
        const auto put32 = [&](unsigned at, std::uint32_t value) { std::memcpy(params + at, &value, 4); };
        put64(0, 1); put64(8, 1); put64(16, reinterpret_cast<std::uintptr_t>(owner));
        put32(40, 1000); put64(432, 1); put64(440, 1); put64(448, 1); put64(480, 1); put32(488, mode);
        put64(456, reinterpret_cast<std::uintptr_t>(failure == 2 ? &UnknownThrowingSubmit : &ThrowingSubmit) - 1);
        if (failure) { std::uint32_t abi = 2; assert(configure(&abi) == 0); }
        const auto result = call(params);
        assert(result == (failure == 2 ? 0xE312 : 0xE311));
        assert(*reinterpret_cast<std::uint32_t*>(params + 44) == result && params[160]);
        assert(owner[1] == 2);
    }
    FreeLibrary(helper);
}
int main(int argc, char** argv) {
    renium_continuation::taskAbi.store(renium_continuation::Abi::Msvc);
    assert(initial.size() == 512);
    const auto& allocator = renium_allocator::Engine();
    assert(allocator.allocate == &mi_malloc && allocator.release == &mi_free && allocator.aligned == &mi_malloc_aligned);
    continuation_test::Run();
    assert(argc == 2); PackageBoundary(argv[1]);
    // Import resolution follows the bound IAT, and a partial contract refuses
    // rather than allocating in one heap and releasing in another.
    alignas(16) std::array<unsigned char, 4096> image{};
    const auto dos = reinterpret_cast<IMAGE_DOS_HEADER*>(image.data());
    dos->e_magic = IMAGE_DOS_SIGNATURE; dos->e_lfanew = 128;
    const auto nt = reinterpret_cast<IMAGE_NT_HEADERS64*>(image.data() + 128);
    nt->Signature = IMAGE_NT_SIGNATURE; nt->OptionalHeader.Magic = IMAGE_NT_OPTIONAL_HDR64_MAGIC;
    nt->OptionalHeader.SizeOfImage = static_cast<DWORD>(image.size());
    nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT] = {512, 40};
    const auto import = reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(image.data() + 512);
    import->Name = 768; import->OriginalFirstThunk = 1024; import->FirstThunk = 1280;
    std::memcpy(image.data() + 768, "mimalloc.dll", 13);
    auto names = reinterpret_cast<IMAGE_THUNK_DATA64*>(image.data() + 1024);
    auto addresses = reinterpret_cast<IMAGE_THUNK_DATA64*>(image.data() + 1280);
    names[0].u1.AddressOfData = 1536;
    std::memcpy(image.data() + 1538, "mi_malloc", 10);
    addresses[0].u1.Function = reinterpret_cast<std::uintptr_t>(&mi_malloc);
    auto partial = renium_allocator::ImportedAllocator(image.data());
    assert(partial.imported && !partial.Valid());
    import->Name = 2048; // An executable without mimalloc retains its CRT.
    const auto legacy = renium_allocator::ImportedAllocator(image.data());
    assert(!legacy.imported && legacy.Valid());
    mi_free(mi_malloc(1)); mi_free(mi_malloc_aligned(1, 64));
    renium_history::Binding previous{};
    previous.descriptor = 123; previous.table = 456; previous.memberOffset = 144;
    previous.finish = reinterpret_cast<std::uintptr_t>(&EngineFinish);
    auto predecessor = EngineHistoryPredecessor(previous, false);
    assert(renium_history::CertifiedPredecessor(predecessor, previous) == reinterpret_cast<renium_history::Finish>(&EngineFinish));
    predecessor = EngineHistoryPredecessor(previous, true);
    assert(renium_history::CertifiedPredecessor(predecessor, previous) == reinterpret_cast<renium_history::Finish>(predecessor));
    auto wrong = previous; ++wrong.descriptor;
    assert(!renium_history::CertifiedPredecessor(predecessor, wrong));
    assert(!renium_history::CertifiedPredecessor(previous.finish, previous));
    EngineHistoryPredecessor(previous, false);
    unsigned calls = 0;
    renium_history::originalFinish = reinterpret_cast<renium_history::Finish>(&EngineFinish);
    using TypedFinish = void (*)(void*, std::string, std::uint32_t, renium_history::Options);
    const auto hook = reinterpret_cast<TypedFinish>(&renium_history::ReniumHistoryFinish);
    for (unsigned i = 0; i < 100; ++i) {
        auto owner = std::shared_ptr<const void>(&calls, [](const void*) {});
        hook(&calls, std::string(80, 't'), 1, owner);
        assert(owner.use_count() == 1 && calls == i + 1);
        alignas(std::string) unsigned char storage[sizeof(std::string)];
        EngineGetString(storage);
        const auto value = reinterpret_cast<std::string*>(storage);
        assert(*value == std::string(8192, 'e'));
        value->~basic_string();
        bool called = false;
        // A heap-sized closure forces callback allocation across the boundary.
        EngineStoreCallback([&, padding = std::string(1024, 'p')](std::string text) {
            assert(padding.size() == 1024 && text == std::string(8192, 'c')); called = true;
        });
        EngineInvokeCallback();
        assert(called);
        auto aligned = ::operator new(100, std::align_val_t(64));
        assert(reinterpret_cast<std::uintptr_t>(aligned) % 64 == 0);
        ::operator delete(aligned, std::align_val_t(64));
    }
}
