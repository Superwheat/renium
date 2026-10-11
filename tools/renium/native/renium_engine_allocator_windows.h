// STL objects cross the Studio/helper ABI (strings, callbacks and streams).
// Studio 0.742 uses mimalloc while the /MT helper's default new uses the CRT
// heap. Use the allocator actually imported by the executable, from the first
// helper allocation onward; never switch allocators after objects exist.
#pragma once
#include <Windows.h>
#include <cstdlib>
#include <cstring>
#include <new>
#include <malloc.h>

namespace renium_allocator {
struct Allocator {
    void* (*allocate)(std::size_t) = nullptr;
    void (*release)(void*) = nullptr;
    void* (*aligned)(std::size_t, std::size_t) = nullptr;
    bool imported = false;
    bool Valid() const noexcept { return !imported || (allocate && release && aligned); }
};

inline Allocator ImportedAllocator(const void* module) noexcept {
    const auto base = static_cast<const unsigned char*>(module);
    if (!base) return {};
    const auto dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(base);
    if (dos->e_magic != IMAGE_DOS_SIGNATURE || dos->e_lfanew <= 0 || dos->e_lfanew > 4096) return {};
    const auto nt = reinterpret_cast<const IMAGE_NT_HEADERS64*>(base + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE || nt->OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC) return {};
    const auto size = nt->OptionalHeader.SizeOfImage;
    const auto fits = [size](std::size_t offset, std::size_t bytes) { return offset <= size && bytes <= size - offset; };
    const auto text = [&](std::size_t offset, const char* expected, bool insensitive = false) {
        const auto length = std::strlen(expected) + 1;
        return fits(offset, length) && (insensitive
            ? _strnicmp(reinterpret_cast<const char*>(base + offset), expected, length) == 0
            : std::memcmp(base + offset, expected, length) == 0);
    };
    const auto directory = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!fits(directory.VirtualAddress, directory.Size)) return {};
    for (std::size_t offset = 0; offset + sizeof(IMAGE_IMPORT_DESCRIPTOR) <= directory.Size; offset += sizeof(IMAGE_IMPORT_DESCRIPTOR)) {
        const auto entry = reinterpret_cast<const IMAGE_IMPORT_DESCRIPTOR*>(base + directory.VirtualAddress + offset);
        if (!entry->Name) break;
        if (!text(entry->Name, "mimalloc.dll", true)) continue;
        Allocator result;
        result.imported = true;
        if (!entry->OriginalFirstThunk || !entry->FirstThunk) return result;
        for (std::size_t slot = 0; slot < size / sizeof(IMAGE_THUNK_DATA64); ++slot) {
            const auto names = static_cast<std::size_t>(entry->OriginalFirstThunk) + slot * sizeof(IMAGE_THUNK_DATA64);
            const auto values = static_cast<std::size_t>(entry->FirstThunk) + slot * sizeof(IMAGE_THUNK_DATA64);
            if (!fits(names, sizeof(IMAGE_THUNK_DATA64)) || !fits(values, sizeof(IMAGE_THUNK_DATA64))) return result;
            const auto name = reinterpret_cast<const IMAGE_THUNK_DATA64*>(base + names)->u1.AddressOfData;
            if (!name) break;
            if (IMAGE_SNAP_BY_ORDINAL64(name) || !fits(name, sizeof(WORD))) continue;
            const auto value = reinterpret_cast<const IMAGE_THUNK_DATA64*>(base + values)->u1.Function;
            if (text(name + sizeof(WORD), "mi_malloc")) result.allocate = reinterpret_cast<decltype(result.allocate)>(value);
            if (text(name + sizeof(WORD), "mi_free")) result.release = reinterpret_cast<decltype(result.release)>(value);
            if (text(name + sizeof(WORD), "mi_malloc_aligned")) result.aligned = reinterpret_cast<decltype(result.aligned)>(value);
        }
        // A partial allocator contract must never mix allocation and release.
        return result;
    }
    return {};
}

inline const Allocator& Engine() noexcept {
    // Resolving this uses no C++ heap allocation and is safe during DLL static
    // initialization. The executable's imports were bound before LoadLibrary.
    static const Allocator allocator = ImportedAllocator(GetModuleHandleW(nullptr));
    return allocator;
}

inline void* Allocate(std::size_t size, std::size_t alignment = 0) {
    const auto& allocator = Engine();
    if (!allocator.Valid()) throw std::bad_alloc();
    if (!size) size = 1;
    for (;;) {
        void* result = alignment
            ? (allocator.aligned ? allocator.aligned(size, alignment) : _aligned_malloc(size, alignment))
            : (allocator.allocate ? allocator.allocate(size) : std::malloc(size));
        if (result) return result;
        const auto handler = std::get_new_handler();
        if (!handler) throw std::bad_alloc();
        handler();
    }
}
inline void Release(void* pointer, bool aligned = false) noexcept {
    const auto& allocator = Engine();
    if (!allocator.Valid()) std::abort();
    if (allocator.release) allocator.release(pointer);
    else if (aligned) _aligned_free(pointer);
    else std::free(pointer);
}
}

void* operator new(std::size_t size) { return renium_allocator::Allocate(size); }
void* operator new[](std::size_t size) { return renium_allocator::Allocate(size); }
void operator delete(void* pointer) noexcept { renium_allocator::Release(pointer); }
void operator delete[](void* pointer) noexcept { renium_allocator::Release(pointer); }
void operator delete(void* pointer, std::size_t) noexcept { renium_allocator::Release(pointer); }
void operator delete[](void* pointer, std::size_t) noexcept { renium_allocator::Release(pointer); }
void* operator new(std::size_t size, std::align_val_t alignment) { return renium_allocator::Allocate(size, static_cast<std::size_t>(alignment)); }
void* operator new[](std::size_t size, std::align_val_t alignment) { return renium_allocator::Allocate(size, static_cast<std::size_t>(alignment)); }
void operator delete(void* pointer, std::align_val_t) noexcept { renium_allocator::Release(pointer, true); }
void operator delete[](void* pointer, std::align_val_t) noexcept { renium_allocator::Release(pointer, true); }
void operator delete(void* pointer, std::size_t, std::align_val_t) noexcept { renium_allocator::Release(pointer, true); }
void operator delete[](void* pointer, std::size_t, std::align_val_t) noexcept { renium_allocator::Release(pointer, true); }
