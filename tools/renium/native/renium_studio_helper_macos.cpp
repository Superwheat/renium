#define _DARWIN_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cctype>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <dispatch/dispatch.h>
#include <dlfcn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <exception>
#include <functional>
#include <limits>
#include <limits.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <mach/vm_region.h>
#include <mach/vm_statistics.h>
#include <mach/vm_param.h>
#include <libkern/OSCacheControl.h>
#include <memory>
#include <mutex>
#include <pthread.h>
#include <sstream>
#include <stdexcept>
#include <string>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <thread>
#include <unistd.h>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>
#include "renium_studio_history.h"
#include "renium_studio_terrain.h"
#include "renium_playtest_macos.h"
#include "renium_test_server_macos.h"
#include "renium_teleport_dispatch_macos.h"
#include "renium_play_join_macos.h"
#include "renium_teleport_gui_macos.h"
#include "renium_test_client_macos.h"
#include "renium_route_launch_macos.h"
#include "renium_test_join_macos.h"

extern "C" bool ReniumArmLaunchGuard();
void ReniumTeleportCaption(const std::string&, std::vector<unsigned char>&);
void ReniumStudioAction(const std::string&, std::vector<unsigned char>&);
extern "C" void ReniumInitializeLaunchGuard();
extern "C" bool ReniumStudioAudio(unsigned, unsigned*, unsigned*, unsigned*, bool*, char*, std::size_t);

#if defined(_WIN32)
extern "C" int unsetenv(const char*);
#endif

struct Request
{
    std::uint32_t magic;
    std::uint32_t version;
    std::uint32_t command;
    std::uint32_t pathLength;
    std::uint32_t titleLength;
    std::uint32_t reserved;
    std::uint64_t factoryRva;
    std::uint64_t executeRva;
    unsigned char imageUuid[16];
};

struct Response
{
    std::uint32_t magic;
    std::uint32_t status;
    std::uint64_t outputSize;
    std::uint64_t elapsedMicros;
    char error[512];
};

struct StudioShared
{
    void* value;
    void* owner;
    ~StudioShared() {}
};

struct StudioQString
{
    void* data;
    ~StudioQString() {}
};

struct SharedInstance
{
    void* instance;
    void* owner;
};

struct DataModelCandidate
{
    void* outer;
    std::size_t instanceOffset;
    std::size_t childrenOffset;
    std::uint32_t rootMask;
    std::size_t rootCount;
    std::string name;
};

struct DataModelScanStats
{
    std::size_t pointers = 0;
    std::size_t outerRtti = 0;
    std::size_t selfInstances = 0;
    std::size_t instanceRtti = 0;
    std::size_t childVectors = 0;
    std::size_t requiredRoots = 0;
    std::uint64_t collectUs = 0, rttiNanos = 0, rttiReads = 0;
    std::string vectorDetails;
};

static constexpr std::uint32_t Magic = 0x4d4e4552;
static constexpr std::uint32_t Version = 19;
// Command 3 does not use factoryRva as a function address. Its top bit opts in
// to per-candidate clocks; older helpers ignore it and keep the same payloads.
static constexpr std::uint64_t PropertyPhaseTimingFlag = std::uint64_t{1} << 63;
static constexpr std::size_t DataModelInstanceOffsetMin = 0x100;
static constexpr std::size_t DataModelInstanceOffsetMax = 0x400;
static constexpr std::size_t InstanceClassDescriptorOffset = 0x18;
static constexpr std::size_t InstanceChildrenOffsetMin = 0x40;
static constexpr std::size_t InstanceChildrenOffsetMax = 0xc0;
static constexpr std::size_t InstanceNameOffsetMin = 0x20;
static constexpr std::size_t InstanceNameOffsetMax = 0x400;
static std::mutex SerializeMutex;
// Never hold this cache lock across an engine call or completion wait. In
// particular, cold property lookup takes it on Studio's UI thread.
static std::mutex DataModelCacheMutex;
struct DataModelLayout
{
    std::size_t instanceOffset = 0;
    std::size_t childrenOffset = 0;
};
static char SocketPath[sizeof(((sockaddr_un*)nullptr)->sun_path)]{};
static void* CachedDataModel = nullptr;
static std::size_t CachedDataModelInstanceOffset = 0;
static std::size_t CachedDataModelChildrenOffset = 0;
static std::atomic<std::size_t> CachedInstanceNameOffset{0};
static std::atomic<std::uint64_t> CachedInstanceNameLayout{0};

struct InstanceNameLayout
{
    std::size_t offset = 0;
    std::size_t nested = 0;
    bool doubleIndirect = false;
    bool cString = false;
};

static std::uint64_t PackInstanceNameLayout(const InstanceNameLayout& layout)
{
    return (static_cast<std::uint64_t>(layout.offset) << 16) |
        (static_cast<std::uint64_t>(layout.nested) << 4) | (layout.doubleIndirect ? 4 : 0) |
        (layout.cString ? 2 : 0) | 1;
}

static bool LoadInstanceNameLayout(InstanceNameLayout& layout)
{
    const auto packed = CachedInstanceNameLayout.load(std::memory_order_relaxed);
    if (!(packed & 1))
        return false;
    layout.offset = static_cast<std::size_t>(packed >> 16);
    layout.nested = static_cast<std::size_t>((packed >> 4) & 0xFFF);
    layout.doubleIndirect = (packed & 4) != 0;
    layout.cString = (packed & 2) != 0;
    return true;
}

static std::string CachedDataModelTitle;

// Optional bounded diagnostics: offsets are microseconds from RPC handling,
// zero means not reached. Atomics allow a timeout receipt while a late callback
// still owns the timing record. No engine pointers or property values escape.
struct PropertyTiming
{
    const bool detailed;
    explicit PropertyTiming(bool measure = false) : detailed(measure) {}
    const std::chrono::steady_clock::time_point started = std::chrono::steady_clock::now();
    std::atomic<std::uint64_t> lockUs{0}, scanUs{0}, cache{0}, modelCache{0};
    std::atomic<std::uint64_t> collectUs{0}, rttiUs{0}, rttiReads{0}, pointers{0};
    std::atomic<std::uint64_t> uiQueued{0}, uiBegin{0}, uiEnd{0}, submit{0}, runBegin{0}, runEnd{0};
    std::uint64_t now() const
    {
        return 1 + std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now() - started).count();
    }
    void finish(Response& response) const
    {
        response.elapsedMicros = now() - 1;
        const auto length = strnlen(response.error, sizeof(response.error));
        std::snprintf(response.error + length, sizeof(response.error) - length,
            "%spt1 lock=%llu scan=%llu cache=%llu model=%llu ui=%llu/%llu/%llu submit=%llu run=%llu/%llu collect=%llu rtti=%llu/%llu/%u ptrs=%llu",
            length ? " | " : "", lockUs.load(), scanUs.load(), cache.load(), modelCache.load(),
            uiQueued.load(), uiBegin.load(), uiEnd.load(), submit.load(), runBegin.load(), runEnd.load(),
            collectUs.load(), rttiUs.load(), rttiReads.load(), static_cast<unsigned>(detailed), pointers.load());
    }
};

static void SetError(Response& response, const std::string& error);
static void ReleaseQString(StudioQString& value);
static bool ReadMemory(std::uintptr_t address, void* output, std::size_t size);
template <typename T>
static bool ReadValue(std::uintptr_t address, T& value);

static_assert(sizeof(Request) == 56);
static_assert(sizeof(Response) == 536);

static bool ReadMemory(std::uintptr_t address, void* output, std::size_t size)
{
    // Use the platform's exported user-VM limit, not a Studio address heuristic.
    // Scanned image words often contain code/string bits rather than pointers;
    // their guaranteed-invalid Mach reads otherwise dominate cold discovery.
    if (size && (address >= MACH_VM_MAX_ADDRESS || size > MACH_VM_MAX_ADDRESS - address))
        return false;
    mach_vm_size_t read = 0;
    return mach_vm_read_overwrite(
               mach_task_self(),
               address,
               size,
               reinterpret_cast<mach_vm_address_t>(output),
               &read) == KERN_SUCCESS &&
        read == size;
}

template <typename T>
static bool ReadValue(std::uintptr_t address, T& value)
{
    return ReadMemory(address, &value, sizeof(value));
}

static bool ReadCString(std::uintptr_t address, std::string& value, std::size_t limit)
{
    value.clear();
    for (std::size_t offset = 0; offset < limit; offset += 64)
    {
        char bytes[64];
        const auto count = std::min(sizeof(bytes), limit - offset);
        if (!ReadMemory(address + offset, bytes, count))
            return false;
        const auto* end = static_cast<const char*>(memchr(bytes, 0, count));
        value.append(bytes, static_cast<std::size_t>((end ? end : bytes + count) - bytes));
        if (end)
            return true;
    }
    return false;
}

static bool ReadLibcppString(std::uintptr_t address, std::string& value)
{
    unsigned char bytes[24];
    if (!ReadMemory(address, bytes, sizeof(bytes)))
        return false;
    std::uintptr_t data = address;
    std::size_t size = bytes[23];
    if ((bytes[23] & 0x80) != 0)
    {
        std::memcpy(&data, bytes, sizeof(data));
        std::memcpy(&size, bytes + 8, sizeof(size));
    }
    else if (size > 23)
        return false;
    if (size > 4096 || (size && data < 0x10000))
        return false;
    value.resize(size);
    if (size && !ReadMemory(data, value.data(), size))
        return false;
    return std::none_of(
        value.begin(),
        value.end(),
        [](unsigned char byte)
        {
            return byte == 0 || byte < 9 || (byte > 13 && byte < 32);
        });
}

static bool ReadLibcppName(std::uintptr_t address, std::string& value)
{
    if (ReadLibcppString(address, value) && !value.empty())
        return true;
    return ReadLibcppString(address + 8, value) && !value.empty();
}

static bool ReadInstanceClass(std::uintptr_t instance, std::string& value)
{
    std::uintptr_t descriptor = 0;
    if (!ReadValue(instance + InstanceClassDescriptorOffset, descriptor) || !descriptor)
        return false;
    std::uintptr_t name = 0;
    if (ReadValue(descriptor + 8, name) && name &&
        ReadLibcppName(name, value) && !value.empty())
        return true;
    return ReadLibcppName(descriptor + 8, value) && !value.empty();
}

