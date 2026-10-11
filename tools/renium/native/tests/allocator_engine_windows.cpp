// Deliberately separate heap: reproduces Studio's allocator transition without
// loading Studio. This fixture is linked as mimalloc.dll for its import ABI.
#include <Windows.h>
#include <cstdint>
#include <cstdlib>
#include <functional>
#include <memory>
#include <new>
#include <optional>
#include <string>
#include "../renium_studio_history.h"

static HANDLE EngineHeap() { static const auto heap = HeapCreate(0, 0, 0); return heap; }
extern "C" __declspec(dllexport) void* mi_malloc_aligned(std::size_t size, std::size_t alignment) {
    const auto raw = HeapAlloc(EngineHeap(), 0, size + alignment + sizeof(void*));
    if (!raw) return nullptr;
    const auto address = (reinterpret_cast<std::uintptr_t>(raw) + sizeof(void*) + alignment - 1) & ~(alignment - 1);
    reinterpret_cast<void**>(address)[-1] = raw;
    return reinterpret_cast<void*>(address);
}
extern "C" __declspec(dllexport) void* mi_malloc(std::size_t size) { return mi_malloc_aligned(size, 16); }
extern "C" __declspec(dllexport) void mi_free(void* pointer) {
    if (!pointer) return;
    const auto raw = static_cast<void**>(pointer)[-1];
    if (!HeapValidate(EngineHeap(), 0, raw) || !HeapFree(EngineHeap(), 0, raw)) std::abort();
}
void* operator new(std::size_t size) { if (auto p = mi_malloc(size)) return p; throw std::bad_alloc(); }
void* operator new[](std::size_t size) { return ::operator new(size); }
void operator delete(void* p) noexcept { mi_free(p); }
void operator delete[](void* p) noexcept { mi_free(p); }
void operator delete(void* p, std::size_t) noexcept { mi_free(p); }
void operator delete[](void* p, std::size_t) noexcept { mi_free(p); }

using Options = std::optional<std::shared_ptr<const void>>;
extern "C" __declspec(dllexport) void EngineFinish(void* history, std::string token, std::uint32_t operation, Options options) {
    if (token != std::string(80, 't') || operation != 1 || !options || options->get() != history) std::abort();
    ++*static_cast<unsigned*>(history);
}
extern "C" __declspec(dllexport) void EngineGetString(void* output) { new(output) std::string(8192, 'e'); }
static std::function<void(std::string)> callback;
extern "C" __declspec(dllexport) void EngineStoreCallback(std::function<void(std::string)> value) { callback = std::move(value); }
extern "C" __declspec(dllexport) void EngineInvokeCallback() { callback(std::string(8192, 'c')); callback = {}; }

extern "C" __declspec(dllexport) std::uintptr_t EngineHistoryPredecessor(renium_history::Binding binding, bool active) {
    using namespace renium_history;
    static struct Owner { void* table = nullptr; long strong = 1, weak = 1; } owner;
    installed = binding;
    originalFinish = reinterpret_cast<Finish>(&EngineFinish);
    registrations.clear();
    if (active) registrations.emplace("pending", Registration{nullptr, 0, std::make_shared<WeakOwner>(&owner), {}, {}, false});
    return reinterpret_cast<std::uintptr_t>(&ReniumHistoryFinish);
}
