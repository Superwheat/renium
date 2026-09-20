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
    std::array<std::uint64_t, 12> descriptor{};
    descriptor[0] = 123;
    descriptor[8] = address;
    for (unsigned index = 0; index < 32; ++index) {
        const std::string name(index % 2 ? 100 : 3, 'x');
        renium_functions::Input input{};
        input.descriptor = reinterpret_cast<std::uintptr_t>(descriptor.data());
        input.table = descriptor[0];
        input.field = 64;
        input.mode = 3;
        input.function = address;
        input.firstSize = static_cast<std::uint32_t>(name.size());
        std::vector<char> bytes(sizeof(input) + name.size());
        std::memcpy(bytes.data(), &input, sizeof(input));
        std::memcpy(bytes.data() + sizeof(input), name.data(), name.size());
        auto completion = renium_functions::Begin(&receiver, {}, bytes.data(), bytes.size(),
            [&](std::uint64_t pointer, void* output, std::size_t size) {
                if (pointer < input.descriptor || pointer + size > input.descriptor + sizeof(descriptor)) return false;
                std::memcpy(output, reinterpret_cast<void*>(pointer), size);
                return true;
            });
        if (completion->Wait(std::chrono::milliseconds(1)) != "https://developer.roblox.com/" + name
            || receiver.identity != identity) {
            std::fprintf(stderr, "Function return changed the receiver or response at call %u\n", index);
            return 1;
        }
    }
    return 0;
}