static std::vector<std::string> ExpectedDataModelNames(const std::string& title)
{
    std::string normalized = title;
    const std::string suffix = " - Roblox Studio";
    if (normalized.size() >= suffix.size() &&
        normalized.compare(normalized.size() - suffix.size(), suffix.size(), suffix) == 0)
        normalized.resize(normalized.size() - suffix.size());
    std::vector<std::string> names;
    if (!normalized.empty())
        names.push_back(normalized);
    const auto separator = normalized.find_last_of("/\\");
    if (separator != std::string::npos && separator + 1 < normalized.size())
    {
        auto base = normalized.substr(separator + 1);
        if (std::find(names.begin(), names.end(), base) == names.end())
            names.push_back(std::move(base));
    }
    return names;
}

static bool ReadInstanceNameAt(
    std::uintptr_t instance,
    const InstanceNameLayout& layout,
    std::string& value)
{
    std::uintptr_t indirect = 0;
    if (!ReadValue(instance + layout.offset, indirect) || indirect < 0x10000)
        return false;
    auto address = indirect + layout.nested;
    if (layout.doubleIndirect)
    {
        std::uintptr_t nested = 0;
        if (!ReadValue(address, nested) || nested < 0x10000)
            return false;
        address = nested;
    }
    return layout.cString ? ReadCString(address, value, 256) : ReadLibcppName(address, value);
}

// The Name field is discovered once against a known name and then read at
// exactly that layout. Scanning every candidate offset per instance can
// reach a sibling's name through unrelated pointers and match the wrong
// instance, so a mismatch at the cached layout is a real mismatch.
static bool ReadExpectedInstanceName(
    std::uintptr_t instance,
    const std::vector<std::string>& expectedNames,
    std::string& value)
{
    const auto matches = [&](const std::string& candidate)
    {
        return std::find(expectedNames.begin(), expectedNames.end(), candidate) !=
            expectedNames.end();
    };
    InstanceNameLayout cached;
    if (LoadInstanceNameLayout(cached) && ReadInstanceNameAt(instance, cached, value))
        return matches(value);
    for (std::size_t offset = InstanceNameOffsetMin;
         offset <= InstanceNameOffsetMax;
         offset += sizeof(void*))
    {
        for (std::size_t nestedOffset = 0; nestedOffset <= 64; nestedOffset += 8)
        {
            for (const bool doubleIndirect : {false, true})
            {
                for (const bool cString : {false, true})
                {
                    const InstanceNameLayout layout{offset, nestedOffset, doubleIndirect, cString};
                    if (ReadInstanceNameAt(instance, layout, value) && matches(value))
                    {
                        CachedInstanceNameLayout.store(
                            PackInstanceNameLayout(layout), std::memory_order_relaxed);
                        CachedInstanceNameOffset.store(offset, std::memory_order_relaxed);
                        return true;
                    }
                }
            }
        }
    }
    value.clear();
    return false;
}

static bool ReadVtableRttiType(std::uintptr_t vtable, std::string& name)
{
    std::uintptr_t typeInfo = 0;
    std::uintptr_t typeName = 0;
    return vtable >= sizeof(void*) &&
        ReadValue(vtable - sizeof(void*), typeInfo) && typeInfo &&
        ReadValue(typeInfo + sizeof(void*), typeName) && typeName &&
        ReadCString(typeName, name, 256);
}

static bool ReadRttiType(std::uintptr_t object, std::string& name)
{
    std::uintptr_t vtable = 0;
    return ReadValue(object, vtable) && ReadVtableRttiType(vtable, name);
}

static bool IsRttiType(std::uintptr_t object, const char* expected)
{
    std::string name;
    return ReadRttiType(object, name) && name == expected;
}

static bool IsDataModelInstance(std::uintptr_t instance)
{
    std::uintptr_t self = 0;
    return ReadValue(instance + 8, self) && self == instance &&
        IsRttiType(instance, "N3RBX9DataModelE");
}

static bool ReadChildren(
    std::uintptr_t instance,
    std::size_t childrenOffset,
    std::vector<SharedInstance>& children)
{
    std::uintptr_t vector = 0;
    std::uintptr_t begin = 0;
    std::uintptr_t end = 0;
    std::uintptr_t capacity = 0;
    if (!ReadValue(instance + childrenOffset, vector) || !vector)
        return false;
    if (!ReadValue(vector, begin) || !ReadValue(vector + 8, end) ||
        !ReadValue(vector + 16, capacity) || end < begin || capacity < end ||
        (end - begin) % sizeof(SharedInstance) != 0)
        return false;
    const auto count = (end - begin) / sizeof(SharedInstance);
    if (count == 0 || count > 512)
        return false;
    children.resize(count);
    return ReadMemory(begin, children.data(), children.size() * sizeof(SharedInstance));
}

static std::uint32_t RequiredRootMask(
    const std::vector<SharedInstance>& children,
    std::size_t& readableClasses)
{
    std::uint32_t mask = 0;
    readableClasses = 0;
    for (const auto& child : children)
    {
        std::string name;
        if (!child.instance ||
            (!ReadInstanceClass(reinterpret_cast<std::uintptr_t>(child.instance), name) &&
             !ReadRttiType(reinterpret_cast<std::uintptr_t>(child.instance), name)))
            continue;
        ++readableClasses;
        if (name == "Workspace" || name == "N3RBX9WorkspaceE")
            mask |= 1;
        else if (name == "Players" || name == "N3RBX7PlayersE")
            mask |= 2;
        else if (name == "MaterialService" || name == "N3RBX15MaterialServiceE")
            mask |= 4;
        else if (name == "StudioData" || name == "N3RBX10StudioDataE")
            mask |= 8;
        else if (name == "ChangeHistoryService" || name == "N3RBX20ChangeHistoryServiceE")
            mask |= 16;
        else if (name == "ScriptEditorService" || name == "N3RBX19ScriptEditorServiceE")
            mask |= 32;
        else if (name == "PluginGuiService" || name == "N3RBX16PluginGuiServiceE")
            mask |= 64;
        else if (name == "DraftsService" || name == "N3RBX13DraftsServiceE")
            mask |= 128;
        else if (name == "Selection" || name == "N3RBX9SelectionE")
            mask |= 256;
        else if (name == "StudioService" || name == "N3RBX13StudioServiceE")
            mask |= 512;
    }
    return mask;
}

static bool FindChildrenOffset(
    std::uintptr_t instance,
    std::size_t& offset,
    std::uint32_t& rootMask,
    std::size_t& rootCount,
    DataModelScanStats& stats)
{
    for (std::size_t current = InstanceChildrenOffsetMin;
         current <= InstanceChildrenOffsetMax;
         current += sizeof(void*))
    {
        std::vector<SharedInstance> children;
        if (!ReadChildren(instance, current, children))
            continue;
        ++stats.childVectors;
        std::size_t readableClasses = 0;
        const auto mask = RequiredRootMask(children, readableClasses);
        if (stats.vectorDetails.size() < 240)
        {
            char detail[80];
            std::snprintf(
                detail,
                sizeof(detail),
                "%s0x%zx:%zu/%zu:m%u",
                stats.vectorDetails.empty() ? "" : ",",
                current,
                readableClasses,
                children.size(),
                static_cast<unsigned>(mask));
            stats.vectorDetails += detail;
        }
        if ((mask & 15) != 15)
            continue;
        ++stats.requiredRoots;
        offset = current;
        rootMask = mask;
        rootCount = children.size();
        return true;
    }
    return false;
}

static bool FindDataModelInstanceOffset(
    std::uintptr_t outer,
    std::size_t& instanceOffset,
    std::size_t& childrenOffset,
    std::uint32_t& rootMask,
    std::size_t& rootCount,
    DataModelScanStats& stats)
{
    if (!IsRttiType(outer, "N3RBX9DataModelE"))
        return false;
    ++stats.outerRtti;
    for (std::size_t current = DataModelInstanceOffsetMin;
         current <= DataModelInstanceOffsetMax;
         current += sizeof(void*))
    {
        const auto instance = outer + current;
        std::uintptr_t self = 0;
        if (!ReadValue(instance + 8, self) || self != instance)
            continue;
        ++stats.selfInstances;
        if (!IsRttiType(instance, "N3RBX9DataModelE"))
            continue;
        ++stats.instanceRtti;
        if (!FindChildrenOffset(instance, childrenOffset, rootMask, rootCount, stats))
            continue;
        instanceOffset = current;
        return true;
    }
    return false;
}

// Global sections contain many pointers into the same heap pages. Copy each
// bounded span once instead of making a Mach RPC for every first word. Failed
// spans fall back to the original individual reads (including readable objects
// next to inaccessible memory). Matching objects are always revalidated live.
template <typename Visit>
static void ReadCandidateVtables(std::vector<std::uintptr_t>& pointers, Visit visit)
{
    std::sort(pointers.begin(), pointers.end());
    pointers.erase(std::unique(pointers.begin(), pointers.end()), pointers.end());
    for (std::size_t begin = 0; begin < pointers.size();)
    {
        const auto first = pointers[begin];
        std::size_t end = begin + 1;
        while (end < pointers.size() && (pointers[end] >> 12) == (first >> 12)) ++end;
        unsigned char bytes[4096];
        const auto size = pointers[end - 1] - first + sizeof(std::uintptr_t);
        const auto copied = size <= sizeof(bytes) && ReadMemory(first, bytes, size);
        for (auto index = begin; index < end; ++index)
        {
            std::uintptr_t vtable = 0;
            if (copied) std::memcpy(&vtable, bytes + (pointers[index] - first), sizeof(vtable));
            else if (!ReadValue(pointers[index], vtable)) continue;
            visit(pointers[index], vtable);
        }
        begin = end;
    }
}

