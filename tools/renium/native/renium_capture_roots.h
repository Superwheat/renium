#pragma once

struct CaptureIdentityRow
{
    unsigned char id[16]; // Native UniqueId getter's four LE words, unchanged.
    char debugId[48];     // GetDebugId(32), NUL terminated and zero padded.
    std::uint32_t parent; // Earlier row index; UINT32_MAX for selected roots.
    std::uint32_t reserved;
};
static_assert(sizeof(CaptureIdentityRow) == 72);

// Renium wire records only. All engine member offsets come from the existing
// instance-layout detector; no engine object is copied into these records.
struct CapturePathNode { SharedInstance object; std::uint64_t sibling; };
struct CaptureRootReceipt
{
    std::uint32_t status, row, sibling, subtree; // 0 captured, 1 missing, 2 moved
    CaptureIdentityRow parent;
};
struct CaptureRootRequest
{
    std::uint32_t depth, reserved;
    char expectedDebugId[48];
    CapturePathNode path[64];
    CaptureRootReceipt receipt;
};
struct CaptureRoots
{
    std::uint32_t count, reserved;
    CaptureRootRequest roots[256];
};
static_assert(sizeof(CaptureRootReceipt) == 88);
static_assert(offsetof(CaptureRootRequest, path) == 56);
static_assert(offsetof(CaptureRootRequest, receipt) == 1592);
static_assert(sizeof(CaptureRootRequest) == 1680);
static_assert(sizeof(CaptureRoots) == 430088);

// Check membership before dereferencing an address supplied by the host. The
// expected debug ID additionally fences deletion/reallocation and duplicate
// path replacements. A stale path is a root result, never a guessed new target.
template<class Children, class Self, class Parent>
bool CapturePathPresent(const CaptureRootRequest& root, void* model,
    Children children, Self self, Parent parent)
{
    if (!root.depth || root.depth > 64) return false;
    auto ancestor = model;
    for (std::uint32_t i = 0; i < root.depth; ++i) {
        const auto& node = root.path[i];
        const auto list = children(ancestor);
        if (node.sibling >= list.second) return false;
        const auto live = list.first[node.sibling];
        if (live.instance != node.object.instance || live.owner != node.object.owner)
            return false;
        const auto identity = self(live.instance);
        if (identity.instance != live.instance || identity.owner != live.owner ||
            parent(live.instance) != ancestor) return false;
        ancestor = live.instance;
    }
    return true;
}
