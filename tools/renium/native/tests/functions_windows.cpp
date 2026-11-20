#include <array>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <functional>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>
#include "../renium_studio_functions.h"

struct Receiver {
    std::array<std::uint64_t, 4> identity{11, 22, 33, 44};
    __declspec(noinline) std::string Documentation(std::string name) {
        return "https://developer.roblox.com/" + name;
    }
};

static bool catches_package_getter_fault(unsigned char* cell, unsigned* reads, unsigned at) {
    const std::uintptr_t address = reinterpret_cast<std::uintptr_t>(cell);
    __try {
        renium_functions::ConfirmPackageFlag(address, [=] {
            if (++*reads == at) return *reinterpret_cast<volatile unsigned char*>(1) != 0;
            return *cell == 1;
        }, [](std::uintptr_t) { return true; });
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return GetExceptionCode() == EXCEPTION_ACCESS_VIOLATION;
    }
    return false;
}

bool package_flag_confirmation() {
    unsigned char cell = 0;
    const auto candidate = reinterpret_cast<std::uintptr_t>(&cell);
    auto writable = [](std::uintptr_t) { return true; };
    auto rejects = [&](auto get, auto allowed, const auto& inputs) {
        try { renium_functions::ConfirmPackageFlag(inputs, get, allowed); }
        catch (const std::runtime_error&) { return true; }
        return false;
    };
    unsigned reads = 0;
    if (!rejects([&] { ++reads; return false; }, writable, candidate) || cell != 0 || reads != 2) return false;
    reads = 0;
    if (!rejects([&]() -> bool { if (++reads == 2) throw std::runtime_error("getter refused"); return false; }, writable, candidate) || cell != 0) return false;
    reads = 0;
    if (!rejects([&] { ++reads; return false; }, [](std::uintptr_t) { return false; }, candidate) || cell != 0 || reads != 1) return false;
    reads = 0;
    cell = 1;
    if (!rejects([] { return false; }, writable, candidate) || cell != 1) return false;
    cell = 0;
    reads = 0;
    // One coincidental true answer must not confirm the candidate.
    if (!rejects([&] { return ++reads > 1; }, writable, candidate) || cell != 0 || reads != 3) return false;
    reads = 0;
    if (!rejects([&] { ++reads; return reads == 2; }, writable, candidate) || cell != 0 || reads != 4) return false;
    // A concurrent change must not be overwritten by restoration.
    reads = 0;
    if (!rejects([&] { if (++reads == 2) cell = 2; return false; }, writable, candidate) || cell != 2) return false;
    for (unsigned fault : {2, 3, 4}) {
        cell = 0; reads = 0;
        if (!catches_package_getter_fault(&cell, &reads, fault) || cell != 0 || reads != fault) return false;
    }
    cell = 0; reads = 0;
    if (!renium_functions::ConfirmPackageFlag(candidate, [&] { ++reads; return cell == 1; }, writable) || cell != 1 || reads != 4) return false;
    return true;
}

int main() {
    // Refuse even a stale/direct helper request before any engine memory read.
    for (unsigned mode : {1u, 2u}) {
        renium_functions::Input input{};
        input.mode = mode;
        input.function = 1;
        unsigned reads = 0;
        bool refused = false;
        try {
            renium_functions::Begin(nullptr, {}, reinterpret_cast<const char*>(&input), sizeof(input),
                [&](std::uintptr_t, void*, std::size_t) { ++reads; return false; });
        } catch (const std::runtime_error& error) {
            refused = std::strstr(error.what(), "yield-call-contract") != nullptr;
        }
        if (!refused || reads) return 4;
    }
    if (!package_flag_confirmation()) return 3;
    Receiver receiver;
    const auto identity = receiver.identity;
    auto member = &Receiver::Documentation;
    std::uint64_t address = 0;
    static_assert(sizeof(member) == sizeof(address));
    std::memcpy(&address, &member, sizeof(address));
    std::array<std::uint64_t, 128> descriptor{};
    descriptor[0] = 123;
    for (unsigned field : {16, 64, 768}) {
    descriptor[field / 8] = address;
    for (unsigned index = 0; index < 32; ++index) {
        const std::string name(index % 2 ? 100 : 3, 'x');
        renium_functions::Input input{};
        input.descriptor = reinterpret_cast<std::uintptr_t>(descriptor.data());
        input.table = descriptor[0];
        input.field = field;
        input.mode = 3;
        input.function = address;
        input.firstSize = static_cast<std::uint32_t>(name.size());
        std::vector<char> bytes(sizeof(input) + name.size());
        std::memcpy(bytes.data(), &input, sizeof(input));
        std::memcpy(bytes.data() + sizeof(input), name.data(), name.size());
        auto read = [&](std::uint64_t pointer, void* output, std::size_t size) {
                if (pointer < input.descriptor || pointer + size > input.descriptor + sizeof(descriptor)) return false;
                std::memcpy(output, reinterpret_cast<void*>(pointer), size);
                return true;
            };
        auto completion = renium_functions::Begin(&receiver, {}, bytes.data(), bytes.size(), read);
        if (completion->Wait(std::chrono::milliseconds(1)) != "https://developer.roblox.com/" + name
            || receiver.identity != identity) {
            std::fprintf(stderr, "Function return changed the receiver or response at call %u\n", index);
            return 1;
        }
        descriptor[field / 8 + 1] = 1;
        bool refused = false;
        try { renium_functions::Begin(&receiver, {}, bytes.data(), bytes.size(), read); }
        catch (const std::runtime_error&) { refused = true; }
        descriptor[field / 8 + 1] = 0;
        if (!refused || receiver.identity != identity) return 2;
    }
    }
    return 0;
}