static void AddCandidates(
    const mach_header_64* header,
    std::intptr_t slide,
    const std::vector<std::string>& expectedNames,
    std::vector<DataModelCandidate>& candidates,
    DataModelScanStats& stats,
    bool measureRtti = false)
{
    const auto collectStart = std::chrono::steady_clock::now();
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    std::vector<std::uintptr_t> objects;
    std::unordered_map<std::uintptr_t, bool> dataModelTypes;
    for (std::uint32_t index = 0; index < header->ncmds; ++index)
    {
        const auto load = reinterpret_cast<const load_command*>(command);
        if (load->cmd == LC_SEGMENT_64)
        {
            const auto segment = reinterpret_cast<const segment_command_64*>(load);
            if ((std::strcmp(segment->segname, "__DATA") == 0 ||
                 std::strcmp(segment->segname, "__DATA_CONST") == 0) &&
                segment->vmsize <= 256ULL * 1024ULL * 1024ULL)
            {
                const auto section = reinterpret_cast<const section_64*>(segment + 1);
                for (std::uint32_t sectionIndex = 0; sectionIndex < segment->nsects; ++sectionIndex)
                {
                    const auto& current = section[sectionIndex];
                    if (current.size < sizeof(void*) ||
                        current.size > 64ULL * 1024ULL * 1024ULL)
                        continue;
                    const auto count = static_cast<std::size_t>(current.size / sizeof(void*));
                    std::vector<std::uintptr_t> pointers(count);
                    if (!ReadMemory(
                            static_cast<std::uintptr_t>(current.addr + slide),
                            pointers.data(),
                            pointers.size() * sizeof(void*)))
                        continue;
                    for (const auto outer : pointers)
                    {
                        if (outer >= 0x10000) objects.push_back(outer);
                    }
                }
            }
        }
        if (load->cmdsize < sizeof(load_command))
            break;
        command += load->cmdsize;
    }
    stats.collectUs = std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now() - collectStart).count();
    ReadCandidateVtables(objects, [&](std::uintptr_t outer, std::uintptr_t vtable) {
        auto type = dataModelTypes.find(vtable);
        if (type == dataModelTypes.end())
        {
            std::string name;
            const auto rttiStart = measureRtti ? std::chrono::steady_clock::now()
                : std::chrono::steady_clock::time_point{};
            const auto readable = ReadVtableRttiType(vtable, name);
            if (measureRtti) stats.rttiNanos += std::chrono::duration_cast<std::chrono::nanoseconds>(
                std::chrono::steady_clock::now() - rttiStart).count();
            ++stats.rttiReads;
            if (!readable) return;
            // Scan-local only; unreadable metadata is never cached.
            type = dataModelTypes.emplace(vtable, name == "N3RBX9DataModelE").first;
        }
        if (!type->second) return;
        std::size_t instanceOffset = 0, childrenOffset = 0, rootCount = 0;
        std::uint32_t rootMask = 0;
        if (!FindDataModelInstanceOffset(outer, instanceOffset, childrenOffset, rootMask, rootCount, stats))
            return;
        std::string name;
        ReadExpectedInstanceName(outer + instanceOffset, expectedNames, name);
        candidates.push_back({reinterpret_cast<void*>(outer), instanceOffset, childrenOffset,
            rootMask, rootCount, std::move(name)});
    });
    stats.pointers = objects.size();
}

static bool FindDataModel(
    const mach_header_64* header,
    std::intptr_t slide,
    const std::string& title,
    void*& output,
    DataModelLayout& layout,
    std::string& error,
    PropertyTiming* timing = nullptr,
    std::uintptr_t expectedModel = 0)
{
    const auto lockStart = std::chrono::steady_clock::now();
    std::lock_guard lock(DataModelCacheMutex);
    if (timing) timing->lockUs = std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now() - lockStart).count();
    if (CachedDataModel)
    {
        const auto cached = reinterpret_cast<std::uintptr_t>(CachedDataModel);
        const auto instance = cached + CachedDataModelInstanceOffset;
        std::vector<SharedInstance> children;
        std::size_t readableClasses = 0;
        if ((!expectedModel || cached == expectedModel) && CachedDataModelTitle == title && CachedDataModelInstanceOffset &&
            CachedDataModelChildrenOffset &&
            IsDataModelInstance(instance) &&
            ReadChildren(instance, CachedDataModelChildrenOffset, children) &&
            (RequiredRootMask(children, readableClasses) & 15) == 15)
        {
            if (timing) timing->cache = 1;
            output = CachedDataModel;
            layout = {CachedDataModelInstanceOffset, CachedDataModelChildrenOffset};
            return true;
        }
        if (timing) timing->cache = CachedDataModelTitle == title ? 3 : 2;
        CachedDataModel = nullptr;
        CachedDataModelInstanceOffset = 0;
        CachedDataModelChildrenOffset = 0;
        CachedDataModelTitle.clear();
    }
    if (!header || header->magic != MH_MAGIC_64)
    {
        error = "Studio main image is not a 64-bit Mach-O";
        return false;
    }
    std::vector<DataModelCandidate> candidates;
    DataModelScanStats stats;
    const auto scanStart = std::chrono::steady_clock::now();
    AddCandidates(header, slide, ExpectedDataModelNames(title), candidates, stats, timing && timing->detailed);
    if (expectedModel) {
        candidates.erase(std::remove_if(candidates.begin(), candidates.end(), [&](const auto& candidate) {
            return reinterpret_cast<std::uintptr_t>(candidate.outer) != expectedModel;
        }), candidates.end());
    }
    if (timing)
    {
        timing->scanUs = std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now() - scanStart).count();
        timing->collectUs = stats.collectUs;
        timing->rttiUs = stats.rttiNanos / 1000;
        timing->rttiReads = stats.rttiReads;
        timing->pointers = stats.pointers;
    }
    if (candidates.size() > 1 && std::any_of(
            candidates.begin(),
            candidates.end(),
            [](const DataModelCandidate& candidate)
            {
                return !candidate.name.empty();
            }))
    {
        candidates.erase(
            std::remove_if(
                candidates.begin(),
                candidates.end(),
                [](const DataModelCandidate& candidate)
                {
                    return candidate.name.empty();
                }),
            candidates.end());
    }
    if (candidates.size() > 1)
    {
        const auto editorScore = [](const DataModelCandidate& candidate)
        {
            auto bits = candidate.rootMask >> 4;
            std::uint32_t score = 0;
            while (bits)
            {
                score += bits & 1;
                bits >>= 1;
            }
            return score;
        };
        const auto best = std::max_element(
            candidates.begin(),
            candidates.end(),
            [&](const DataModelCandidate& left, const DataModelCandidate& right)
            {
                return editorScore(left) < editorScore(right);
            });
        const auto bestScore = editorScore(*best);
        if (std::count_if(
                candidates.begin(),
                candidates.end(),
                [&](const DataModelCandidate& candidate)
                {
                    return editorScore(candidate) == bestScore;
                }) == 1)
        {
            auto selected = std::move(*best);
            candidates.clear();
            candidates.push_back(std::move(selected));
        }
    }
    if (candidates.size() != 1)
    {
        std::string names;
        for (const auto& candidate : candidates)
        {
            if (!names.empty())
                names += ",";
            names += (candidate.name.empty() ? "<empty>" : candidate.name) +
                ("/m" + std::to_string(candidate.rootMask) + "/r" +
                 std::to_string(candidate.rootCount));
        }
        error = "active Studio DataModel selection returned " +
            std::to_string(candidates.size()) + " candidates (pointers=" +
            std::to_string(stats.pointers) + ", outerRtti=" +
            std::to_string(stats.outerRtti) + ", self=" +
            std::to_string(stats.selfInstances) + ", instanceRtti=" +
            std::to_string(stats.instanceRtti) + ", childVectors=" +
            std::to_string(stats.childVectors) + ", roots=" +
            std::to_string(stats.requiredRoots) + ", vectors=" +
            stats.vectorDetails + ", names=" + names + ")";
        return false;
    }
    output = candidates[0].outer;
    CachedDataModel = output;
    CachedDataModelInstanceOffset = candidates[0].instanceOffset;
    CachedDataModelChildrenOffset = candidates[0].childrenOffset;
    CachedDataModelTitle = title;
    layout = {CachedDataModelInstanceOffset, CachedDataModelChildrenOffset};
    return true;
}

static bool LikelyPointer(std::uintptr_t value)
{
    return value >= 0x10000 && value % alignof(void*) == 0;
}

static bool LikelyCodePointer(std::uintptr_t value)
{
    return value >= 0x10000 && value % 4 == 0;
}

static bool RetainOwner(void* owner)
{
    if (!owner)
        return false;
    auto references = reinterpret_cast<std::atomic<std::int64_t>*>(
        reinterpret_cast<unsigned char*>(owner) + 8);
    auto current = references->load(std::memory_order_acquire);
    while (current >= 0)
    {
        if (references->compare_exchange_weak(
                current,
                current + 1,
                std::memory_order_acq_rel,
                std::memory_order_acquire))
            return true;
    }
    return false;
}

static void ReleaseWeakOwner(void* owner)
{
    auto weak = reinterpret_cast<std::atomic<std::int64_t>*>(reinterpret_cast<unsigned char*>(owner) + 16);
    if (weak->fetch_sub(1, std::memory_order_acq_rel) == 0)
        reinterpret_cast<void (*)(void*)>((*reinterpret_cast<void***>(owner))[3])(owner);
}

static void ReleaseOwner(void* owner)
{
    auto bytes = reinterpret_cast<unsigned char*>(owner);
    auto strong = reinterpret_cast<std::atomic<std::int64_t>*>(bytes + 8);
    if (strong->fetch_sub(1, std::memory_order_acq_rel) == 0)
    {
        const auto vtable = *reinterpret_cast<void***>(owner);
        reinterpret_cast<void (*)(void*)>(vtable[2])(owner);
        ReleaseWeakOwner(owner);
    }
}

static bool ResolveImageFunction(
    const mach_header_64* header,
    std::intptr_t slide,
    const Request& request,
    std::uint64_t rva,
    const char* name,
    std::uintptr_t& function,
    std::string& error)
{
    const segment_command_64* text = nullptr;
    const uuid_command* uuid = nullptr;
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    for (std::uint32_t index = 0; index < header->ncmds; ++index)
    {
        const auto load = reinterpret_cast<const load_command*>(command);
        if (load->cmdsize < sizeof(load_command))
        {
            error = "Studio's loaded image commands are invalid";
            return false;
        }
        if (load->cmd == LC_SEGMENT_64)
        {
            const auto segment = reinterpret_cast<const segment_command_64*>(load);
            if (std::strcmp(segment->segname, "__TEXT") == 0)
                text = segment;
        }
        else if (load->cmd == LC_UUID)
            uuid = reinterpret_cast<const uuid_command*>(load);
        command += load->cmdsize;
    }
    if (!text || !uuid || !rva ||
        std::memcmp(uuid->uuid, request.imageUuid, sizeof(request.imageUuid)) != 0 ||
        rva >= text->vmsize)
    {
        error = std::string("Studio's ") + name + " does not match its loaded image";
        return false;
    }
    function = static_cast<std::uintptr_t>(text->vmaddr + slide) + rva;
    Dl_info info{};
    if (!LikelyCodePointer(function) ||
        !dladdr(reinterpret_cast<void*>(function), &info) || info.dli_fbase != header)
    {
        error = std::string("Studio's ") + name + " is invalid";
        return false;
    }
    return true;
}

