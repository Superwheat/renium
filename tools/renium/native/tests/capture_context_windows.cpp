#include "../renium_capture_context.h"
#include <array>
#include <cassert>
#include <fstream>
#include <iostream>
#include <limits>
#include <string>
#include <vector>
#include "../renium_capture_roots.h"
#include "../renium_reader_mutations.h"

static ReniumSerializerParams Fixture(std::uint32_t offset, std::uint64_t mask)
{
    ReniumSerializerParams p{};
    p.protocolVersion = ReniumSerializerVersion;
    p.captureMode = 1;
    p.contextFieldOffset = offset;
    p.dataModelInstanceSize = 0x188;
    p.contextPointerMask = mask;
    p.taskContext = 0x12345000;
    return p;
}

static void Refuses(ReniumSerializerParams p, const char* reason)
{
    try {
        // Invalid bindings must fail before any read, even at an unmapped model.
        CheckCaptureContext(p, nullptr);
        assert(false);
    } catch (const std::runtime_error& error) {
        assert(std::string(error.what()).find(reason) != std::string::npos);
        assert(p.contextRead == 0);
    }
}

static void SelectedRootTests()
{
    int model = 0, parent = 1, leaf = 2, owner = 3;
    CaptureRootRequest root{};
    root.depth = 2;
    root.path[0] = {{&parent, &owner}, 0};
    root.path[1] = {{&leaf, &owner}, 1};
    SharedInstance top[] = {{&parent, &owner}};
    SharedInstance children[] = {{&parent, &owner}, {&leaf, &owner}};
    auto present = [&] {
        return CapturePathPresent(root, &model, [&](void* object) {
            return object == &model ? std::make_pair(top, std::size_t(1))
                                    : std::make_pair(children, std::size_t(2));
        }, [&](void* object) { return SharedInstance{object, &owner}; },
        [&](void* object) -> void* { return object == &parent ? &model : &parent; });
    };
    assert(present());
    root.path[1].sibling = 0; assert(!present());
    root.path[1].sibling = 1;
    children[1].owner = nullptr; assert(!present());
    children[1].owner = &owner;
    top[0].instance = nullptr; assert(!present()); // no stale descendant dereference
    top[0].instance = &parent;
    root.depth = 0; assert(!present());
    root.depth = 65; assert(!present());
}

static void UnitTests()
{
    for (const auto offset : {0x48u, 0x58u, 0xa0u, 0x180u}) {
        for (const auto tag : {0ull, 1ull, 7ull, 15ull, 255ull}) {
            auto p = Fixture(offset, ~tag);
            std::array<unsigned char, 0x188> model{};
            const auto raw = p.taskContext | tag;
            std::memcpy(model.data() + offset, &raw, sizeof(raw));
            CheckCaptureContext(p, model.data());
            assert(p.contextRead == 1 && p.contextObserved == p.taskContext);
            const auto expected = p.taskContext;
            const auto changed = (expected + 0x1000) | tag;
            std::memcpy(model.data() + offset, &changed, sizeof(changed));
            try {
                CheckCaptureContext(p, model.data());
                assert(false);
            } catch (const std::runtime_error& error) {
                assert(std::string(error.what()) == "DataModel task context changed before capture");
                assert(p.contextRead == 1 && p.contextObserved == expected + 0x1000);
                assert(p.taskContext == expected); // never refresh submission's pointer
            }
        }
    }
    for (auto offset : {0u, 7u, 9u, 0x181u, 0x188u, 0xfffffff8u, 0xffffffffu}) {
        auto p = Fixture(offset, ~7ull);
        Refuses(p, "Invalid DataModel capture task-context binding");
    }
    for (auto size : {0u, 7u, 0x47u, 0x4fu}) {
        auto p = Fixture(0x48, ~7ull);
        p.dataModelInstanceSize = size;
        Refuses(p, "Invalid DataModel capture task-context binding");
    }
    for (auto mask : {0ull, 0xffffull, ~5ull}) {
        Refuses(Fixture(0x48, mask), "Invalid DataModel capture task-context binding");
    }
    for (auto pointer : {0ull, 1ull, 0x12345001ull, 0x800000000000ull}) {
        auto p = Fixture(0x48, ~7ull);
        p.taskContext = pointer;
        Refuses(p, "Invalid DataModel capture task-context binding");
    }
    for (auto version : {0u, 1u, 2u, 3u, 5u, 0xffffffffu}) {
        auto p = Fixture(0x48, ~7ull);
        p.protocolVersion = version;
        Refuses(p, "Incompatible native serializer protocol");
    }
    auto p = Fixture(0x48, ~7ull);
    p.status = 0xE004;
    const char* error = "DataModel task context changed before capture";
    std::memcpy(p.error, error, std::strlen(error));
    auto response = MakeCaptureResponse(p, p.status);
    assert(response.base.version == ReniumSerializerVersion && CaptureResponseSize(p) == 440);
    p.timeline.frequency = 1000;
    p.timeline.ticks[3] = 120;
    p.timeline.ticks[4] = 160;
    response = MakeCaptureResponse(p, p.status);
    assert(response.timeline.frequency == 1000);
    assert(response.timeline.ticks[3] == 120 && response.timeline.ticks[4] == 160);
    assert(response.base.status == p.status && response.base.exitCode == p.status);
    assert(std::string(response.base.error) == error);
    // Observation shares the parameter prefix but keeps its original wire header.
    p.captureMode = 0;
    response = MakeCaptureResponse(p, p.status);
    assert(response.base.version == 1 && CaptureResponseSize(p) == 320);
}

