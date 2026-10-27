#pragma once
#include <functional>
#include <vector>
#include "renium_play_loader_windows.h"
#include "renium_continuation_windows.h"

// One immutable launch scope is shared by request dispatch and arrival. It is
// captured at the original Edit controller before the new model runs scripts.
namespace renium_play_owner {
using Variant = renium_play_loader::VariantBuffer;
using Instance = std::shared_ptr<void>;
using Execute = void(*)(void*, Variant, std::function<void(Variant)>, std::function<void(std::string)>);
// Rust qualifies this ABI at the reflected dispatch before installing hooks.
using TaggedExecute = void(*)(void*, Variant, renium_continuation::Function<void(Variant)>, renium_continuation::Function<void(std::string)>);
struct Binding {
    std::uint64_t sourceModel, sourceTest, getArgs, parentOffset, childrenOffset, classOffset;
    std::uint64_t placeField, universeField, tableType, getAttribute, stringTypeTable, selfOffset;
    std::uint64_t contextField, contextMask, contextTable, submitTask;
};
struct Scope {
    Binding binding{};
    std::string nonce;
    std::uint64_t place = 0, universe = 0;
    std::atomic<std::shared_ptr<Variant>> arguments;
    std::atomic<bool> active{true};
};
inline bool Class(void* instance, std::uint64_t offset, const char* expected) {
    const auto descriptor = renium_play_loader::Word(instance, offset);
    const auto name = renium_play_loader::Word(reinterpret_cast<void*>(descriptor), 8);
    return *reinterpret_cast<const std::string*>(name) == expected;
}
inline const std::vector<Instance>* Children(void* instance, std::uint64_t offset, std::size_t maximum) {
    auto children = reinterpret_cast<const std::vector<Instance>*>(renium_play_loader::Word(instance, offset));
    if (children && children->size() > maximum) throw std::runtime_error("Teleport ownership hierarchy exceeds its bound");
    return children;
}
inline void Capture(const std::shared_ptr<Scope>& scope, void* receiver, const Variant& input) {
    if (!scope || !scope->active.load() || receiver != reinterpret_cast<void*>(scope->binding.sourceTest)
        || input.type != scope->binding.tableType) return;
    renium_play_loader::Request launch{};
    launch.getAttribute = scope->binding.getAttribute;
    launch.sourceInstance = scope->binding.sourceModel;
    launch.stringTypeTable = scope->binding.stringTypeTable;
    strcpy_s(launch.launchNonce, scope->nonce.c_str());
    if (renium_play_loader::MatchesLaunch(launch)) scope->arguments.store(std::make_shared<Variant>(input));
}
inline void* Model(void* instance, const Binding& binding) {
    for (unsigned depth = 0; instance && depth < 4; ++depth) {
        if (Class(instance, binding.classOffset, "DataModel")) return instance;
        instance = reinterpret_cast<void*>(renium_play_loader::Word(instance, binding.parentOffset));
    }
    return nullptr;
}
inline void* Owns(const Scope& scope, void* instance, bool client = false) {
    const auto arguments = scope.arguments.load();
    if (!scope.active.load()) return nullptr;
    const auto& binding = scope.binding;
    auto model = Model(instance, binding);
    if (!model || model == reinterpret_cast<void*>(binding.sourceModel)
        || renium_play_loader::Word(model, binding.placeField) != scope.place
        || renium_play_loader::Word(model, binding.universeField) != scope.universe) return nullptr;
    auto roots = Children(model, binding.childrenOffset, 256);
    if (!roots) return nullptr;
    void* test = nullptr;
    bool networkClient = false;
    for (const auto& child : *roots) {
        if (Class(child.get(), binding.classOffset, "StudioTestService")) test = child.get();
        if (Class(child.get(), binding.classOffset, "NetworkClient")) networkClient = true;
    }
    if (!test || (client && !networkClient)) return nullptr;
    if (!arguments) {
        // Studio's own Play controls bypass ExecutePlayModeAsync and do not
        // copy DataModel attributes. Hold a same-process source-server call
        // until the daemon binds its OS-verified runtime relay. The relay's
        // nonce is still mandatory before anything can be accepted.
        if (networkClient) return nullptr;
        renium_play_loader::Request launch{};
        launch.getAttribute = binding.getAttribute;
        launch.sourceInstance = binding.sourceModel;
        launch.stringTypeTable = binding.stringTypeTable;
        strcpy_s(launch.launchNonce, scope.nonce.c_str());
        return renium_play_loader::MatchesLaunch(launch) ? model : nullptr;
    }
    Variant actual;
    reinterpret_cast<void*(*)(void*, void*)>(binding.getArgs)(test, &actual);
    return actual.type == binding.tableType && arguments->type == binding.tableType
        && renium_play_loader::Word(actual.payload, 0) == renium_play_loader::Word(arguments->payload, 0) ? model : nullptr;
}
inline std::shared_ptr<void> Hold(void* model, const Binding& binding) {
    const auto owner = reinterpret_cast<void*>(renium_play_loader::Word(model, binding.selfOffset + 8));
    if (!owner || !AddOwnerReference(owner)) throw std::runtime_error("Teleport model ended before dispatch");
    return std::shared_ptr<void>(owner, ReleaseOwnerReference);
}
inline bool Submit(void* model, const Binding& binding, std::function<void()> work) {
    const auto context = renium_play_loader::Word(model, binding.contextField) & binding.contextMask;
    if (!context || renium_play_loader::Word(reinterpret_cast<void*>(context), 0) != binding.contextTable)
        throw std::runtime_error("Teleport model has an incompatible task context");
    return reinterpret_cast<bool(*)(void*, std::function<void()>*, std::uint32_t)>(binding.submitTask)(reinterpret_cast<void*>(context), &work, 1);
}
}