static bool ResolveDataModelTaskContext(
    void*, const DataModelLayout&, const mach_header_64*, std::uintptr_t,
    void*&, std::string& error)
{
    // No engine call is permitted until a complete, image-verified ARM64
    // context getter and its ownership contract can be demonstrated.
    error = "macOS DataModel queue context has no verified structural locator; use the plugin path";
    return false;
}

static const mach_header_64* MainStudioImage(std::intptr_t& slide)
{
    for (std::uint32_t index = 0; index < _dyld_image_count(); ++index)
    {
        const auto header = reinterpret_cast<const mach_header_64*>(_dyld_get_image_header(index));
        if (header && header->filetype == MH_EXECUTE)
        {
            slide = _dyld_get_image_vmaddr_slide(index);
            return header;
        }
    }
    return nullptr;
}

static Response PackageAction(const Request&, const std::string&, const std::string&)
{
    Response response{Magic, 10, 0, 0, {}};
    SetError(response, "Native macOS package actions have no verified structural bindings; use the plugin package path");
    return response;
}

static void ReleaseQString(StudioQString& value)
{
    if (!value.data)
        return;
    auto references = reinterpret_cast<std::atomic<std::int32_t>*>(value.data);
    const auto current = references->load(std::memory_order_relaxed);
    if (current >= 0 &&
        (current == 0 || references->fetch_sub(1, std::memory_order_acq_rel) == 1))
    {
        using Deallocate = void (*)(void*, std::size_t, std::size_t);
        const auto deallocate = reinterpret_cast<Deallocate>(
            dlsym(RTLD_DEFAULT, "_ZN10QArrayData10deallocateEPS_mm"));
        if (deallocate)
            deallocate(value.data, 2, 8);
    }
    value.data = nullptr;
}

static void ReleaseShared(StudioShared& value)
{
    if (!value.owner)
        return;
    auto bytes = reinterpret_cast<unsigned char*>(value.owner);
    auto strong = reinterpret_cast<std::atomic<std::int64_t>*>(bytes + 8);
    if (strong->fetch_sub(1, std::memory_order_acq_rel) == 0)
    {
        const auto vtable = *reinterpret_cast<void***>(value.owner);
        reinterpret_cast<void (*)(void*)>(vtable[2])(value.owner);
        auto weak = reinterpret_cast<std::atomic<std::int64_t>*>(bytes + 16);
        if (weak->fetch_sub(1, std::memory_order_acq_rel) == 0)
            reinterpret_cast<void (*)(void*)>(vtable[3])(value.owner);
    }
    value.value = nullptr;
    value.owner = nullptr;
}

static std::string UuidText(const unsigned char* uuid)
{
    char output[33]{};
    for (std::size_t index = 0; index < 16; ++index)
        std::snprintf(output + index * 2, 3, "%02x", uuid[index]);
    return output;
}

static bool ResolveTrace(
    const mach_header_64* header,
    std::intptr_t slide,
    const Request& request,
    std::uintptr_t& factoryAddress,
    std::uintptr_t& executeAddress,
    std::string& error)
{
    const segment_command_64* text = nullptr;
    const uuid_command* uuid = nullptr;
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    for (std::uint32_t index = 0; index < header->ncmds; ++index)
    {
        const auto load = reinterpret_cast<const load_command*>(command);
        if (load->cmd == LC_SEGMENT_64)
        {
            const auto segment = reinterpret_cast<const segment_command_64*>(load);
            if (std::strcmp(segment->segname, "__TEXT") == 0)
                text = segment;
        }
        else if (load->cmd == LC_UUID)
            uuid = reinterpret_cast<const uuid_command*>(load);
        if (load->cmdsize < sizeof(load_command))
            return false;
        command += load->cmdsize;
    }
    if (!text || !uuid)
    {
        error = "Studio's loaded image has no text identity";
        return false;
    }
    if (std::memcmp(uuid->uuid, request.imageUuid, sizeof(request.imageUuid)) != 0)
    {
        error = "Studio image " + UuidText(uuid->uuid) + " does not match traced image " +
            UuidText(request.imageUuid);
        return false;
    }
    if (request.factoryRva >= text->vmsize || request.executeRva >= text->vmsize)
    {
        error = "serializer trace points outside Studio's text segment";
        return false;
    }
    const auto runtimeBase = static_cast<std::uintptr_t>(text->vmaddr + slide);
    factoryAddress = runtimeBase + request.factoryRva;
    executeAddress = runtimeBase + request.executeRva;
    return true;
}

static void SetError(Response& response, const std::string& error)
{
    std::snprintf(response.error, sizeof(response.error), "%s", error.c_str());
}

static Response Serialize(
    const Request& request,
    const std::string& path,
    const std::string& title)
{
    Response response{Magic, 1, 0, 0, {}};
    std::lock_guard lock(SerializeMutex);
    const auto started = std::chrono::steady_clock::now();
    std::intptr_t slide = 0;
    const auto header = MainStudioImage(slide);
    std::uintptr_t factoryAddress = 0;
    std::uintptr_t executeAddress = 0;
    std::string error;
    if (!header ||
        !ResolveTrace(header, slide, request, factoryAddress, executeAddress, error))
    {
        response.status = 2;
        SetError(response, error.empty() ? "Studio main image is unavailable" : error);
        return response;
    }
    void* dataModel = nullptr;
    DataModelLayout layout;
    if (!FindDataModel(header, slide, title, dataModel, layout, error))
    {
        response.status = 3;
        SetError(response, error);
        return response;
    }
    using FromUtf8 = StudioQString (*)(const char*, int);
    using Factory = StudioShared (*)(void*, const StudioQString*, void**, const bool*);
    using Execute = void (*)(void*);
    const auto fromUtf8 = reinterpret_cast<FromUtf8>(
        dlsym(RTLD_DEFAULT, "_ZN7QString15fromUtf8_helperEPKci"));
    if (!fromUtf8)
    {
        response.status = 4;
        SetError(response, "Qt QString conversion is unavailable");
        return response;
    }
    auto output = fromUtf8(path.c_str(), static_cast<int>(path.size()));
    const bool direct = true;
    auto state = reinterpret_cast<Factory>(factoryAddress)(
        nullptr,
        &output,
        &dataModel,
        &direct);
    if (!state.value || !state.owner)
    {
        ReleaseQString(output);
        response.status = 5;
        SetError(response, "Studio serializer state creation failed");
        return response;
    }
    reinterpret_cast<Execute>(executeAddress)(state.value);
    ReleaseShared(state);
    ReleaseQString(output);
    struct stat metadata{};
    if (stat(path.c_str(), &metadata) != 0 || metadata.st_size <= 0)
    {
        response.status = 6;
        SetError(response, "Studio serializer did not create the requested file");
        return response;
    }
    response.status = 0;
    response.outputSize = static_cast<std::uint64_t>(metadata.st_size);
    response.elapsedMicros = static_cast<std::uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now() - started)
            .count());
    return response;
}

static bool ReadExact(int socket, void* output, std::size_t size)
{
    auto bytes = static_cast<unsigned char*>(output);
    while (size)
    {
        const auto count = read(socket, bytes, size);
        if (count <= 0)
            return false;
        bytes += count;
        size -= static_cast<std::size_t>(count);
    }
    return true;
}

static bool WriteExact(int socket, const void* input, std::size_t size)
{
    auto bytes = static_cast<const unsigned char*>(input);
    while (size)
    {
        const auto count = write(socket, bytes, size);
        if (count <= 0)
            return false;
        bytes += count;
        size -= static_cast<std::size_t>(count);
    }
    return true;
}

// Process-local transport for the Rust reflection resolver. No Luau method or
// network endpoint exposes this interface; the socket is private to the OS user.
// Discovery, executable verification and authorization remain in Rust.
struct PropertyCallParams
{
    std::uintptr_t model, instance, owner, descriptor, descriptorVtable, classDescriptor;
    std::uintptr_t getter, setter, identityBinding, identityGetter;
    std::uint64_t classOffset, selfOffset, parentOffset, getterSlot, setterSlot, identitySlot;
    std::uint32_t ancestorCount, operation, inputSize, timeoutMs;
    std::uintptr_t ancestors[65];
    unsigned char expectedIdentity[16];
    char input[65536];
};
static_assert(sizeof(PropertyCallParams) == 66216);
struct PropertyIdentity { std::uint64_t low, high; };
static std::atomic<unsigned> PendingPropertyTasks{0};
struct PropertyPermit
{
    bool acquired = false;
    PropertyPermit()
    {
        auto count = PendingPropertyTasks.load(std::memory_order_relaxed);
        while (count < 8)
            if (PendingPropertyTasks.compare_exchange_weak(count, count + 1, std::memory_order_relaxed))
            {
                acquired = true;
                break;
            }
    }
    PropertyPermit(const PropertyPermit&) = delete;
    PropertyPermit& operator=(const PropertyPermit&) = delete;
    ~PropertyPermit()
    {
        if (acquired) PendingPropertyTasks.fetch_sub(1, std::memory_order_relaxed);
    }
};
struct PropertyModelContext;
#include "renium_studio_functions.h"

struct PropertyCallTask
{
    // Released last, after both the waiter and every queued callback dispose
    // their task references. A timed-out client must not reopen queue capacity.
    std::shared_ptr<PropertyPermit> permit;
    std::shared_ptr<PropertyTiming> timing;
    PropertyCallParams params{};
    std::string extraInput;
    std::shared_ptr<renium_functions::Completion> function;
    bool prepareOnly = false;
    std::shared_ptr<PropertyModelContext> modelContext;
    std::chrono::steady_clock::time_point deadline;
    std::mutex mutex;
    std::condition_variable completed;
    bool done = false;
    std::string error;
    std::vector<unsigned char> output;
};

// A weak reference keeps only the C++ control block alive, not a closing place.
// Acquire it once on the UI thread. Warm calls can safely lock it from the RPC
// thread instead of waiting an extra UI frame before every DataModel task.
struct PropertyModelContext
{
    std::uintptr_t model, instance;
    void* owner = nullptr;
    void* context;
    std::string title;
    ~PropertyModelContext() { if (owner) ReleaseWeakOwner(owner); }
};
static std::mutex PropertyModelMutex;
static std::shared_ptr<PropertyModelContext> CachedPropertyModel;

struct ModelOwnerRelease
{
    std::shared_ptr<PropertyPermit> permit;
    void* owner;
};