static void PrintLayout()
{
    const std::size_t layout[] = {
        ReniumSerializerVersion, sizeof(ReniumSerializerParams),
        offsetof(ReniumSerializerParams, protocolVersion),
        offsetof(ReniumSerializerParams, taskContext),
        offsetof(ReniumSerializerParams, contextFieldOffset),
        offsetof(ReniumSerializerParams, dataModelInstanceSize),
        offsetof(ReniumSerializerParams, contextPointerMask),
        offsetof(ReniumSerializerParams, contextObserved),
        offsetof(ReniumSerializerParams, contextRead),
        sizeof(CaptureContextResponse), offsetof(CaptureContextResponse, fieldOffset),
        offsetof(CaptureContextResponse, instanceSize), offsetof(CaptureContextResponse, pointerMask),
        offsetof(CaptureContextResponse, observed), offsetof(CaptureContextResponse, read),
    };
    for (const auto value : layout) std::cout << value << ' ';
}

static void ReaderMutationTests()
{
    using namespace renium_reader;
    struct FakeNode
    {
        void* parent = nullptr;
        void* owner = nullptr;
        void* selfPtr = nullptr;
        std::vector<SharedInstance> children;
        std::size_t childReads = 0;
        std::size_t parentReads = 0;
    };
    int ownerToken = 1, wrongOwner = 2;
    auto initNode = [&](FakeNode& node, void* parent) {
        node.parent = parent;
        node.owner = &ownerToken;
        node.selfPtr = &node;
    };
    auto getParent = [](void* ptr) -> void* {
        auto* n = static_cast<FakeNode*>(ptr);
        ++n->parentReads;
        return n->parent;
    };
    auto getSelf = [](void* ptr) -> SharedInstance {
        auto* n = static_cast<FakeNode*>(ptr);
        return {n->selfPtr, n->owner};
    };
    auto getChildren = [](void* ptr) -> std::pair<const SharedInstance*, std::size_t> {
        auto* n = static_cast<FakeNode*>(ptr);
        ++n->childReads;
        return {n->children.empty() ? nullptr : n->children.data(), n->children.size()};
    };

    // #6: Deep hierarchy (> 512 hops) stays Rooted; nullptr is Detached; cycle exceeds MaxRootedHops -> DepthExceeded.
    FakeNode dataModel;
    initNode(dataModel, nullptr);
    std::vector<FakeNode> deep(600);
    for (std::size_t i = 0; i < deep.size(); ++i)
        initNode(deep[i], i == 0 ? static_cast<void*>(&dataModel) : static_cast<void*>(&deep[i - 1]));
    assert(CheckRootedAtDataModel(&deep.back(), &dataModel, getParent) == RootedState::Rooted);

    FakeNode detached;
    initNode(detached, nullptr);
    assert(CheckRootedAtDataModel(&detached, &dataModel, getParent) == RootedState::Detached);

    FakeNode cycleA, cycleB;
    initNode(cycleA, &cycleB);
    initNode(cycleB, &cycleA);
    assert(CheckRootedAtDataModel(&cycleA, &dataModel, getParent) == RootedState::DepthExceeded);

    // ValidateChild: rejects bad self/owner/parent and accepts valid child.
    FakeNode parentNode, validChild, badSelf, badOwner, wrongParent;
    initNode(parentNode, &dataModel);
    initNode(validChild, &parentNode);
    initNode(badSelf, &parentNode); badSelf.selfPtr = &parentNode;
    initNode(badOwner, &parentNode); badOwner.owner = &wrongOwner;
    initNode(wrongParent, &dataModel);
    assert(ValidateChild(&parentNode, {&validChild, &ownerToken}, getSelf, getParent));
    assert(!ValidateChild(&parentNode, {nullptr, &ownerToken}, getSelf, getParent));
    assert(!ValidateChild(&parentNode, {&validChild, nullptr}, getSelf, getParent));
    assert(!ValidateChild(&parentNode, {&badSelf, &ownerToken}, getSelf, getParent));
    assert(!ValidateChild(&parentNode, {&badOwner, &ownerToken}, getSelf, getParent));
    assert(!ValidateChild(&parentNode, {&wrongParent, &ownerToken}, getSelf, getParent));

    BatchMutationTracker tracker;
    auto hasEngineCreated = [&](void* ptr) {
        for (const auto& item : tracker.engineCreated)
            if (item.instance == ptr) return true;
        return false;
    };

    // #4 + #5 + anchors + ValidateChild inside scan + failing addOwnerRef + equal-XOR swap:
    FakeNode service, historyTarget, anchorFolder;
    FakeNode batch1Item, batch1Detached, foreignBetween, anchorForeign;
    FakeNode batch2Item, engineMade, engineChild, anchorEngineChild, batch3Item;
    FakeNode invalidBadOwner, invalidWrongParent, unretainedValid;
    initNode(service, &dataModel);
    initNode(historyTarget, &dataModel);
    initNode(anchorFolder, &service);
    initNode(batch1Item, &service);
    initNode(batch1Detached, nullptr);
    initNode(foreignBetween, &service);
    initNode(anchorForeign, &anchorFolder);
    initNode(batch2Item, &service);
    initNode(engineMade, &service);
    initNode(engineChild, &engineMade);
    initNode(anchorEngineChild, &anchorFolder);
    initNode(batch3Item, &service);
    initNode(invalidBadOwner, &service); invalidBadOwner.owner = &wrongOwner;
    initNode(invalidWrongParent, &dataModel);
    initNode(unretainedValid, &service);

    std::vector<void*> targets = {&service, &historyTarget};
    std::vector<void*> anchors;
    std::vector<void*> allCreated;
    auto targetInst = [](void* ptr) { return ptr; };
    auto createdInst = [](void* ptr) { return ptr; };
    int rejectedOwnerToken = 99;
    unretainedValid.owner = &rejectedOwnerToken;
    std::size_t addOwnerCallsForInvalid = 0;
    auto addOwner = [&](void* owner) {
        if (owner == &wrongOwner) { ++addOwnerCallsForInvalid; return true; }
        if (owner == &rejectedOwnerToken) return false;
        return owner != nullptr;
    };

    // Batch 1: imports anchorFolder, batch1Item (rooted), and batch1Detached (detached).
    tracker.SnapshotBatchPreState(targets, 1, targetInst, anchors, getChildren);
    service.children.push_back({&anchorFolder, &ownerToken});
    service.children.push_back({&batch1Item, &ownerToken});
    allCreated.push_back(&anchorFolder);
    allCreated.push_back(&batch1Item);
    allCreated.push_back(&batch1Detached);
    tracker.ScanBatchEngineMutations(targets, 1, targetInst, allCreated, 0, 3, createdInst, &dataModel, getParent, getSelf, getChildren, addOwner);
    assert(tracker.createdRemoved.size() == 3 && tracker.createdRemoved[0] == 0 && tracker.createdRemoved[1] == 0 && tracker.createdRemoved[2] == 1);
    assert(tracker.engineCreated.empty());

    // Between batch 1 and batch 2: foreign children added to service and anchorFolder;
    // also detach batch1Item in batch 2 to verify cross-batch detachment is NOT listed
    // in createdRemoved (so plugin identity/verification catches it and fails closed).
    service.children.push_back({&foreignBetween, &ownerToken});
    anchorFolder.children.push_back({&anchorForeign, &ownerToken});
    anchors.push_back(&anchorFolder);
    batch1Item.childReads = 0;
    batch1Item.parentReads = 0;
    batch1Detached.parentReads = 0;

    // Batch 2: imports batch2Item; engine creates engineMade->engineChild in service and
    // anchorEngineChild in anchorFolder; also place invalid children & failing-addOwner child.
    tracker.SnapshotBatchPreState(targets, 1, targetInst, anchors, getChildren);
    batch1Item.parent = nullptr;
    service.children.push_back({&batch2Item, &ownerToken});
    service.children.push_back({&engineMade, &ownerToken});
    service.children.push_back({&invalidBadOwner, &ownerToken});
    service.children.push_back({&invalidWrongParent, &ownerToken});
    service.children.push_back({&unretainedValid, &rejectedOwnerToken});
    engineMade.children.push_back({&engineChild, &ownerToken});
    anchorFolder.children.push_back({&anchorEngineChild, &ownerToken});
    allCreated.push_back(&batch2Item);
    tracker.ScanBatchEngineMutations(targets, 1, targetInst, allCreated, 3, 4, createdInst, &dataModel, getParent, getSelf, getChildren, addOwner);
    // #4: Earlier non-anchor batch 1 instances are NOT re-walked during batch 2.
    assert(batch1Item.childReads == 0 && batch1Item.parentReads == 0 && batch1Detached.parentReads == 0);
    assert(tracker.createdRemoved[1] == 0); // cross-batch detachment stays unlisted (fails closed)
    // ValidateChild inside scan rejected invalidBadOwner & invalidWrongParent before addOwnerRef;
    // failing addOwnerRef excluded unretainedValid.
    assert(addOwnerCallsForInvalid == 0);
    assert(!hasEngineCreated(&invalidBadOwner));
    assert(!hasEngineCreated(&invalidWrongParent));
    assert(!hasEngineCreated(&unretainedValid));
    // #5 + anchor branch: foreignBetween & anchorForeign excluded; engineMade, engineChild, anchorEngineChild detected.
    assert(tracker.engineCreated.size() == 3);
    assert(hasEngineCreated(&engineMade));
    assert(hasEngineCreated(&engineChild));
    assert(hasEngineCreated(&anchorEngineChild));
    assert(!hasEngineCreated(&foreignBetween));
    assert(!hasEngineCreated(&anchorForeign));

    // Batch 3: service and anchor mutate again; foreign children stay excluded across later batches.
    tracker.SnapshotBatchPreState(targets, 1, targetInst, anchors, getChildren);
    service.children.push_back({&batch3Item, &ownerToken});
    allCreated.push_back(&batch3Item);
    tracker.ScanBatchEngineMutations(targets, 1, targetInst, allCreated, 4, 5, createdInst, &dataModel, getParent, getSelf, getChildren, addOwner);
    assert(!hasEngineCreated(&foreignBetween));
    assert(!hasEngineCreated(&anchorForeign));
    assert(tracker.engineCreated.size() == 3);

    // Equal-XOR pointer swap test: (0x1000 ^ 0x1040) == (0x1080 ^ 0x10C0) == 0x40,
    // yet MixPointer-based ChildDigest (sum + xorsum) distinguishes the two pairs.
    void* const p0 = reinterpret_cast<void*>(static_cast<std::uintptr_t>(0x1000));
    void* const p1 = reinterpret_cast<void*>(static_cast<std::uintptr_t>(0x1040));
    void* const p2 = reinterpret_cast<void*>(static_cast<std::uintptr_t>(0x1080));
    void* const p3 = reinterpret_cast<void*>(static_cast<std::uintptr_t>(0x10C0));
    assert((reinterpret_cast<std::uintptr_t>(p0) ^ reinterpret_cast<std::uintptr_t>(p1))
        == (reinterpret_cast<std::uintptr_t>(p2) ^ reinterpret_cast<std::uintptr_t>(p3)));
    SharedInstance pair01[] = {{p0, &ownerToken}, {p1, &ownerToken}};
    SharedInstance pair23[] = {{p2, &ownerToken}, {p3, &ownerToken}};
    assert(ComputeChildDigestRange(pair01, 2) != ComputeChildDigestRange(pair23, 2));

    // Deep-chain memoization test: 2,000-node chain in one batch takes O(N) parent reads, not O(N^2).
    std::vector<FakeNode> chain(2000);
    for (std::size_t i = 0; i < chain.size(); ++i)
        initNode(chain[i], i == 0 ? static_cast<void*>(&service) : static_cast<void*>(&chain[i - 1]));
    BatchMutationTracker deepTracker;
    std::vector<void*> deepCreated;
    deepCreated.reserve(chain.size());
    for (auto& node : chain) deepCreated.push_back(&node);
    std::vector<void*> noAnchors;
    deepTracker.SnapshotBatchPreState(targets, 1, targetInst, noAnchors, getChildren);
    deepTracker.ScanBatchEngineMutations(targets, 1, targetInst, deepCreated, 0, deepCreated.size(), createdInst, &dataModel, getParent, getSelf, getChildren, addOwner);
    std::size_t totalParentReads = 0;
    for (const auto& node : chain) totalParentReads += node.parentReads;
    assert(totalParentReads <= chain.size() * 2);

    // #6 fail-closed: DepthExceeded inside ScanBatchEngineMutations throws instead of marking removed.
    allCreated.push_back(&cycleA);
    bool threwDepth = false;
    try {
        tracker.ScanBatchEngineMutations(targets, 1, targetInst, allCreated, 5, 6, createdInst, &dataModel, getParent, getSelf, getChildren, addOwner);
    } catch (const std::runtime_error& err) {
        threwDepth = std::string(err.what()).find("depth limit") != std::string::npos;
    }
    assert(threwDepth);
}

int main(int argc, char** argv)
{
    UnitTests();
    SelectedRootTests();
    ReaderMutationTests();
    if (argc == 1) { PrintLayout(); return 0; }
    assert(argc == 3);
    ReniumSerializerParams p{};
    std::ifstream input(argv[1], std::ios::binary);
    input.read(reinterpret_cast<char*>(&p), sizeof(p));
    assert(input.gcount() == sizeof(p) && input.peek() == EOF);
    ValidateCaptureContextBinding(p);
    assert(p.contextRead == 0 && p.contextObserved == 0);
    std::vector<unsigned char> model(p.dataModelInstanceSize);
    const auto raw = p.taskContext | ~p.contextPointerMask;
    std::memcpy(model.data() + p.contextFieldOffset, &raw, sizeof(raw));
    CheckCaptureContext(p, model.data());
    p.status = 4;
    const auto response = MakeCaptureResponse(p, 0);
    std::ofstream output(argv[2], std::ios::binary);
    output.write(reinterpret_cast<const char*>(&p), sizeof(p));
    output.write(reinterpret_cast<const char*>(&response), sizeof(response));
    return output.good() ? 0 : 1;
}
