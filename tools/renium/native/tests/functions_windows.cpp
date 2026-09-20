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

int main() {
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