static std::shared_ptr<void> RetainModelOwner(void* owner, const std::shared_ptr<PropertyPermit>& permit)
{
    // Allocate the release receipt before retaining. The shared_ptr constructor
    // invokes its deleter on allocation failure, so no acquired owner is lost.
    auto release = std::make_unique<ModelOwnerRelease>(ModelOwnerRelease{permit, owner});
    if (!RetainOwner(owner)) return {};
    return std::shared_ptr<void>(release.release(), [](void* retained) {
        auto receipt = std::unique_ptr<ModelOwnerRelease>(static_cast<ModelOwnerRelease*>(retained));
        auto strong = reinterpret_cast<std::atomic<std::int64_t>*>(
            reinterpret_cast<unsigned char*>(receipt->owner) + 8);
        auto count = strong->load(std::memory_order_acquire);
        // libc++ stores strong references minus one. A successful decrement
        // above zero cannot destroy the model; do not queue inert cleanup and
        // exhaust all eight permits during a completed-call burst. CAS is
        // required: another owner may release between the load and decrement.
        while (count > 0)
            if (strong->compare_exchange_weak(count, count - 1,
                    std::memory_order_acq_rel, std::memory_order_acquire))
                return; // Do not access the owner after dropping our reference.
        // Keep the last strong reference AND permit until UI-thread disposal.
        dispatch_async_f(dispatch_get_main_queue(), receipt.release(), [](void* raw) {
            const auto release = std::unique_ptr<ModelOwnerRelease>(static_cast<ModelOwnerRelease*>(raw));
            ReleaseOwner(release->owner);
        });
    });
}

static void FinishPropertyError(const std::shared_ptr<PropertyCallTask>& task, const char* error)
{
    { std::lock_guard lock(task->mutex); task->error = error; task->done = true; }
    task->completed.notify_all();
}

static void CapturePropertyIdentities(const std::shared_ptr<PropertyCallTask>& task,
    std::vector<unsigned char>& output)
{
    const auto& p = task->params;
    std::uint64_t fields[8]{};
    if (p.inputSize < sizeof(fields))
        throw std::runtime_error("Native identity request is truncated");
    std::memcpy(fields, p.input, sizeof(fields));
    const auto descriptor = fields[0], table = fields[1], slot = fields[2], invoker = fields[3];
    const auto memberOffset = fields[4], function = fields[5], childrenOffset = fields[6], count = fields[7];
    const auto equal = [](std::uintptr_t at, std::uintptr_t expected) {
        std::uintptr_t value = 0;
        return ReadValue(at, value) && value == expected;
    };
    if (count < 1 || count > 64 || p.inputSize != sizeof(fields) + count * sizeof(SharedInstance) ||
        slot >= 32 || slot % 8 || memberOffset < 64 || memberOffset >= 256 ||
        childrenOffset > 2048 || childrenOffset % 8 ||
        !equal(descriptor, table) || !equal(table + slot, invoker) ||
        !equal(descriptor + memberOffset, function) || !equal(descriptor + memberOffset + 8, 0))
        throw std::runtime_error("Native identity binding changed");
    struct Entry { SharedInstance value; std::uintptr_t parent; };
    std::vector<Entry> pending;
    for (std::size_t i = 0; i < count; ++i)
    {
        SharedInstance root{};
        std::memcpy(&root, p.input + sizeof(fields) + i * sizeof(root), sizeof(root));
        SharedInstance self{};
        const auto instance = reinterpret_cast<std::uintptr_t>(root.instance);
        if (!instance || !root.owner || !ReadMemory(instance + p.selfOffset, &self, sizeof(self)) ||
            self.instance != root.instance || self.owner != root.owner ||
            !equal(instance + p.parentOffset, task->modelContext->instance))
            throw std::runtime_error("Native identity service ownership changed");
        pending.push_back({root, task->modelContext->instance});
    }
    std::unordered_set<void*> seen;
    const auto initialSize = output.size();
    while (!pending.empty())
    {
        if (seen.size() % 256 == 0 && std::chrono::steady_clock::now() >= task->deadline)
            throw std::runtime_error("Native identity capture exceeded its deadline");
        const auto entry = pending.back();
        pending.pop_back();
        const auto instance = reinterpret_cast<std::uintptr_t>(entry.value.instance);
        SharedInstance self{};
        if (!instance || !entry.value.owner || !seen.insert(entry.value.instance).second || seen.size() > 1000000)
            throw std::runtime_error("Native identity graph ownership changed");
        // Discovery and service ownership above need fault-tolerant Mach reads.
        // Descendants now come exclusively from that retained, locked engine
        // graph. Reading each word through the kernel adds hundreds of thousands
        // of syscalls; use the same direct traversal as native Windows capture.
        std::memcpy(&self, reinterpret_cast<const void*>(instance + p.selfOffset), sizeof(self));
        const auto parent = *reinterpret_cast<const std::uintptr_t*>(instance + p.parentOffset);
        if (self.instance != entry.value.instance || self.owner != entry.value.owner || parent != entry.parent)
            throw std::runtime_error("Native identity graph ownership changed");
        // This task holds the DataModel on its write queue; these non-yielding
        // inherited getters cannot release the live hierarchy or run Lua.
        const auto identity = reinterpret_cast<PropertyIdentity (*)(void*, void*)>(p.identityGetter)(
            reinterpret_cast<void*>(p.identityBinding), entry.value.instance);
        const auto debug = reinterpret_cast<std::string (*)(void*, int)>(function)(entry.value.instance, 32);
        if (debug.empty() || debug.size() >= 48 || debug.find('\0') != std::string::npos)
            throw std::runtime_error("Native debug identity is invalid");
        const auto offset = output.size();
        output.resize(offset + 64, 0);
        std::memcpy(output.data() + offset, &identity, 16);
        std::memcpy(output.data() + offset + 16, debug.data(), debug.size());
        const auto children = *reinterpret_cast<const std::uintptr_t*>(instance + childrenOffset);
        if (!children) continue;
        std::uintptr_t bounds[3]{};
        std::memcpy(bounds, reinterpret_cast<const void*>(children), sizeof(bounds));
        if ((!bounds[0] && (bounds[1] || bounds[2])) || bounds[1] < bounds[0] ||
            bounds[2] < bounds[1] || (bounds[1] - bounds[0]) % sizeof(SharedInstance))
            throw std::runtime_error("Native identity children vector changed");
        const auto childCount = (bounds[1] - bounds[0]) / sizeof(SharedInstance);
        if (childCount > 1000000 - seen.size() || pending.size() + childCount > 1000000)
            throw std::runtime_error("Native identity graph exceeds its limit");
        if (!childCount) continue;
        const auto values = reinterpret_cast<const SharedInstance*>(bounds[0]);
        for (std::size_t i = 0; i < childCount; ++i) pending.push_back({values[i], instance});
    }
    if (output.size() == initialSize)
        throw std::runtime_error("Native identity capture is empty");
}

static void ExecutePropertyCall(const std::shared_ptr<PropertyCallTask>& task)
{
    if (task->timing) task->timing->runBegin = task->timing->now();
    std::vector<unsigned char> output;
    std::string error;
    try
    {
        const auto& p = task->params;
        if (std::chrono::steady_clock::now() >= task->deadline)
            throw std::runtime_error("Protected property call expired before execution");
        const auto model = task->modelContext;
        const auto modelOwner = model ? RetainModelOwner(model->owner, task->permit) : std::shared_ptr<void>{};
        if (!modelOwner)
            throw std::runtime_error("Protected property DataModel expired");
        // Connection preparation validates the queue and its live owner, but
        // does not resolve, read, or write any reflected property.
        if (!task->prepareOnly)
        {
        const auto equal = [](std::uintptr_t at, std::uintptr_t expected) {
            std::uintptr_t actual = 0;
            return ReadValue(at, actual) && actual == expected;
        };
        if (!equal(p.instance + p.classOffset, p.classDescriptor) ||
            !equal(p.instance + p.selfOffset, p.instance) ||
            !equal(p.instance + p.selfOffset + 8, p.owner) ||
            !equal(p.descriptor, p.descriptorVtable) ||
            !equal(p.descriptorVtable + p.getterSlot, p.getter) ||
            (p.setter && !equal(p.descriptorVtable + p.setterSlot, p.setter)))
            throw std::runtime_error("Protected property target or descriptor was replaced");
        for (std::size_t i = 0; i + 1 < p.ancestorCount; ++i)
            if (!equal(p.ancestors[i] + p.parentOffset, p.ancestors[i + 1]))
                throw std::runtime_error("Protected property target was removed or reparented");
        std::uintptr_t identityTable = 0;
        if (!ReadValue(p.identityBinding, identityTable) ||
            !equal(identityTable + p.identitySlot, p.identityGetter) ||
            !RetainOwner(reinterpret_cast<void*>(p.owner)))
            throw std::runtime_error("Protected property identity expired");
        // Only retain the instance after validation on its DataModel queue.
        const auto owner = std::shared_ptr<void>(reinterpret_cast<void*>(p.owner), ReleaseOwner);
        const auto identity = reinterpret_cast<PropertyIdentity (*)(void*, void*)>(p.identityGetter)(
            reinterpret_cast<void*>(p.identityBinding), reinterpret_cast<void*>(p.instance));
        output.resize(sizeof(identity));
        std::memcpy(output.data(), &identity, sizeof(identity));
        if (p.operation != 0)
        {
            // Operations 3 and 4 atomically identify and read, never write.
            if (p.operation != 3 && p.operation != 4 &&
                std::memcmp(&identity, p.expectedIdentity, sizeof(identity)) != 0)
                throw std::runtime_error("Protected property instance identity changed");
            if (p.operation == 9) {
                task->function = renium_functions::Begin(reinterpret_cast<void*>(p.instance), owner,
                    p.input, p.inputSize, ReadMemory);
            }
            else if (p.operation == 6)
            {
                renium_history::Binding binding{};
                if (p.inputSize) {
                    if (p.inputSize <= sizeof(binding)) throw std::runtime_error("Missing history registration token");
                    std::memcpy(&binding, p.input, sizeof(binding));
                    renium_history::Register(binding, reinterpret_cast<void*>(p.instance), p.ancestors[p.ancestorCount - 1],
                        task->modelContext->owner,
                        std::string(p.input + sizeof(binding), p.inputSize - sizeof(binding)), ReadMemory);
                }
                binding = renium_history::GetBinding();
                output.resize(sizeof(identity) + sizeof(binding));
                std::memcpy(output.data() + sizeof(identity), &binding, sizeof(binding));
            }
            else if (p.operation == 8)
            {
                if (p.inputSize) {
                    if (p.inputSize != sizeof(renium_terrain_observation::Request)) throw std::runtime_error("Invalid Terrain notification request");
                    renium_terrain_observation::Request request{};
                    std::memcpy(&request, p.input, sizeof(request));
                    renium_terrain_observation::Install(reinterpret_cast<void*>(p.instance), reinterpret_cast<void*>(p.owner), request,
                        p.classOffset, p.selfOffset, p.parentOffset, p.ancestors[p.ancestorCount - 1], ReadMemory, RetainOwner, ReleaseOwner);
                }
                const auto binding = renium_terrain_observation::GetBinding(reinterpret_cast<void*>(p.instance));
                const auto data = reinterpret_cast<const unsigned char*>(&binding);
                output.insert(output.end(), data, data + sizeof(binding));
            }
            else if (p.operation == 7)
            {
                const auto changed = renium_terrain::Apply(reinterpret_cast<void*>(p.instance), owner,
                    p.ancestors[p.ancestorCount - 1], task->extraInput, ReadMemory);
                output.push_back(changed.changed ? 1 : 0);
                output.insert(output.end(), changed.fingerprint.begin(), changed.fingerprint.end());
            }
            else if (p.operation == 4)
            {
                CapturePropertyIdentities(task, output);
            }
            else
            {
                if (p.operation == 5)
                {
                    // BinaryString bindings return void when setting. Compare on
                    // the same DataModel task before writing any serialized bytes.
                    std::uint32_t expectedSize = 0;
                    if (!p.setter || p.inputSize < sizeof(expectedSize))
                        throw std::runtime_error("Invalid conditional binary property write");
                    std::memcpy(&expectedSize, p.input, sizeof(expectedSize));
                    if (expectedSize > p.inputSize - sizeof(expectedSize))
                        throw std::runtime_error("Invalid expected binary property length");
                    const auto current = reinterpret_cast<std::string (*)(void*, void*)>(p.getter)(
                        reinterpret_cast<void*>(p.descriptor), reinterpret_cast<void*>(p.instance));
                    const std::string expected(p.input + sizeof(expectedSize), expectedSize);
                    if (current != expected)
                        throw std::runtime_error("Binary property changed before the conditional write");
                    const std::string input(p.input + sizeof(expectedSize) + expectedSize,
                        p.inputSize - sizeof(expectedSize) - expectedSize);
                    if (input != current)
                        reinterpret_cast<void (*)(void*, void*, const std::string*)>(p.setter)(
                            reinterpret_cast<void*>(p.descriptor), reinterpret_cast<void*>(p.instance), &input);
                }
                else if (p.operation == 2)
                {
                    if (!p.setter) throw std::runtime_error("This property has no supported setter");
                    const std::string input(p.input, p.inputSize);
                    if (!reinterpret_cast<bool (*)(void*, void*, const std::string*)>(p.setter)(
                            reinterpret_cast<void*>(p.descriptor), reinterpret_cast<void*>(p.instance), &input))
                        throw std::runtime_error("Studio rejected the property value");
                }
                const auto value = reinterpret_cast<std::string (*)(void*, void*)>(p.getter)(
                    reinterpret_cast<void*>(p.descriptor), reinterpret_cast<void*>(p.instance));
                if (value.size() > 65536)
                    throw std::runtime_error("Protected property value exceeds 64 KiB");
                output.insert(output.end(), value.begin(), value.end());
            }
        }
        }
    }
    catch (const std::exception& exception) { error = exception.what(); }
    catch (...) { error = "Studio raised an unknown protected-property exception"; }
    {
        std::lock_guard lock(task->mutex);
        task->error = std::move(error);
        task->output = std::move(output);
        task->done = true;
        if (task->timing) task->timing->runEnd = task->timing->now();
    }
    task->completed.notify_all();
}

