#pragma once

#include <cstddef>
#include <cstdint>
#include <stdexcept>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>
#include "renium_capture_roots.h"

namespace renium_reader {

enum class RootedState { Rooted, Detached, DepthExceeded };

inline constexpr std::size_t MaxRootedHops = 65536;

inline std::uint64_t MixPointer(void* ptr) noexcept
{
    std::uint64_t x = static_cast<std::uint64_t>(reinterpret_cast<std::uintptr_t>(ptr));
    x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ULL;
    x = (x ^ (x >> 27)) * 0x94d049bb133111ebULL;
    return x ^ (x >> 31);
}

template<class GetParent>
RootedState CheckRootedAtDataModel(void* instance, void* dataModel, GetParent&& getParent)
{
    void* current = instance;
    for (std::size_t hops = 0; hops < MaxRootedHops; ++hops)
    {
        if (!current) return RootedState::Detached;
        if (current == dataModel) return RootedState::Rooted;
        current = getParent(current);
    }
    return RootedState::DepthExceeded;
}

template<class GetParent>
RootedState CheckRootedAtDataModelMemo(
    void* instance,
    void* dataModel,
    std::unordered_set<void*>& knownRooted,
    std::vector<void*>& scratchPath,
    GetParent&& getParent)
{
    scratchPath.clear();
    void* current = instance;
    for (std::size_t hops = 0; hops < MaxRootedHops; ++hops)
    {
        if (!current) return RootedState::Detached;
        if (current == dataModel || knownRooted.find(current) != knownRooted.end())
        {
            for (void* visited : scratchPath) knownRooted.insert(visited);
            return RootedState::Rooted;
        }
        scratchPath.push_back(current);
        current = getParent(current);
    }
    return RootedState::DepthExceeded;
}

template<class GetSelf, class GetParent>
bool ValidateChild(void* container, const SharedInstance& child, GetSelf&& getSelf, GetParent&& getParent)
{
    if (!child.instance || !child.owner) return false;
    const auto self = getSelf(child.instance);
    if (self.instance != child.instance || self.owner != child.owner) return false;
    return getParent(child.instance) == container;
}

struct ChildDigest
{
    std::size_t count = 0;
    std::uint64_t sum = 0;
    std::uint64_t xorsum = 0;

    bool operator==(const ChildDigest& other) const noexcept
    {
        return count == other.count && sum == other.sum && xorsum == other.xorsum;
    }
    bool operator!=(const ChildDigest& other) const noexcept { return !(*this == other); }
};

inline ChildDigest ComputeChildDigestRange(const SharedInstance* begin, std::size_t count) noexcept
{
    std::uint64_t sum = 0;
    std::uint64_t xorsum = 0;
    for (std::size_t c = 0; c < count; ++c)
    {
        const auto mixed = MixPointer(begin[c].instance);
        sum += mixed;
        xorsum ^= mixed;
    }
    return {count, sum, xorsum};
}

template<class GetChildren>
ChildDigest ComputeChildDigest(void* container, GetChildren&& getChildren)
{
    const auto [begin, count] = getChildren(container);
    return ComputeChildDigestRange(begin, count);
}

struct BatchMutationTracker
{
    std::vector<std::uint8_t> createdRemoved;
    std::vector<ChildDigest> targetLastDigests;
    std::vector<ChildDigest> targetBatchPreDigests;
    std::unordered_map<void*, ChildDigest> anchorLastDigests;
    std::unordered_map<void*, ChildDigest> anchorBatchPreDigests;
    std::unordered_set<void*> foreignChildren;
    std::unordered_set<void*> knownInstances;
    std::vector<SharedInstance> engineCreated;

    template<class GetChildren>
    void RecordForeignChildren(void* container, GetChildren&& getChildren)
    {
        const auto [begin, count] = getChildren(container);
        for (std::size_t c = 0; c < count; ++c)
        {
            void* inst = begin[c].instance;
            if (inst && knownInstances.find(inst) == knownInstances.end())
                foreignChildren.insert(inst);
        }
    }

    template<class TargetVec, class GetTargetInstance, class GetChildren>
    void SnapshotBatchPreState(
        const TargetVec& targets,
        std::size_t skipTargetIdx,
        GetTargetInstance&& getTargetInstance,
        const std::vector<void*>& anchors,
        GetChildren&& getChildren)
    {
        if (knownInstances.empty())
        {
            knownInstances.reserve(targets.size() * 2 + 1024);
            for (const auto& t : targets) knownInstances.insert(getTargetInstance(t));
        }
        const bool firstBatch = targetLastDigests.empty();
        targetBatchPreDigests.resize(targets.size());
        if (firstBatch) targetLastDigests.resize(targets.size());
        for (std::size_t idx = 0; idx < targets.size(); ++idx)
        {
            if (idx == skipTargetIdx) continue;
            void* container = getTargetInstance(targets[idx]);
            const auto digest = ComputeChildDigest(container, getChildren);
            targetBatchPreDigests[idx] = digest;
            if (firstBatch || digest != targetLastDigests[idx])
            {
                // Record initial or between-batch foreign children persistently for the whole import.
                RecordForeignChildren(container, getChildren);
            }
        }
        anchorBatchPreDigests.clear();
        for (void* anchor : anchors)
        {
            if (knownInstances.find(anchor) != knownInstances.end())
            {
                bool isTarget = false;
                for (std::size_t idx = 0; idx < targets.size(); ++idx)
                    if (idx != skipTargetIdx && getTargetInstance(targets[idx]) == anchor)
                    { isTarget = true; break; }
                if (isTarget) continue;
            }
            const auto digest = ComputeChildDigest(anchor, getChildren);
            anchorBatchPreDigests[anchor] = digest;
            const auto prev = anchorLastDigests.find(anchor);
            if (prev == anchorLastDigests.end() || digest != prev->second)
                RecordForeignChildren(anchor, getChildren);
        }
    }

