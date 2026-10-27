#pragma once
#include <atomic>
#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <utility>

// Hidden BoundYieldFuncDesc arguments are not described by reflection RTTI.
// Rust certifies their constructors and operation tables before configuring
// this helper. The selection is process-scoped and cannot change under hooks.
namespace renium_continuation {
enum class Abi : std::uint32_t { Unknown, Msvc, Tagged };
inline std::atomic<Abi> abi{Abi::Unknown}, taskAbi{Abi::Unknown};
inline bool Tagged() {
    const auto value = abi.load();
    if (value == Abi::Unknown) throw std::runtime_error("Native continuations were not verified");
    return value == Abi::Tagged;
}

// Inline storage plus a tagged operations pointer. Bit 1 denotes inline
// storage, bit 2 a nontrivial callable with invoke/copy/move/destroy entries.
// Invocation forwards arguments by reference; the owning function call is
// still responsible for their destruction. In particular strings and shared
// pointers must not be destroyed twice across the DLL boundary.
template<class T> class Function;
template<class... Args> class Function<void(Args...)> {
    alignas(8) unsigned char storage[24]{};
    std::uintptr_t tagged = 0;
    struct alignas(8) Operations {
        void(*invoke)(void*, Args*...);
        void(*copy)(void*, const void*);
        void(*move)(void*, void*);
        void(*destroy)(void*);
    };
    const Operations* Table() const { return reinterpret_cast<const Operations*>(tagged & ~std::uintptr_t{7}); }
    void* Data() const { return tagged & 2 ? const_cast<unsigned char*>(storage) : *reinterpret_cast<void* const*>(storage); }
    void Reset() noexcept {
        const auto previous = tagged;
        if (!(previous & ~std::uintptr_t{7})) return;
        auto data = Data();
        tagged = 0;
        if (previous & 4) reinterpret_cast<const Operations*>(previous & ~std::uintptr_t{7})->destroy(data);
        if (!(previous & 2)) ::operator delete(data);
    }
    void Move(Function& other) noexcept {
        tagged = other.tagged;
        if ((tagged & 6) == 6) {
            Table()->move(storage, other.storage);
            Table()->destroy(other.storage);
        } else std::memcpy(storage, other.storage, sizeof(storage));
        other.tagged = 0;
    }
    using Local = std::shared_ptr<std::function<void(Args...)>>;
    static void InvokeLocal(void* value, Args*... args) { (**static_cast<Local*>(value))(std::move(*args)...); }
    static void CopyLocal(void* to, const void* from) { new(to) Local(*static_cast<const Local*>(from)); }
    static void MoveLocal(void* to, void* from) { new(to) Local(std::move(*static_cast<Local*>(from))); }
    static void DestroyLocal(void* value) { static_cast<Local*>(value)->~shared_ptr(); }
    inline static const Operations local{InvokeLocal, CopyLocal, MoveLocal, DestroyLocal};
public:
    Function() = default;
    Function(std::function<void(Args...)> value) {
        if (!value) return;
        new(storage) Local(std::make_shared<std::function<void(Args...)>>(std::move(value)));
        tagged = reinterpret_cast<std::uintptr_t>(&local) | 6;
    }
    Function(const Function& other) {
        if (!other) return;
        if (other.tagged & 2) {
            if (other.tagged & 4) other.Table()->copy(storage, other.storage);
            else std::memcpy(storage, other.storage, sizeof(storage));
        } else {
            throw std::runtime_error("Heap-stored native continuations were not verified");
        }
        tagged = other.tagged;
    }
    Function(Function&& other) noexcept { Move(other); }
    Function& operator=(Function other) noexcept { Reset(); Move(other); return *this; }
    ~Function() { Reset(); }
    explicit operator bool() const { return (tagged & ~std::uintptr_t{7}) != 0; }
    void operator()(Args... args) const {
        if (!*this) throw std::bad_function_call();
        const auto invoke = Table()->invoke;
        invoke(Data(), &args...);
    }
};
inline bool SubmitTask(std::uintptr_t address, void* context, std::function<void()>* work, std::uint32_t mode) {
    if (taskAbi.load() == Abi::Tagged) {
        Function<void()> callback(std::move(*work));
        return reinterpret_cast<bool(*)(void*, Function<void()>*, std::uint32_t)>(address)(context, &callback, mode);
    }
    if (taskAbi.load() != Abi::Msvc) throw std::runtime_error("Native task continuations were not verified");
    return reinterpret_cast<bool(*)(void*, std::function<void()>*, std::uint32_t)>(address)(context, work, mode);
}
struct SubmitDataModelTask {
    std::uintptr_t address;
    explicit SubmitDataModelTask(std::uintptr_t address) : address(address) {}
    bool operator()(void* context, std::function<void()>* work, std::uint32_t mode) const { return SubmitTask(address, context, work, mode); }
};
static_assert(sizeof(Function<void(std::string)>) == 32);
static_assert(sizeof(std::function<void(std::string)>) == 64);
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumPlayContinuations(void* input) {
    const auto value = *static_cast<const std::uint32_t*>(input);
    if (value != 1 && value != 2) return 1;
    auto expected = renium_continuation::Abi::Unknown;
    const auto selected = static_cast<renium_continuation::Abi>(value);
    return renium_continuation::abi.compare_exchange_strong(expected, selected) || expected == selected ? 0 : 2;
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumTaskContinuations(void* input) {
    const auto value = *static_cast<const std::uint32_t*>(input);
    if (value != 1 && value != 2) return 1;
    auto expected = renium_continuation::Abi::Unknown;
    const auto selected = static_cast<renium_continuation::Abi>(value);
    return renium_continuation::taskAbi.compare_exchange_strong(expected, selected) || expected == selected ? 0 : 2;
}