static void SubmitPropertyCall(void* context, std::uintptr_t submit, const std::shared_ptr<PropertyCallTask>& task)
{
    if (std::chrono::steady_clock::now() >= task->deadline)
        throw std::runtime_error("Protected property expired before submission");
    std::function<void()> callback{[task]() { ExecutePropertyCall(task); }};
    if (task->timing) task->timing->submit = task->timing->now();
    if (!reinterpret_cast<bool (*)(void*, std::function<void()>*, std::uint32_t)>(submit)(context, &callback, 1))
        throw std::runtime_error("Studio rejected the protected-property task");
}

static bool RunPropertyCall(const std::string& payload, const std::string& title,
    const mach_header_64* header, std::intptr_t slide, std::uintptr_t submit,
    std::vector<unsigned char>& output, std::string& error, bool prepareOnly,
    const std::shared_ptr<PropertyPermit>& permit,
    const std::shared_ptr<PropertyTiming>& timing)
{
    if (prepareOnly ? payload.size() != 20 : payload.size() < sizeof(PropertyCallParams) || payload.size() > sizeof(PropertyCallParams) + 128 * 1024 * 1024)
        throw std::runtime_error("Invalid protected-property request size");
    auto task = std::make_shared<PropertyCallTask>();
    task->permit = permit;
    task->timing = timing;
    task->prepareOnly = prepareOnly;
    if (prepareOnly)
    {
        // A fixed context-only request has no descriptor or getter/setter.
        auto& p = task->params;
        std::memcpy(&p.model, payload.data(), 8);
        std::memcpy(&p.instance, payload.data() + 8, 8);
        std::memcpy(&p.timeoutMs, payload.data() + 16, 4);
        p.ancestors[0] = p.ancestors[1] = p.instance;
        p.ancestorCount = 2;
        if (!LikelyPointer(p.model) || !LikelyPointer(p.instance))
            throw std::runtime_error("Invalid connection preparation target");
    }
    else {
        std::memcpy(&task->params, payload.data(), sizeof(PropertyCallParams));
        task->extraInput.assign(payload.data() + sizeof(PropertyCallParams), payload.size() - sizeof(PropertyCallParams));
    }
    const auto& p = task->params;
    if (p.operation > 9 || p.inputSize > sizeof(p.input) || p.ancestorCount < 2 || p.ancestorCount > 65 ||
        (p.operation == 7 ? task->extraInput.empty() : !task->extraInput.empty()) ||
        p.timeoutMs < 1 || p.timeoutMs > (p.operation == 9 ? 30000 : 3000) || p.ancestors[0] != p.instance)
        throw std::runtime_error("Invalid protected-property request");
    task->deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(p.timeoutMs);
    struct Submission
    {
        std::shared_ptr<PropertyCallTask> task;
        std::shared_ptr<void> modelOwner;
        const mach_header_64* header;
        std::intptr_t slide;
        std::uintptr_t submit;
        std::string title;
    };
    auto submission = std::make_shared<Submission>(Submission{task, {}, header, slide, submit, title});
    std::shared_ptr<PropertyModelContext> cached;
    {
        std::lock_guard lock(PropertyModelMutex);
        cached = CachedPropertyModel;
    }
    if (cached && cached->model == p.model && cached->instance == p.ancestors[p.ancestorCount - 1] &&
        cached->title == title)
        submission->modelOwner = RetainModelOwner(cached->owner, permit);
    if (submission->modelOwner)
    {
        if (timing) timing->modelCache = 1;
        task->modelContext = cached;
        try { SubmitPropertyCall(cached->context, submit, task); }
        catch (const std::exception& exception) { FinishPropertyError(task, exception.what()); }
        catch (...) { FinishPropertyError(task, "Studio raised an unknown property submission exception"); }
    }
    else
    {
    if (timing)
    {
        timing->modelCache = cached ? (cached->title == title ? 3 : 2) : 0;
        timing->uiQueued = timing->now();
    }
    // Cold lookup retains on the UI thread, where place closure is serialized.
    // The queued DataModel callback does NOT own Submission/modelOwner: a
    // timed-out or closing DataModel must not be kept alive by its own queue.
    dispatch_async_f(dispatch_get_main_queue(), new std::shared_ptr<Submission>(submission), [](void* raw) {
        const auto boxed = std::unique_ptr<std::shared_ptr<Submission>>(static_cast<std::shared_ptr<Submission>*>(raw));
        const auto state = *boxed;
        const auto current = state->task;
        if (current->timing) current->timing->uiBegin = current->timing->now();
        try
        {
            if (std::chrono::steady_clock::now() >= current->deadline)
                throw std::runtime_error("Protected property expired before submission");
            void* model = nullptr;
            void* context = nullptr;
            std::string problem;
            std::uintptr_t owner = 0;
            DataModelLayout layout;
            const auto& params = current->params;
            if (!FindDataModel(state->header, state->slide, state->title, model, layout, problem, current->timing.get(), params.model) ||
                reinterpret_cast<std::uintptr_t>(model) != params.model ||
                params.ancestors[params.ancestorCount - 1] != params.model + layout.instanceOffset ||
                !ResolveDataModelTaskContext(model, layout, state->header, state->submit, context, problem) ||
                !ReadValue(params.model + layout.instanceOffset + 16, owner))
                throw std::runtime_error(problem.empty() ? "Protected property DataModel was replaced" : problem);
            state->modelOwner = RetainModelOwner(reinterpret_cast<void*>(owner), current->permit);
            if (!state->modelOwner)
                throw std::runtime_error("Protected property DataModel expired");
            auto next = std::make_shared<PropertyModelContext>();
            next->model = params.model;
            next->instance = params.ancestors[params.ancestorCount - 1];
            next->context = context;
            next->title = state->title;
            reinterpret_cast<std::atomic<std::int64_t>*>(owner + 16)->fetch_add(1, std::memory_order_relaxed);
            next->owner = reinterpret_cast<void*>(owner);
            current->modelContext = next;
            { std::lock_guard lock(PropertyModelMutex); CachedPropertyModel = std::move(next); }
            if (current->timing) current->timing->uiEnd = current->timing->now();
            SubmitPropertyCall(context, state->submit, current);
        }
        catch (const std::exception& exception)
        {
            FinishPropertyError(current, exception.what());
        }
        catch (...) { FinishPropertyError(current, "Studio raised an unknown property submission exception"); }
    });
    }
    std::unique_lock taskLock(task->mutex);
    if (!task->completed.wait_until(taskLock, task->deadline, [&]() { return task->done; }))
    {
        error = "Protected property task timed out; inspect the value before repeating a write";
        return false;
    }
    error = task->error;
    output = std::move(task->output);
    if (error.empty() && p.operation == 9) {
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(task->deadline - std::chrono::steady_clock::now());
        taskLock.unlock();
        const auto value = task->function->Wait(remaining);
        output.insert(output.end(), value.begin(), value.end());
    }
    return error.empty();
}

