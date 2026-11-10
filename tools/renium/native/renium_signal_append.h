#pragma once
#include <Windows.h>
#include <cstdint>
#include <stdexcept>

// Selected by the verified caller/callee contract, never by Studio's build.
// Both forms leave exactly one weak connection reference owned by the caller.
inline void AppendSignal(std::uint32_t abi, std::uintptr_t append, void* signal,
    unsigned char* node, void** connection) {
    switch (abi) {
    case 0:
        reinterpret_cast<void(__fastcall*)(void*, void*)>(append)(signal, node);
        InterlockedIncrement(reinterpret_cast<LONG*>(node + 4));
        *connection = node;
        break;
    case 1:
        reinterpret_cast<void*(__fastcall*)(void*, void**, void*)>(append)(signal, connection, node);
        break;
    default:
        throw std::runtime_error("Unknown signal append ABI");
    }
}
