#pragma once
#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <stdexcept>
#include <utility>

// Engine inline callbacks use a 24-byte capture and a tagged operation table.
// Keep allocations and destruction inside this module; Studio only moves the
// inline shared_ptr through our operations. No engine allocator is substituted.
namespace renium_tagged {
class Callback {
    alignas(8) unsigned char storage[24]{};
    std::uintptr_t tagged = 0;
    struct alignas(8) Operations {
        void(*invoke)(void*);
        void(*copy)(void*, const void*);
        void(*move)(void*, void*);
        void(*destroy)(void*);
    };
    using Local = std::shared_ptr<std::function<void()>>;
    static void Invoke(void* value) { (**static_cast<Local*>(value))(); }
    static void Copy(void* to, const void* from) { new(to) Local(*static_cast<const Local*>(from)); }
    static void Move(void* to, void* from) { new(to) Local(std::move(*static_cast<Local*>(from))); }
    static void Destroy(void* value) { static_cast<Local*>(value)->~shared_ptr(); }
    inline static const Operations operations{Invoke, Copy, Move, Destroy};
public:
    explicit Callback(std::function<void()> value) {
        static_assert(sizeof(Local) <= sizeof(storage));
        new(storage) Local(std::make_shared<std::function<void()>>(std::move(value)));
        tagged = reinterpret_cast<std::uintptr_t>(&operations) | 6;
    }
    Callback(const Callback&) = delete;
    Callback& operator=(const Callback&) = delete;
    ~Callback() { if (tagged) Destroy(storage); }
};
static_assert(sizeof(Callback) == 32);
}