static Response PropertyTransport(
    const Request& request,
    const std::string& payload,
    const std::string& title,
    std::vector<unsigned char>& output,
    const std::shared_ptr<PropertyPermit>& permit,
    const std::shared_ptr<PropertyTiming>& timing)
{
    Response response{Magic, 20, 0, 0, {}};
    std::intptr_t slide = 0;
    const auto header = MainStudioImage(slide);
    std::uintptr_t submit = 0;
    std::string error;
    if (!header || !ResolveImageFunction(header, slide, request, request.executeRva,
            "reflection task submitter", submit, error))
    {
        SetError(response, error);
        return response;
    }
    if (request.reserved == 0)
    {
        void* model = nullptr;
        DataModelLayout layout;
        if (!FindDataModel(header, slide, title, model, layout, error, timing.get()))
        {
            SetError(response, error);
            return response;
        }
        const auto instance = reinterpret_cast<std::uintptr_t>(model) + layout.instanceOffset;
        std::string name;
        const auto names = ExpectedDataModelNames(title);
        std::uintptr_t owner = 0;
        if (!ReadExpectedInstanceName(instance, names, name) ||
            std::find(names.begin(), names.end(), name) == names.end() ||
            !ReadValue(instance + 16, owner) || !LikelyPointer(owner))
        {
            SetError(response, "Reflection target does not match the selected Studio place");
            return response;
        }
        const std::uint64_t context[] = {
            reinterpret_cast<std::uintptr_t>(header), instance, owner,
            layout.childrenOffset, CachedInstanceNameOffset.load(std::memory_order_relaxed),
            InstanceClassDescriptorOffset, 8, reinterpret_cast<std::uintptr_t>(model)};
        output.resize(sizeof(context));
        std::memcpy(output.data(), context, sizeof(context));
    }
    else if (request.reserved == 5 && payload.size() == 1)
    {
        std::lock_guard lock(DataModelCacheMutex);
        std::vector<DataModelCandidate> candidates;
        DataModelScanStats stats;
        AddCandidates(header, slide, ExpectedDataModelNames(title), candidates, stats);
        for (const auto& candidate : candidates) {
            const auto model = reinterpret_cast<std::uintptr_t>(candidate.outer);
            const auto instance = model + candidate.instanceOffset;
            std::uintptr_t owner = 0;
            if (!IsDataModelInstance(instance) || !ReadValue(instance + 16, owner) || !LikelyPointer(owner))
                continue;
            const std::uint64_t context[] = {
                reinterpret_cast<std::uintptr_t>(header), instance, owner,
                candidate.childrenOffset, CachedInstanceNameOffset.load(std::memory_order_relaxed),
                InstanceClassDescriptorOffset, 8, model};
            const auto bytes = reinterpret_cast<const unsigned char*>(context);
            output.insert(output.end(), bytes, bytes + sizeof(context));
        }
    }
    else if (request.reserved == 6 && payload.size() == 1)
    {
        // Read-only generation certificate: no pruning, registration or engine call.
        std::lock_guard lock(renium_history::installationMutex);
        const auto binding = renium_history::originalFinish ? renium_history::installed : renium_history::Binding{};
        const auto hook = reinterpret_cast<std::uintptr_t>(&renium_history::ReniumHistoryFinish);
        output.resize(sizeof(binding) + sizeof(hook));
        std::memcpy(output.data(), &binding, sizeof(binding));
        std::memcpy(output.data() + sizeof(binding), &hook, sizeof(hook));
    }
    else if (request.reserved == 7)
    {
        try { renium_mac_playtest::Load(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 8)
    {
        try { renium_mac_teleport::Handle(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 9)
    {
        try { renium_mac_join::Handle(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 10)
    {
        try { renium_mac_gui::Handle(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 11)
    {
        try { ReniumTeleportCaption(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 12)
    {
        try { renium_mac_test_client::Handle(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 14)
    {
        try { renium_mac_route_launch::Handle(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 13)
    {
        try { renium_mac_test_join::Handle(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 15)
    {
        try { ReniumStudioAction(payload, output); }
        catch (const std::exception& error) { SetError(response, error.what()); return response; }
    }
    else if (request.reserved == 1 && payload.size() == 12)
    {
        std::uint64_t address = 0;
        std::uint32_t size = 0;
        std::memcpy(&address, payload.data(), sizeof(address));
        std::memcpy(&size, payload.data() + sizeof(address), sizeof(size));
        if (address < 0x10000 || !size || size > 1024 * 1024 || address > UINT64_MAX - size)
        {
            SetError(response, "Invalid bounded reflection read");
            return response;
        }
        output.resize(size);
        if (!ReadMemory(address, output.data(), size))
        {
            output.clear();
            SetError(response, "Reflection memory changed or is unreadable");
            return response;
        }
    }
    else if (request.reserved == 2)
    {
        if (!RunPropertyCall(payload, title, header, slide, submit, output, error, false, permit, timing))
        {
            SetError(response, error);
            return response;
        }
    }
    else if (request.reserved == 4)
    {
        if (!RunPropertyCall(payload, title, header, slide, submit, output, error, true, permit, timing))
        {
            SetError(response, error);
            return response;
        }
    }
    else if (request.reserved == 3 && !payload.empty() && payload.size() % 12 == 0 && payload.size() / 12 <= 4096)
    {
        // Batched bounded copies only. Address discovery and string decoding
        // remain in Rust; one failed read is reported per entry, not dereferenced.
        std::size_t total = 0;
        for (std::size_t i = 0; i < payload.size(); i += 12)
        {
            std::uint64_t address = 0;
            std::uint32_t size = 0;
            std::memcpy(&address, payload.data() + i, 8);
            std::memcpy(&size, payload.data() + i + 8, 4);
            if (address < 0x10000 || !size || size >= 1024 * 1024 || address > UINT64_MAX - size ||
                total + size + 1 > 1024 * 1024)
            {
                SetError(response, "Invalid bounded reflection batch");
                return response;
            }
            total += size + 1;
        }
        output.resize(total);
        std::size_t cursor = 0;
        for (std::size_t i = 0; i < payload.size(); i += 12)
        {
            std::uint64_t address = 0;
            std::uint32_t size = 0;
            std::memcpy(&address, payload.data() + i, 8);
            std::memcpy(&size, payload.data() + i + 8, 4);
            output[cursor] = ReadMemory(address, output.data() + cursor + 1, size) ? 1 : 0;
            cursor += size + 1;
        }
    }
    else
    {
        SetError(response, "Unsupported reflection transport operation");
        return response;
    }
    response.status = 0;
    response.outputSize = output.size();
    return response;
}

static void RespondToClient(int client, const Request& request, const std::string& path,
    const std::string& title, const std::shared_ptr<PropertyPermit>& permit = {})
{
    Response response{Magic, 7, 0, 0, {}};
    std::vector<unsigned char> propertyOutput;
    std::shared_ptr<PropertyTiming> timing;
    try
    {
        if (request.command == 3 && (request.reserved == 0 || request.reserved == 2 || request.reserved == 4))
            timing = std::make_shared<PropertyTiming>((request.factoryRva & PropertyPhaseTimingFlag) != 0);
        if (request.command == 3)
            response = PropertyTransport(request, path, title, propertyOutput, permit, timing);
        else
            response = request.command == 1
                ? Serialize(request, path, title)
                : PackageAction(request, path, title);
    }
    catch (const std::exception& exception)
    {
        response.status = 8;
        SetError(response, exception.what());
    }
    catch (...)
    {
        response.status = 9;
        SetError(response, "Studio serializer raised an unknown exception");
    }
    if (timing) timing->finish(response);
    WriteExact(client, &response, sizeof(response));
    if (request.command == 3 && response.status == 0 && !propertyOutput.empty())
        WriteExact(client, propertyOutput.data(), propertyOutput.size());
}

struct ClientSocket
{
    int fd;
    explicit ClientSocket(int value) : fd(value) {}
    ClientSocket(const ClientSocket&) = delete;
    ClientSocket& operator=(const ClientSocket&) = delete;
    int release() noexcept { return std::exchange(fd, -1); }
    ~ClientSocket() { if (fd >= 0) close(fd); }
};

struct PropertyClient
{
    std::shared_ptr<PropertyPermit> permit;
    int client;
    Request request;
    std::string path, title;
};

static void* RunPropertyClient(void* raw) noexcept
{
    const auto state = std::unique_ptr<PropertyClient>(static_cast<PropertyClient*>(raw));
    const ClientSocket socket(state->client);
    try
    {
        RespondToClient(socket.fd, state->request, state->path, state->title, state->permit);
    }
    catch (...)
    {
        // Also cover exceptions while packaging an error. Cleanup never
        // depends on the peer accepting the response or still being connected.
        Response response{Magic, 9, 0, 0, {}};
        std::snprintf(response.error, sizeof(response.error), "Native property response failed");
        WriteExact(socket.fd, &response, sizeof(response));
    }
    return nullptr;
}

static bool StartPropertyClient(std::unique_ptr<PropertyClient>& state, std::string& error)
{
    pthread_attr_t attributes;
    auto status = pthread_attr_init(&attributes);
    if (status == 0)
    {
        status = pthread_attr_setdetachstate(&attributes, PTHREAD_CREATE_DETACHED);
        if (status == 0)
        {
            pthread_t thread;
            status = pthread_create(&thread, &attributes, RunPropertyClient, state.get());
        }
        pthread_attr_destroy(&attributes);
    }
    if (status != 0)
    {
        error = std::string("Could not start native property worker: ") + std::strerror(status);
        return false;
    }
    // Creation transfers the socket and state together. No fallible detach
    // follows it, and releasing the pointer does not access the running state.
    state.release();
    return true;
}

// Resolves a writable data address of the loaded Studio image by RVA, so the
// host can only target storage inside a segment the image declares writable.
static bool ResolveImageData(
    const mach_header_64* header,
    std::intptr_t slide,
    const Request& request,
    std::uint64_t rva,
    std::uintptr_t& address,
    std::string& error)
{
    const segment_command_64* text = nullptr;
    const uuid_command* uuid = nullptr;
    std::vector<const segment_command_64*> writable;
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    for (std::uint32_t index = 0; index < header->ncmds; ++index)
    {
        const auto load = reinterpret_cast<const load_command*>(command);
        if (load->cmdsize < sizeof(load_command))
        {
            error = "Studio's loaded image commands are invalid";
            return false;
        }
        if (load->cmd == LC_SEGMENT_64)
        {
            const auto segment = reinterpret_cast<const segment_command_64*>(load);
            if (std::strcmp(segment->segname, "__TEXT") == 0)
                text = segment;
            if (segment->initprot & VM_PROT_WRITE)
                writable.push_back(segment);
        }
        else if (load->cmd == LC_UUID)
            uuid = reinterpret_cast<const uuid_command*>(load);
        command += load->cmdsize;
    }
    if (!text || !uuid || !rva ||
        std::memcmp(uuid->uuid, request.imageUuid, sizeof(request.imageUuid)) != 0)
    {
        error = "Studio's package notice flag does not match its loaded image";
        return false;
    }
    address = static_cast<std::uintptr_t>(text->vmaddr + slide) + rva;
    for (const auto segment : writable)
    {
        const auto start = static_cast<std::uintptr_t>(segment->vmaddr + slide);
        if (address >= start && address < start + segment->vmsize)
            return true;
    }
    error = "Studio's package notice flag is not writable data";
    return false;
}

static bool ResolveImageText(
    const mach_header_64* header,
    std::intptr_t slide,
    const Request& request,
    std::uint64_t rva,
    std::uintptr_t& address,
    std::string& error)
{
    const segment_command_64* text = nullptr;
    const uuid_command* uuid = nullptr;
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    for (std::uint32_t index = 0; index < header->ncmds; ++index)
    {
        const auto load = reinterpret_cast<const load_command*>(command);
        if (load->cmdsize < sizeof(load_command))
        {
            error = "Studio's loaded image commands are invalid";
            return false;
        }
        if (load->cmd == LC_SEGMENT_64)
        {
            const auto segment = reinterpret_cast<const segment_command_64*>(load);
            if (std::strcmp(segment->segname, "__TEXT") == 0)
                text = segment;
        }
        else if (load->cmd == LC_UUID)
            uuid = reinterpret_cast<const uuid_command*>(load);
        command += load->cmdsize;
    }
    if (!text || !uuid || !rva || rva % 4 != 0 || rva + 4 > text->vmsize ||
        std::memcmp(uuid->uuid, request.imageUuid, sizeof(request.imageUuid)) != 0)
    {
        error = "Studio's code patch does not match its loaded image";
        return false;
    }
    address = static_cast<std::uintptr_t>(text->vmaddr + slide) + rva;
    return true;
}

// Studio is re-signed with executable page protection disabled, so a private
// copy of the code page can take the new instruction and run again.
static bool PatchInstruction(std::uintptr_t address, std::uint32_t value, std::string& error)
{
    const auto page = address & ~static_cast<std::uintptr_t>(vm_page_size - 1);
    auto result = mach_vm_protect(
        mach_task_self(), page, vm_page_size, FALSE, VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY);
    if (result != KERN_SUCCESS)
    {
        error = "could not make Studio code writable: " + std::to_string(result);
        return false;
    }
    *reinterpret_cast<volatile std::uint32_t*>(address) = value;
    result = mach_vm_protect(mach_task_self(), page, vm_page_size, FALSE, VM_PROT_READ | VM_PROT_EXECUTE);
    if (result != KERN_SUCCESS)
    {
        error = "could not restore Studio code protection: " + std::to_string(result);
        return false;
    }
    sys_icache_invalidate(reinterpret_cast<void*>(address), sizeof(value));
    if (*reinterpret_cast<volatile std::uint32_t*>(address) != value)
    {
        error = "Studio code patch was not retained";
        return false;
    }
    return true;
}

// True transfers this socket to a bounded task; the accept loop retains all
// other sockets. Waiting for the UI/DataModel queue must not block raw reads.
static bool HandleClient(int client)
{
    Request request{};
    Response response{Magic, 7, 0, 0, {}};
    if (!ReadExact(client, &request, sizeof(request)) || request.magic != Magic ||
        (request.command != 1 && request.command != 2 && request.command != 3 &&
            request.command != 4 && request.command != 5 && request.command != 6 && request.command != 7) ||
        (request.command < 4 && request.pathLength == 0) ||
        request.pathLength >= (request.command == 3 ? 128u * 1024 * 1024 + 66216u : PATH_MAX) || request.titleLength >= PATH_MAX)
    {
        SetError(response, "invalid serializer request");
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    // Existing operation layouts are unchanged. Older clients can keep using
    // unrelated open places; playtest operations require the current generation.
    if (request.version != Version && !(request.version == 12 && !(request.command == 3 && request.reserved >= 7)))
    {
        SetError(response, "incompatible native helper protocol; restart Studio with the matching Renium build");
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    if (request.command == 7)
    {
        unsigned count = 0, muted = 0, pending = 0;
        bool focused = false;
        if (request.pathLength != 0 || request.titleLength != 0 || request.reserved > 4 || request.factoryRva != 0 || request.executeRva != 0)
            SetError(response, "invalid process audio request");
        else if (ReniumStudioAudio(request.reserved, &count, &muted, &pending, &focused, response.error, sizeof(response.error)))
            response.status = 0;
        response.outputSize = (std::uint64_t{count} << 32) | muted;
        response.elapsedMicros = (std::uint64_t{pending} << 32) | static_cast<unsigned>(focused);
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    if (request.command == 4)
    {
        // No Studio engine calls or global application changes: arm this
        // process's AppKit guard before it starts its own test processes.
        if (request.pathLength != 0 || request.titleLength != 0 || request.reserved != 0)
            SetError(response, "invalid background launch request");
        else if (ReniumArmLaunchGuard())
            response.status = 0;
        else
            SetError(response, "could not arm background launch");
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    if (request.command == 6)
    {
        std::intptr_t slide = 0;
        const auto header = MainStudioImage(slide);
        std::string error;
        std::uintptr_t address = 0;
        const auto expected = static_cast<std::uint32_t>(request.factoryRva);
        if (request.pathLength != 0 || request.titleLength != 0 || request.reserved != 0 ||
            request.factoryRva > 0xffffffffu)
            SetError(response, "invalid code patch request");
        else if (!header ||
            !ResolveImageText(header, slide, request, request.executeRva, address, error))
            SetError(response, error);
        else
        {
            const std::uint32_t nop = 0xd503201fu;
            const auto current = *reinterpret_cast<volatile std::uint32_t*>(address);
            if (current == nop)
            {
                response.outputSize = 1;
                response.status = 0;
            }
            else if (current != expected)
                SetError(response, "Studio code changed before its patch");
            else if (!PatchInstruction(address, nop, error))
                SetError(response, error);
            else
            {
                response.outputSize = 0;
                response.status = 0;
            }
        }
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    if (request.command == 5)
    {
        std::intptr_t slide = 0;
        const auto header = MainStudioImage(slide);
        std::string error;
        std::uintptr_t flag = 0;
        if (request.pathLength != 0 || request.titleLength != 0 || request.reserved != 0)
            SetError(response, "invalid package notice request");
        else if (!header ||
            !ResolveImageData(header, slide, request, request.executeRva, flag, error))
            SetError(response, error);
        else
        {
            const auto current = *reinterpret_cast<volatile unsigned char*>(flag);
            if (current > 1)
                SetError(response, "Studio package popup flag is not boolean");
            else
            {
                *reinterpret_cast<volatile unsigned char*>(flag) = 1;
                response.outputSize = current;
                response.status = 0;
            }
        }
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    std::string path(request.pathLength, '\0');
    if (!ReadExact(client, path.data(), path.size()))
    {
        SetError(response, "incomplete serializer request payload");
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    if (request.command == 1 && path.front() != '/')
    {
        SetError(response, "serializer output path must be absolute");
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    std::string title(request.titleLength, '\0');
    if (!title.empty() && !ReadExact(client, title.data(), title.size()))
    {
        SetError(response, "invalid Studio title");
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    if (request.command == 3 && (request.reserved == 2 || request.reserved == 4))
    {
        try
        {
            auto permit = std::make_shared<PropertyPermit>();
            if (!permit->acquired)
            {
                SetError(response, "Native property task limit reached; wait for pending operations");
                WriteExact(client, &response, sizeof(response));
                return false;
            }
            auto state = std::make_unique<PropertyClient>(PropertyClient{
                std::move(permit), client, request, std::move(path), std::move(title)});
            std::string error;
            if (StartPropertyClient(state, error))
                return true;
            SetError(response, error);
        }
        catch (const std::exception& exception)
        {
            SetError(response, exception.what());
        }
        catch (...) { SetError(response, "Could not prepare native property worker"); }
        WriteExact(client, &response, sizeof(response));
        return false;
    }
    RespondToClient(client, request, path, title);
    return false;
}

static void* RunServer(void*)
{
    const auto server = socket(AF_UNIX, SOCK_STREAM, 0);
    if (server < 0)
        return nullptr;
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    std::snprintf(
        SocketPath,
        sizeof(SocketPath),
        "/tmp/renium-studio-%d.sock",
        static_cast<int>(getpid()));
    std::snprintf(address.sun_path, sizeof(address.sun_path), "%s", SocketPath);
    unlink(SocketPath);
    if (bind(server, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0 ||
        chmod(SocketPath, 0600) != 0 || listen(server, 4) != 0)
    {
        close(server);
        unlink(SocketPath);
        return nullptr;
    }
    while (true)
    {
        const auto client = accept(server, nullptr, nullptr);
        if (client < 0)
        {
            if (errno == EINTR)
                continue;
            break;
        }
        ClientSocket socket(client);
        const int noSignal = 1;
        const timeval timeout{3, 0};
        if (setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, sizeof(noSignal)) != 0 ||
            setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0 ||
            setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) != 0)
            continue;
        try
        {
            if (HandleClient(client)) socket.release();
        }
        catch (...)
        {
            Response response{Magic, 9, 0, 0, {}};
            std::snprintf(response.error, sizeof(response.error), "Native request handling failed");
            WriteExact(client, &response, sizeof(response));
        }
    }
    close(server);
    unlink(SocketPath);
    return nullptr;
}

#ifndef RENIUM_NATIVE_HELPER_TEST
__attribute__((constructor)) static void StartReniumHelper()
{
    unsetenv("DYLD_INSERT_LIBRARIES");
    ReniumInitializeLaunchGuard();
    try {
        std::intptr_t slide = 0;
        const auto image = MainStudioImage(slide);
        renium_mac_test_server::Install(image, slide);
    } catch (const std::exception& error) {
        std::fprintf(stderr, "Renium destination server: %s\n", error.what());
        _exit(78);
    }
    pthread_t thread;
    if (pthread_create(&thread, nullptr, RunServer, nullptr) == 0)
        pthread_detach(thread);
}

__attribute__((destructor)) static void StopReniumHelper()
{
    if (SocketPath[0])
        unlink(SocketPath);
}
#endif