    template<
        class TargetVec,
        class GetTargetInstance,
        class CreatedVec,
        class GetCreatedInstance,
        class GetParent,
        class GetSelf,
        class GetChildren,
        class AddOwnerRef>
    void ScanBatchEngineMutations(
        const TargetVec& targets,
        std::size_t skipTargetIdx,
        GetTargetInstance&& getTargetInstance,
        const CreatedVec& allCreated,
        std::size_t batchStart,
        std::size_t batchEnd,
        GetCreatedInstance&& getCreatedInstance,
        void* dataModel,
        GetParent&& getParent,
        GetSelf&& getSelf,
        GetChildren&& getChildren,
        AddOwnerRef&& addOwnerRef)
    {
        if (batchEnd > createdRemoved.size()) createdRemoved.resize(batchEnd, 0);
        for (std::size_t i = batchStart; i < batchEnd; ++i)
            knownInstances.insert(getCreatedInstance(allCreated[i]));

        std::unordered_set<void*> knownRooted;
        knownRooted.reserve((batchEnd - batchStart) + targets.size() + 16);
        knownRooted.insert(dataModel);
        std::vector<void*> scratchPath;
        for (std::size_t i = batchStart; i < batchEnd; ++i)
        {
            const auto state = CheckRootedAtDataModelMemo(
                getCreatedInstance(allCreated[i]),
                dataModel,
                knownRooted,
                scratchPath,
                getParent);
            if (state == RootedState::DepthExceeded)
                throw std::runtime_error("Native import hierarchy exceeded depth limit");
            if (state == RootedState::Detached)
                createdRemoved[i] = 1;
        }

        std::vector<void*> stack;
        auto inspectRange = [&](void* container, const SharedInstance* begin, std::size_t startIdx, std::size_t count, bool checkForeign) {
            for (std::size_t c = startIdx; c < count; ++c)
            {
                const auto& child = begin[c];
                if (!child.instance) continue;
                if (knownInstances.find(child.instance) != knownInstances.end()) continue;
                if (checkForeign && foreignChildren.find(child.instance) != foreignChildren.end()) continue;
                if (!ValidateChild(container, child, getSelf, getParent)) continue;
                knownInstances.insert(child.instance);
                if (addOwnerRef(child.owner))
                    engineCreated.push_back(child);
                stack.push_back(child.instance);
            }
        };
        auto inspectContainer = [&](void* container, bool checkForeign) {
            const auto [begin, count] = getChildren(container);
            inspectRange(container, begin, 0, count, checkForeign);
        };
        auto inspectDeltaContainer = [&](void* container, const ChildDigest& preDigest) -> ChildDigest {
            const auto [begin, count] = getChildren(container);
            const auto digest = ComputeChildDigestRange(begin, count);
            if (digest != preDigest)
            {
                // If the existing prefix is unchanged, only inspect appended children (O(batch)).
                std::size_t startIdx = 0;
                if (preDigest.count > 0 && count >= preDigest.count
                    && ComputeChildDigestRange(begin, preDigest.count) == preDigest)
                {
                    startIdx = preDigest.count;
                }
                inspectRange(container, begin, startIdx, count, true);
            }
            return digest;
        };

        for (std::size_t idx = 0; idx < targets.size(); ++idx)
        {
            if (idx == skipTargetIdx) continue;
            void* container = getTargetInstance(targets[idx]);
            targetLastDigests[idx] = inspectDeltaContainer(container, targetBatchPreDigests[idx]);
        }
        for (const auto& [anchor, preDigest] : anchorBatchPreDigests)
            anchorLastDigests[anchor] = inspectDeltaContainer(anchor, preDigest);

        for (std::size_t i = batchStart; i < batchEnd; ++i)
            if (!createdRemoved[i]) inspectContainer(getCreatedInstance(allCreated[i]), false);

        while (!stack.empty())
        {
            void* node = stack.back();
            stack.pop_back();
            inspectContainer(node, false);
        }
    }

    template<class ReleaseOwnerRef>
    void ReleaseEngineCreated(ReleaseOwnerRef&& releaseOwnerRef) noexcept
    {
        for (const auto& item : engineCreated) releaseOwnerRef(item.owner);
        engineCreated.clear();
    }
};

}
