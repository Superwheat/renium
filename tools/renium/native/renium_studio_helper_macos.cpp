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
#include <unordered_set>
#include <utility>
#include <vector>

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
    std::string vectorDetails;
};

static constexpr std::uint32_t Magic = 0x4d4e4552;
static constexpr std::uint32_t Version = 6;
static constexpr std::size_t DataModelInstanceOffsetMin = 0x100;
static constexpr std::size_t DataModelInstanceOffsetMax = 0x400;
static constexpr std::size_t InstanceClassDescriptorOffset = 0x18;
static constexpr std::size_t InstanceChildrenOffsetMin = 0x40;
static constexpr std::size_t InstanceChildrenOffsetMax = 0xc0;
static constexpr std::size_t InstanceNameOffsetMin = 0x20;
static constexpr std::size_t InstanceNameOffsetMax = 0x400;
static std::mutex SerializeMutex;
static char SocketPath[sizeof(((sockaddr_un*)nullptr)->sun_path)]{};
static void* CachedDataModel = nullptr;
static std::size_t CachedDataModelInstanceOffset = 0;
static std::size_t CachedDataModelChildrenOffset = 0;
static std::size_t CachedInstanceNameOffset = 0;
static std::string CachedDataModelTitle;

static void SetError(Response& response, const std::string& error);
static void ReleaseQString(StudioQString& value);
static bool ReadMemory(std::uintptr_t address, void* output, std::size_t size);
template <typename T>
static bool ReadValue(std::uintptr_t address, T& value);

static_assert(sizeof(Request) == 56);
static_assert(sizeof(Response) == 536);

static bool ReadMemory(std::uintptr_t address, void* output, std::size_t size)
{
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
    std::uintptr_t data = address + 1;
    std::size_t size = bytes[0] >> 1;
    if ((bytes[0] & 1) != 0)
    {
        std::memcpy(&data, bytes + 16, sizeof(data));
        std::memcpy(&size, bytes + 8, sizeof(size));
    }
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
    const auto readAt = [&](std::size_t offset)
    {
        std::uintptr_t indirect = 0;
        if (!ReadValue(instance + offset, indirect) || indirect < 0x10000)
            return false;
        for (std::size_t nestedOffset = 0; nestedOffset <= 64; nestedOffset += 8)
        {
            const auto nestedAddress = indirect + nestedOffset;
            if (ReadLibcppName(nestedAddress, value) && matches(value))
                return true;
            if (ReadCString(nestedAddress, value, 256) && matches(value))
                return true;
            std::uintptr_t nested = 0;
            if (!ReadValue(nestedAddress, nested) || nested < 0x10000)
                continue;
            if (ReadLibcppName(nested, value) && matches(value))
                return true;
            if (ReadCString(nested, value, 256) && matches(value))
                return true;
        }
        return false;
    };
    if (CachedInstanceNameOffset)
        return readAt(CachedInstanceNameOffset);
    for (std::size_t offset = InstanceNameOffsetMin;
         offset <= InstanceNameOffsetMax;
         offset += sizeof(void*))
    {
        if (readAt(offset))
        {
            CachedInstanceNameOffset = offset;
            return true;
        }
    }
    value.clear();
    return false;
}

static bool ReadRttiType(std::uintptr_t object, std::string& name)
{
    std::uintptr_t vtable = 0;
    std::uintptr_t typeInfo = 0;
    std::uintptr_t typeName = 0;
    return ReadValue(object, vtable) && vtable &&
        ReadValue(vtable - sizeof(void*), typeInfo) && typeInfo &&
        ReadValue(typeInfo + sizeof(void*), typeName) && typeName &&
        ReadCString(typeName, name, 256);
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

static void AddCandidates(
    const mach_header_64* header,
    std::intptr_t slide,
    const std::vector<std::string>& expectedNames,
    std::vector<DataModelCandidate>& candidates,
    DataModelScanStats& stats)
{
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    std::unordered_set<std::uintptr_t> seen;
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
                        std::size_t instanceOffset = 0;
                        std::size_t childrenOffset = 0;
                        std::uint32_t rootMask = 0;
                        std::size_t rootCount = 0;
                        if (outer < 0x10000 || !seen.insert(outer).second)
                            continue;
                        ++stats.pointers;
                        if (!FindDataModelInstanceOffset(
                                outer,
                                instanceOffset,
                                childrenOffset,
                                rootMask,
                                rootCount,
                                stats))
                            continue;
                        std::string name;
                        ReadExpectedInstanceName(outer + instanceOffset, expectedNames, name);
                        candidates.push_back(
                            {reinterpret_cast<void*>(outer),
                             instanceOffset,
                             childrenOffset,
                             rootMask,
                             rootCount,
                             std::move(name)});
                    }
                }
            }
        }
        if (load->cmdsize < sizeof(load_command))
            break;
        command += load->cmdsize;
    }
}

static bool FindDataModel(
    const mach_header_64* header,
    std::intptr_t slide,
    const std::string& title,
    void*& output,
    std::string& error)
{
    if (CachedDataModel)
    {
        const auto cached = reinterpret_cast<std::uintptr_t>(CachedDataModel);
        const auto instance = cached + CachedDataModelInstanceOffset;
        std::vector<SharedInstance> children;
        std::size_t readableClasses = 0;
        if (CachedDataModelTitle == title && CachedDataModelInstanceOffset &&
            CachedDataModelChildrenOffset &&
            IsDataModelInstance(instance) &&
            ReadChildren(instance, CachedDataModelChildrenOffset, children) &&
            (RequiredRootMask(children, readableClasses) & 15) == 15)
        {
            output = CachedDataModel;
            return true;
        }
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
    AddCandidates(header, slide, ExpectedDataModelNames(title), candidates, stats);
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
    return true;
}

struct PackagePathSegment
{
    std::string name;
    std::uint32_t ordinal;
};

struct ResolvedPackage
{
    SharedInstance root;
    SharedInstance link;
};

static bool DecodePackagePath(
    const std::string& payload,
    std::vector<PackagePathSegment>& segments,
    std::int64_t& expectedVersion,
    std::string& error)
{
    std::size_t cursor = 0;
    const auto readU32 = [&](std::uint32_t& output)
    {
        if (cursor > payload.size() || payload.size() - cursor < sizeof(output))
            return false;
        std::memcpy(&output, payload.data() + cursor, sizeof(output));
        cursor += sizeof(output);
        return true;
    };
    std::uint32_t payloadVersion = 0;
    std::uint64_t rawExpectedVersion = 0;
    std::uint32_t count = 0;
    if (!readU32(payloadVersion))
    {
        error = "package target payload is truncated";
        return false;
    }
    if (payloadVersion != 9)
    {
        error = "unsupported package target payload version " +
            std::to_string(payloadVersion);
        return false;
    }
    if (cursor > payload.size() ||
        payload.size() - cursor < sizeof(rawExpectedVersion))
    {
        error = "package target has no expected version";
        return false;
    }
    std::memcpy(
        &rawExpectedVersion,
        payload.data() + cursor,
        sizeof(rawExpectedVersion));
    cursor += sizeof(rawExpectedVersion);
    if (!rawExpectedVersion ||
        rawExpectedVersion > static_cast<std::uint64_t>(INT64_MAX) ||
        !readU32(count) || count < 2 || count > 64)
    {
        error = "package target must contain 2-64 path segments";
        return false;
    }
    expectedVersion = static_cast<std::int64_t>(rawExpectedVersion);
    segments.reserve(count);
    for (std::uint32_t index = 0; index < count; ++index)
    {
        std::uint32_t length = 0;
        std::uint32_t ordinal = 0;
        if (!readU32(length) || !readU32(ordinal) || !length || length > 4096 ||
            cursor > payload.size() || payload.size() - cursor < length)
        {
            error = "package target path is malformed";
            return false;
        }
        segments.push_back({payload.substr(cursor, length), ordinal});
        cursor += length;
    }
    if (cursor != payload.size())
    {
        error = "package target contains trailing data";
        return false;
    }
    return true;
}

static bool InstanceMatchesName(std::uintptr_t instance, const std::string& expected)
{
    std::string value;
    return ReadExpectedInstanceName(instance, {expected}, value);
}

static bool InstanceMatchesClass(std::uintptr_t instance, const std::string& expected)
{
    std::string value;
    if (ReadInstanceClass(instance, value) && value == expected)
        return true;
    return ReadRttiType(instance, value) &&
        value == "N3RBX" + std::to_string(expected.size()) + expected + "E";
}

static bool ResolvePackage(
    void* dataModel,
    const std::vector<PackagePathSegment>& segments,
    ResolvedPackage& package,
    std::string& error)
{
    if (!CachedDataModelInstanceOffset || !CachedDataModelChildrenOffset)
    {
        error = "Studio DataModel layout is unavailable";
        return false;
    }
    std::vector<SharedInstance> children;
    const auto dataModelInstance =
        reinterpret_cast<std::uintptr_t>(dataModel) + CachedDataModelInstanceOffset;
    if (!ReadChildren(dataModelInstance, CachedDataModelChildrenOffset, children))
    {
        error = "Studio DataModel roots changed while resolving the package";
        return false;
    }
    SharedInstance current{};
    for (std::size_t depth = 0; depth < segments.size(); ++depth)
    {
        const auto& segment = segments[depth];
        std::vector<SharedInstance> matches;
        for (const auto& child : children)
        {
            std::string className;
            if (child.instance &&
                (depth == 0
                     ? InstanceMatchesClass(
                           reinterpret_cast<std::uintptr_t>(child.instance), segment.name)
                     : InstanceMatchesName(
                           reinterpret_cast<std::uintptr_t>(child.instance), segment.name)))
                matches.push_back(child);
        }
        if (segment.ordinal)
        {
            if (segment.ordinal > matches.size())
            {
                error = "package path ordinal does not match Studio";
                return false;
            }
            current = matches[segment.ordinal - 1];
        }
        else if (matches.size() == 1)
            current = matches.front();
        else
        {
            error = "package path segment '" + segment.name + "' at depth " +
                std::to_string(depth + 1) + " resolved to " +
                std::to_string(matches.size()) + " instances";
            if (matches.empty())
            {
                error += " among [";
                for (const auto& child : children)
                {
                    std::string className;
                    const auto instance = reinterpret_cast<std::uintptr_t>(child.instance);
                    if (!ReadInstanceClass(instance, className) &&
                        !ReadRttiType(instance, className))
                        continue;
                    if (error.back() != '[')
                        error += ',';
                    error += className;
                    if (error.size() > 420)
                        break;
                }
                error += "]";
            }
            error += "; add --ords";
            return false;
        }
        if (depth == 0 &&
            !InstanceMatchesName(
                reinterpret_cast<std::uintptr_t>(current.instance), segment.name))
        {
            error = "could not locate Studio's Instance name field";
            return false;
        }
        if (depth + 1 < segments.size() &&
            !ReadChildren(
                reinterpret_cast<std::uintptr_t>(current.instance),
                CachedDataModelChildrenOffset,
                children))
        {
            error = "package children changed while resolving its path";
            return false;
        }
    }
    if (InstanceMatchesClass(
            reinterpret_cast<std::uintptr_t>(current.instance), "PackageLink"))
    {
        error = "target the package root, not its PackageLink child";
        return false;
    }
    if (!ReadChildren(
            reinterpret_cast<std::uintptr_t>(current.instance),
            CachedDataModelChildrenOffset,
            children))
    {
        error = "package root children changed while locating PackageLink";
        return false;
    }
    std::vector<SharedInstance> links;
    for (const auto& child : children)
    {
        if (child.instance && InstanceMatchesClass(
                reinterpret_cast<std::uintptr_t>(child.instance), "PackageLink"))
            links.push_back(child);
    }
    if (links.size() != 1)
    {
        error = "package root contains " + std::to_string(links.size()) +
            " direct PackageLink children";
        return false;
    }
    package = {current, links.front()};
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

static bool ObjectContainsString(std::uintptr_t object, const std::string& expected)
{
    std::string value;
    for (std::size_t offset = 0; offset <= 64; offset += sizeof(void*))
    {
        const auto address = object + offset;
        if (ReadLibcppName(address, value) && value == expected)
            return true;
        if (ReadCString(address, value, 256) && value == expected)
            return true;
        std::uintptr_t nested = 0;
        if (!ReadValue(address, nested) || !LikelyPointer(nested))
            continue;
        if (ReadLibcppName(nested, value) && value == expected)
            return true;
        if (ReadCString(nested, value, 256) && value == expected)
            return true;
    }
    return false;
}

static bool MemberMatchesName(std::uintptr_t member, const std::string& wanted)
{
    std::uintptr_t nameObject = 0;
    return ReadValue(member + 8, nameObject) && LikelyPointer(nameObject) &&
        ObjectContainsString(nameObject, wanted);
}

static bool FindClassMember(
    std::uintptr_t instance,
    const std::string& wanted,
    std::uintptr_t& output,
    std::string& error)
{
    std::unordered_set<std::uintptr_t> matches;
    for (std::size_t instanceOffset = 0;
         instanceOffset <= 0x100;
         instanceOffset += sizeof(void*))
    {
        std::uintptr_t descriptor = 0;
        if (!ReadValue(instance + instanceOffset, descriptor) ||
            !LikelyPointer(descriptor))
            continue;
        for (std::size_t offset = 0; offset <= 0x200; offset += sizeof(void*))
        {
            std::uintptr_t entries = 0;
            std::uint64_t count = 0;
            std::uint64_t capacity = 0;
            if (!ReadValue(descriptor + offset, entries) ||
                !ReadValue(descriptor + offset + 8, count) ||
                !ReadValue(descriptor + offset + 16, capacity) ||
                !LikelyPointer(entries) || !count || count > 512 ||
                capacity < count || capacity > 1024)
                continue;
            for (std::uint64_t index = 0; index < count; ++index)
            {
                std::uintptr_t member = 0;
                if (ReadValue(entries + index * 16, member) &&
                    LikelyPointer(member) && MemberMatchesName(member, wanted))
                    matches.insert(member);
            }
        }
    }
    if (matches.size() != 1)
    {
        error = "Reflection member '" + wanted + "' resolved to " +
            std::to_string(matches.size()) + " descriptors";
        return false;
    }
    output = *matches.begin();
    return true;
}

static bool ResolvePackageUiBinding(
    void* dataModel,
    const mach_header_64* header,
    const char* memberName,
    const char* signatureToken,
    void*& service,
    std::uintptr_t& function,
    std::string& error)
{
    const auto dataModelInstance = reinterpret_cast<std::uintptr_t>(dataModel) +
        CachedDataModelInstanceOffset;
    std::vector<SharedInstance> roots;
    if (!ReadChildren(dataModelInstance, CachedDataModelChildrenOffset, roots))
    {
        error = "Studio DataModel roots changed while resolving PackageUIService";
        return false;
    }
    std::vector<std::uintptr_t> services;
    for (const auto& root : roots)
    {
        const auto address = reinterpret_cast<std::uintptr_t>(root.instance);
        if (address && InstanceMatchesClass(address, "PackageUIService"))
            services.push_back(address);
    }
    if (services.size() != 1)
    {
        error = "Studio resolved " + std::to_string(services.size()) +
            " PackageUIService instances";
        return false;
    }
    std::uintptr_t descriptor = 0;
    if (!FindClassMember(services.front(), memberName, descriptor, error))
        return false;
    std::string descriptorType;
    if (!ReadRttiType(descriptor, descriptorType) ||
        descriptorType.find("BoundYieldFuncDesc") == std::string::npos ||
        descriptorType.find("PackageUIService") == std::string::npos ||
        descriptorType.find(signatureToken) == std::string::npos)
    {
        error = "PackageUIService." + std::string(memberName) +
            " has an unsupported reflection binding";
        return false;
    }
    std::uintptr_t member = 0;
    std::intptr_t encodedAdjustment = 0;
    if (!ReadValue(descriptor + 0x78, member) ||
        !ReadValue(descriptor + 0x80, encodedAdjustment) ||
        (encodedAdjustment & 1) == 0 || member >= 0x1000 ||
        member % sizeof(void*) != 0)
    {
        error = "PackageUIService." + std::string(memberName) +
            " has an unsupported member-function binding";
        return false;
    }
    const auto adjustment = encodedAdjustment >> 1;
    if (adjustment < -0x1000 || adjustment > 0x1000)
    {
        error = "PackageUIService." + std::string(memberName) +
            " has an invalid class adjustment";
        return false;
    }
    const auto adjustedService = static_cast<std::uintptr_t>(
        static_cast<std::intptr_t>(services.front()) + adjustment);
    std::uintptr_t vtable = 0;
    Dl_info info{};
    if (!ReadValue(adjustedService, vtable) || !LikelyPointer(vtable) ||
        !ReadValue(vtable + member, function) ||
        !LikelyCodePointer(function) ||
        !dladdr(reinterpret_cast<void*>(function), &info) ||
        info.dli_fbase != header)
    {
        error = "PackageUIService." + std::string(memberName) +
            " resolved an invalid implementation";
        return false;
    }
    service = reinterpret_cast<void*>(adjustedService);
    return true;
}

struct PackagePropertyBinding
{
    void* target = nullptr;
    std::uintptr_t getter = 0;
    std::uintptr_t setter = 0;
    std::intptr_t getterAdjustment = 0;
    std::intptr_t setterAdjustment = 0;
    std::size_t instanceAdjustment = 0;
    std::size_t fieldOffset = 0;
    std::size_t fieldWidth = 0;
};

struct PackagePropertyLayout
{
    PackagePropertyBinding modified;
    PackagePropertyBinding hasNewVersion;
    PackagePropertyBinding version;
    bool valid = false;
};

static PackagePropertyLayout CachedPackagePropertyLayout;

static bool DecodeIntegerGetter(
    std::uintptr_t getter,
    std::size_t& offset,
    std::size_t& width)
{
    std::uint32_t code[2]{};
    if (!ReadMemory(getter, code, sizeof(code)) || code[1] != 0xd65f03c0)
        return false;
    const auto masked = code[0] & 0xffc003ff;
    if (masked == 0x39400000)
        width = 1;
    else if (masked == 0xb9400000 || masked == 0xb9800000)
        width = 4;
    else if (masked == 0xf9400000)
        width = 8;
    else
        return false;
    offset = ((code[0] >> 10) & 0xfff) * width;
    return offset < 0x1000;
}

static bool ResolveMemberFunction(
    std::uintptr_t target,
    std::uintptr_t member,
    const mach_header_64* header,
    std::uintptr_t& function)
{
    if (member & 1)
    {
        std::uintptr_t vtable = 0;
        if (!ReadValue(target, vtable) || !LikelyPointer(vtable) ||
            !ReadValue(vtable + member - 1, function))
            return false;
    }
    else
        function = member;
    Dl_info info{};
    return LikelyCodePointer(function) &&
        dladdr(reinterpret_cast<void*>(function), &info) && info.dli_fbase == header;
}

static bool ResolvePackageProperty(
    std::uintptr_t link,
    const std::string& name,
    bool requireSetter,
    const mach_header_64* header,
    PackagePropertyBinding& output,
    std::string& error)
{
    std::uintptr_t descriptor = 0;
    if (!FindClassMember(link, name, descriptor, error))
        return false;
    std::uintptr_t binding = 0;
    if (!ReadValue(descriptor + 0x90, binding) || !LikelyPointer(binding))
    {
        error = "PackageLink." + name + " binding is invalid";
        return false;
    }
    std::string type;
    if (!ReadRttiType(binding, type) || type.find("PropDescriptor") == std::string::npos ||
        type.find("GetSetImpl") == std::string::npos)
    {
        error = "PackageLink." + name + " has an unsupported reflection binding";
        return false;
    }
    std::uintptr_t getterMember = 0;
    std::intptr_t getterAdjustment = 0;
    std::uintptr_t setterMember = 0;
    std::intptr_t setterAdjustment = 0;
    if (!ReadValue(binding + 8, getterMember) ||
        !ReadValue(binding + 16, getterAdjustment) ||
        !ReadValue(binding + 24, setterMember) ||
        !ReadValue(binding + 32, setterAdjustment))
    {
        error = "PackageLink." + name + " member pointers are unreadable";
        return false;
    }
    if (getterAdjustment < -0x1000 || getterAdjustment > 0x1000 ||
        setterAdjustment < -0x1000 || setterAdjustment > 0x1000)
    {
        error = "PackageLink." + name + " has an invalid class adjustment";
        return false;
    }
    if (!ResolveMemberFunction(link, getterMember, header, output.getter) ||
        (requireSetter &&
         !ResolveMemberFunction(link, setterMember, header, output.setter)) ||
        !DecodeIntegerGetter(output.getter, output.fieldOffset, output.fieldWidth))
    {
        error = "PackageLink." + name + " member functions are invalid";
        return false;
    }
    output.getterAdjustment = getterAdjustment;
    output.setterAdjustment = setterAdjustment;
    return true;
}

static bool ReadPackageInteger(
    std::uintptr_t address,
    std::size_t width,
    std::int64_t& value)
{
    if (width == 1)
    {
        std::uint8_t current = 0;
        if (!ReadValue(address, current))
            return false;
        value = current;
        return true;
    }
    if (width == 4)
    {
        std::int32_t current = 0;
        if (!ReadValue(address, current))
            return false;
        value = current;
        return true;
    }
    if (width == 8)
        return ReadValue(address, value);
    return false;
}

static bool ReadPackageIntegerBytes(
    const unsigned char* bytes,
    std::size_t width,
    std::int64_t& value)
{
    if (width == 1)
    {
        value = *bytes;
        return true;
    }
    if (width == 4)
    {
        std::int32_t current = 0;
        std::memcpy(&current, bytes, sizeof(current));
        value = current;
        return true;
    }
    if (width == 8)
    {
        std::memcpy(&value, bytes, sizeof(value));
        return true;
    }
    return false;
}

static bool ResolvePackagePropertyTargets(
    std::uintptr_t link,
    PackagePropertyBinding& modified,
    PackagePropertyBinding& hasNewVersion,
    PackagePropertyBinding& version,
    std::int64_t expectedVersion,
    std::string& error)
{
    std::vector<std::size_t> matches;
    std::ostringstream details;
    constexpr std::size_t scanSize = 0x1000;
    const auto readWindow = [link](
                                const PackagePropertyBinding& binding,
                                std::vector<unsigned char>& bytes)
    {
        bytes.resize(scanSize + binding.fieldWidth);
        const auto start = link + binding.getterAdjustment + binding.fieldOffset;
        return ReadMemory(start, bytes.data(), bytes.size());
    };
    std::vector<unsigned char> modifiedBytes;
    std::vector<unsigned char> hasNewVersionBytes;
    std::vector<unsigned char> versionBytes;
    const auto haveWindows = readWindow(modified, modifiedBytes) &&
        readWindow(hasNewVersion, hasNewVersionBytes) &&
        readWindow(version, versionBytes);
    for (std::size_t adjustment = 0; adjustment <= 0x1000; adjustment += 8)
    {
        std::int64_t modifiedValue = 0;
        std::int64_t hasNewVersionValue = 0;
        std::int64_t versionValue = 0;
        const auto read = haveWindows
            ? ReadPackageIntegerBytes(
                  modifiedBytes.data() + adjustment,
                  modified.fieldWidth,
                  modifiedValue) &&
                ReadPackageIntegerBytes(
                    hasNewVersionBytes.data() + adjustment,
                    hasNewVersion.fieldWidth,
                    hasNewVersionValue) &&
                ReadPackageIntegerBytes(
                    versionBytes.data() + adjustment,
                    version.fieldWidth,
                    versionValue)
            : ReadPackageInteger(
                  link + adjustment + modified.getterAdjustment + modified.fieldOffset,
                  modified.fieldWidth,
                  modifiedValue) &&
                ReadPackageInteger(
                    link + adjustment + hasNewVersion.getterAdjustment +
                        hasNewVersion.fieldOffset,
                    hasNewVersion.fieldWidth,
                    hasNewVersionValue) &&
                ReadPackageInteger(
                    link + adjustment + version.getterAdjustment + version.fieldOffset,
                    version.fieldWidth,
                    versionValue);
        if (read &&
            (modifiedValue == -1 || modifiedValue == 1) &&
            (hasNewVersionValue == 0 || hasNewVersionValue == 1) &&
            versionValue == expectedVersion)
        {
            matches.push_back(adjustment);
            details << std::hex << adjustment << ':' << std::dec << modifiedValue << '/'
                    << hasNewVersionValue << '/' << versionValue << ',';
        }
    }
    if (matches.size() != 1)
    {
        error = "PackageLink property base resolved to " +
            std::to_string(matches.size()) + " candidates [" + details.str() + "]";
        return false;
    }
    const auto adjustment = matches.front();
    std::int64_t modifiedValue = 0;
    std::int64_t hasNewVersionValue = 0;
    std::int64_t versionValue = 0;
    if (!ReadPackageInteger(
            link + adjustment + modified.getterAdjustment + modified.fieldOffset,
            modified.fieldWidth,
            modifiedValue) ||
        !ReadPackageInteger(
            link + adjustment + hasNewVersion.getterAdjustment +
                hasNewVersion.fieldOffset,
            hasNewVersion.fieldWidth,
            hasNewVersionValue) ||
        !ReadPackageInteger(
            link + adjustment + version.getterAdjustment + version.fieldOffset,
            version.fieldWidth,
            versionValue) ||
        !(modifiedValue == -1 || modifiedValue == 1) ||
        !(hasNewVersionValue == 0 || hasNewVersionValue == 1) ||
        versionValue != expectedVersion)
    {
        error = "PackageLink property values are invalid";
        return false;
    }
    modified.target = reinterpret_cast<void*>(
        link + adjustment + modified.getterAdjustment);
    hasNewVersion.target = reinterpret_cast<void*>(
        link + adjustment + hasNewVersion.getterAdjustment);
    version.target = reinterpret_cast<void*>(link + adjustment + version.getterAdjustment);
    modified.instanceAdjustment = adjustment;
    hasNewVersion.instanceAdjustment = adjustment;
    version.instanceAdjustment = adjustment;
    if (modified.getterAdjustment != modified.setterAdjustment)
    {
        error = "PackageLink.ModifiedState getter/setter adjustment differs";
        return false;
    }
    return true;
}

static bool ResolvePackageProperties(
    std::uintptr_t link,
    std::int64_t expectedVersion,
    const mach_header_64* header,
    PackagePropertyBinding& modified,
    PackagePropertyBinding& hasNewVersion,
    PackagePropertyBinding& version,
    std::string& error)
{
    if (CachedPackagePropertyLayout.valid)
    {
        modified = CachedPackagePropertyLayout.modified;
        hasNewVersion = CachedPackagePropertyLayout.hasNewVersion;
        version = CachedPackagePropertyLayout.version;
        const auto setTarget = [link](PackagePropertyBinding& binding)
        {
            binding.target = reinterpret_cast<void*>(
                link + binding.instanceAdjustment + binding.getterAdjustment);
        };
        setTarget(modified);
        setTarget(hasNewVersion);
        setTarget(version);
        std::int64_t modifiedValue = 0;
        std::int64_t hasNewVersionValue = 0;
        std::int64_t versionValue = 0;
        if (ReadPackageInteger(
                reinterpret_cast<std::uintptr_t>(modified.target) + modified.fieldOffset,
                modified.fieldWidth,
                modifiedValue) &&
            ReadPackageInteger(
                reinterpret_cast<std::uintptr_t>(hasNewVersion.target) +
                    hasNewVersion.fieldOffset,
                hasNewVersion.fieldWidth,
                hasNewVersionValue) &&
            ReadPackageInteger(
                reinterpret_cast<std::uintptr_t>(version.target) + version.fieldOffset,
                version.fieldWidth,
                versionValue) &&
            (modifiedValue == -1 || modifiedValue == 1) &&
            (hasNewVersionValue == 0 || hasNewVersionValue == 1) &&
            versionValue == expectedVersion)
            return true;
        CachedPackagePropertyLayout.valid = false;
    }
    if (!ResolvePackageProperty(link, "ModifiedState", true, header, modified, error) ||
        !ResolvePackageProperty(
            link, "HasNewVersion", false, header, hasNewVersion, error) ||
        !ResolvePackageProperty(link, "VersionNumber", false, header, version, error) ||
        !ResolvePackagePropertyTargets(
            link,
            modified,
            hasNewVersion,
            version,
            expectedVersion,
            error))
        return false;
    CachedPackagePropertyLayout = {modified, hasNewVersion, version, true};
    CachedPackagePropertyLayout.modified.target = nullptr;
    CachedPackagePropertyLayout.hasNewVersion.target = nullptr;
    CachedPackagePropertyLayout.version.target = nullptr;
    return true;
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

static void ReleaseOwner(void* owner)
{
    auto bytes = reinterpret_cast<unsigned char*>(owner);
    auto strong = reinterpret_cast<std::atomic<std::int64_t>*>(bytes + 8);
    if (strong->fetch_sub(1, std::memory_order_acq_rel) == 0)
    {
        const auto vtable = *reinterpret_cast<void***>(owner);
        reinterpret_cast<void (*)(void*)>(vtable[2])(owner);
        auto weak = reinterpret_cast<std::atomic<std::int64_t>*>(bytes + 16);
        if (weak->fetch_sub(1, std::memory_order_acq_rel) == 0)
            reinterpret_cast<void (*)(void*)>(vtable[3])(owner);
    }
}

struct PackageStateTask
{
    std::atomic<int> references{1};
    std::mutex mutex;
    std::condition_variable completed;
    std::chrono::steady_clock::time_point deadline;
    void* owner = nullptr;
    PackagePropertyBinding modified;
    PackagePropertyBinding hasNewVersion;
    PackagePropertyBinding version;
    SharedInstance root{};
    void* packageUiService = nullptr;
    std::uintptr_t packageMethod = 0;
    std::int64_t targetVersion = 0;
    void* taskContext = nullptr;
    std::uintptr_t submitTask = 0;
    std::shared_ptr<struct PackageOperationCompletion> operationCompletion;
    bool write = false;
    int desiredModifiedValue = 1;
    bool done = false;
    bool changed = false;
    int modifiedValue = 0;
    int hasNewVersionValue = 0;
    std::int64_t versionValue = 0;
    std::string error;
};

struct PackageOperationCompletion
{
    std::mutex mutex;
    std::condition_variable completed;
    bool done = false;
    std::string error;
};

static void ReleasePackageStateTask(PackageStateTask* task);

static void ReleasePackageStateTask(PackageStateTask* task)
{
    if (task->references.fetch_sub(1, std::memory_order_acq_rel) == 1)
    {
        if (task->owner)
            ReleaseOwner(task->owner);
        delete task;
    }
}

static bool SubmitPackageTask(
    PackageStateTask* task,
    void (*callback)(void*),
    const char* rejected,
    const char* timedOut,
    std::string& error)
{
    {
        std::lock_guard lock(task->mutex);
        task->done = false;
        task->error.clear();
    }
    task->references.fetch_add(1, std::memory_order_relaxed);
    std::function<void()> operation{[task, callback]() { callback(task); }};
    const auto submit = reinterpret_cast<bool (*)(
        void*, std::function<void()>*, std::uint32_t)>(task->submitTask);
    if (!submit(task->taskContext, &operation, 1))
    {
        ReleasePackageStateTask(task);
        error = rejected;
        return false;
    }
    std::unique_lock taskLock(task->mutex);
    if (!task->completed.wait_until(
            taskLock, task->deadline, [&]() { return task->done; }))
    {
        error = timedOut;
        return false;
    }
    error = task->error;
    return error.empty();
}

static void RunPackagePublishTask(void* context)
{
    auto task = static_cast<PackageStateTask*>(context);
    if (std::chrono::steady_clock::now() < task->deadline)
    {
        try
        {
            using PublishPackage = void (*)(
                void*,
                const SharedInstance*,
                bool,
                const std::function<void()>*,
                const std::function<void(std::string)>*);
            const auto completion = std::make_shared<PackageOperationCompletion>();
            task->operationCompletion = completion;
            std::function<void()> succeeded{[completion]()
            {
                {
                    std::lock_guard lock(completion->mutex);
                    completion->done = true;
                }
                completion->completed.notify_one();
            }};
            std::function<void(std::string)> failed{[completion](std::string message)
            {
                {
                    std::lock_guard lock(completion->mutex);
                    completion->error = std::move(message);
                    completion->done = true;
                }
                completion->completed.notify_one();
            }};
            const auto publish = reinterpret_cast<PublishPackage>(task->packageMethod);
            publish(
                task->packageUiService,
                &task->root,
                false,
                &succeeded,
                &failed);
        }
        catch (const std::exception& exception)
        {
            task->error = exception.what();
        }
        catch (...)
        {
            task->error = "Studio package publish raised an unknown exception";
        }
    }
    else
        task->error = "Studio package publish expired before execution";
    {
        std::lock_guard lock(task->mutex);
        task->done = true;
    }
    task->completed.notify_one();
    ReleasePackageStateTask(task);
}

static bool RunPackagePublish(
    PackageStateTask* task,
    std::string& error)
{
    if (!SubmitPackageTask(
            task,
            RunPackagePublishTask,
            "Studio rejected the package publish task",
            "Studio package publish task timed out",
            error))
        return false;
    const auto completion = task->operationCompletion;
    if (!completion)
    {
        error = "Studio package publish returned no completion state";
        return false;
    }
    std::unique_lock lock(completion->mutex);
    if (!completion->completed.wait_until(
            lock, task->deadline, [&]() { return completion->done; }))
    {
        error = "Package did not finish publishing before the deadline";
        return false;
    }
    error = completion->error;
    return error.empty();
}

static void RunPackageUpdateTask(void* context)
{
    auto task = static_cast<PackageStateTask*>(context);
    if (std::chrono::steady_clock::now() < task->deadline)
    {
        try
        {
            using SharedObject = std::shared_ptr<void>;
            using SetPackageVersion = void (*)(
                void*,
                const SharedObject*,
                std::int64_t,
                const std::function<void(SharedObject)>*,
                const std::function<void(std::string)>*);
            const auto completion = std::make_shared<PackageOperationCompletion>();
            task->operationCompletion = completion;
            std::function<void(SharedObject)> succeeded{[completion](SharedObject)
            {
                {
                    std::lock_guard lock(completion->mutex);
                    completion->done = true;
                }
                completion->completed.notify_one();
            }};
            std::function<void(std::string)> failed{[completion](std::string message)
            {
                {
                    std::lock_guard lock(completion->mutex);
                    completion->error = std::move(message);
                    completion->done = true;
                }
                completion->completed.notify_one();
            }};
            const auto update = reinterpret_cast<SetPackageVersion>(task->packageMethod);
            update(
                task->packageUiService,
                reinterpret_cast<const SharedObject*>(&task->root),
                task->targetVersion,
                &succeeded,
                &failed);
        }
        catch (const std::exception& exception)
        {
            task->error = exception.what();
        }
        catch (...)
        {
            task->error = "Studio package update raised an unknown exception";
        }
    }
    else
        task->error = "Studio package update expired before execution";
    {
        std::lock_guard lock(task->mutex);
        task->done = true;
    }
    task->completed.notify_one();
    ReleasePackageStateTask(task);
}

static bool RunPackageUpdate(
    PackageStateTask* task,
    std::string& error)
{
    if (!SubmitPackageTask(
            task,
            RunPackageUpdateTask,
            "Studio rejected the package update task",
            "Studio package update task timed out",
            error))
        return false;
    const auto completion = task->operationCompletion;
    if (!completion)
    {
        error = "Studio package update returned no completion state";
        return false;
    }
    std::unique_lock lock(completion->mutex);
    if (!completion->completed.wait_until(
            lock, task->deadline, [&]() { return completion->done; }))
    {
        error = "Package did not finish updating before the deadline";
        return false;
    }
    error = completion->error;
    return error.empty();
}

static bool ReadPackageState(
    PackageStateTask* task,
    std::uintptr_t link,
    int& modified,
    bool& hasNewVersion,
    std::int64_t& version)
{
    const auto propertyAddress = [link](const PackagePropertyBinding& binding)
    {
        return link + binding.instanceAdjustment +
            binding.getterAdjustment + binding.fieldOffset;
    };
    std::int64_t modifiedValue = 0;
    std::int64_t hasNewVersionValue = 0;
    if (!ReadPackageInteger(
            propertyAddress(task->modified),
            task->modified.fieldWidth,
            modifiedValue) ||
        !ReadPackageInteger(
            propertyAddress(task->hasNewVersion),
            task->hasNewVersion.fieldWidth,
            hasNewVersionValue) ||
        !ReadPackageInteger(
            propertyAddress(task->version),
            task->version.fieldWidth,
            version) ||
        !(modifiedValue == -1 || modifiedValue == 1) ||
        !(hasNewVersionValue == 0 || hasNewVersionValue == 1) || version <= 0)
        return false;
    modified = static_cast<int>(modifiedValue);
    hasNewVersion = hasNewVersionValue != 0;
    return true;
}

static void RunPackageStateTask(void* context)
{
    auto task = static_cast<PackageStateTask*>(context);
    using Getter = std::int64_t (*)(void*);
    using Setter = void (*)(void*, int);
    if (std::chrono::steady_clock::now() < task->deadline)
    {
        try
        {
            const auto getter = reinterpret_cast<Getter>(task->modified.getter);
            const auto initial = static_cast<int>(getter(task->modified.target));
            if (task->write && initial != task->desiredModifiedValue)
            {
                reinterpret_cast<Setter>(task->modified.setter)(
                    task->modified.target, task->desiredModifiedValue);
                task->changed = true;
            }
            if (task->error.empty())
            {
                task->modifiedValue = static_cast<int>(
                    reinterpret_cast<Getter>(task->modified.getter)(
                        task->modified.target));
                task->hasNewVersionValue = static_cast<int>(
                    reinterpret_cast<Getter>(task->hasNewVersion.getter)(
                        task->hasNewVersion.target));
                task->versionValue = reinterpret_cast<Getter>(task->version.getter)(
                    task->version.target);
            }
        }
        catch (const std::exception& exception)
        {
            task->error = exception.what();
        }
        catch (...)
        {
            task->error = "Studio package property raised an unknown exception";
        }
    }
    else
        task->error = "Studio package action expired before execution";
    {
        std::lock_guard lock(task->mutex);
        task->done = true;
    }
    task->completed.notify_one();
    ReleasePackageStateTask(task);
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
    void* dataModel,
    const mach_header_64* header,
    std::uintptr_t submitter,
    void*& taskContext,
    std::string& error)
{
    const auto dataModelInstance = reinterpret_cast<std::uintptr_t>(dataModel) +
        CachedDataModelInstanceOffset;
    std::uintptr_t context = 0;
    std::uintptr_t vtable = 0;
    if (!ReadValue(dataModelInstance + 0x58, context))
    {
        error = "Studio's DataModel task context is unreadable";
        return false;
    }
    context &= ~std::uintptr_t{7};
    if (!LikelyPointer(context) || !ReadValue(context, vtable) ||
        !LikelyPointer(vtable))
    {
        error = "Studio's DataModel task context is invalid";
        return false;
    }
    for (std::size_t index = 0; index < 16; ++index)
    {
        std::uintptr_t method = 0;
        Dl_info info{};
        if (!ReadValue(vtable + index * sizeof(void*), method) ||
            !LikelyCodePointer(method) ||
            !dladdr(reinterpret_cast<void*>(method), &info) ||
            info.dli_fbase != header)
            continue;
        const auto distance = method > submitter ? method - submitter : submitter - method;
        if (distance <= 0x2000)
        {
            taskContext = reinterpret_cast<void*>(context);
            return true;
        }
    }
    error = "Studio's DataModel task submitter failed context validation";
    return false;
}

static Response PackageAction(
    const Request& request,
    const std::string& payload,
    const std::string& title)
{
    Response response{Magic, 10, 0, 0, {}};
    std::lock_guard lock(SerializeMutex);
    const auto started = std::chrono::steady_clock::now();
    const mach_header_64* header = nullptr;
    std::intptr_t slide = 0;
    for (std::uint32_t index = 0; index < _dyld_image_count(); ++index)
    {
        const auto candidate = reinterpret_cast<const mach_header_64*>(
            _dyld_get_image_header(index));
        if (candidate && candidate->filetype == MH_EXECUTE)
        {
            header = candidate;
            slide = _dyld_get_image_vmaddr_slide(index);
            break;
        }
    }
    std::string error;
    void* dataModel = nullptr;
    if (!FindDataModel(header, slide, title, dataModel, error))
    {
        response.status = 11;
        SetError(response, error);
        return response;
    }
    std::vector<PackagePathSegment> segments;
    std::int64_t expectedVersion = 0;
    if (!DecodePackagePath(
            payload,
            segments,
            expectedVersion,
            error))
    {
        response.status = 12;
        SetError(response, error);
        return response;
    }
    ResolvedPackage package{};
    if (!ResolvePackage(dataModel, segments, package, error))
    {
        response.status = 13;
        SetError(response, error);
        return response;
    }
    if (request.reserved >= 1 && request.reserved <= 4)
    {
        const auto link = reinterpret_cast<std::uintptr_t>(package.link.instance);
        const auto retained = request.reserved == 2 ? package.root : package.link;
        auto task = new (std::nothrow) PackageStateTask{};
        if (!task)
        {
            response.status = 14;
            SetError(response, "could not allocate the package state task");
            return response;
        }
        if (!ResolvePackageProperties(
                link,
                expectedVersion,
                header,
                task->modified,
                task->hasNewVersion,
                task->version,
                error) ||
            !RetainOwner(retained.owner))
        {
            task->references = 1;
            ReleasePackageStateTask(task);
            response.status = 14;
            if (error.empty())
                error = "PackageLink expired before the package action";
            SetError(response, error);
            return response;
        }
        task->owner = retained.owner;
        const auto timeoutMs = std::clamp<std::uint64_t>(
            request.factoryRva ? request.factoryRva : 5000, 250, 20000);
        task->deadline = started + std::chrono::milliseconds(timeoutMs - 100);
        if (task->deadline - std::chrono::steady_clock::now() <=
            std::chrono::milliseconds(100))
        {
            task->references = 1;
            ReleasePackageStateTask(task);
            response.status = 14;
            SetError(response, "Package action deadline is too short");
            return response;
        }
        if (!ResolveImageFunction(
                 header,
                 slide,
                 request,
                 request.executeRva,
                 "DataModel task submitter",
                 task->submitTask,
                 error) ||
             !ResolveDataModelTaskContext(
                 dataModel,
                 header,
                 task->submitTask,
                 task->taskContext,
                 error))
        {
            task->references = 1;
            ReleasePackageStateTask(task);
            response.status = 14;
            if (error.empty())
                error = "Studio package publish action was not found";
            SetError(response, error);
            return response;
        }
        task->root = package.root;
        task->targetVersion = expectedVersion;
        task->write = request.reserved == 1 || request.reserved == 4;
        task->desiredModifiedValue = request.reserved == 4 ? -1 : 1;
        auto changed = false;
        auto modified = 0;
        auto hasNewVersion = false;
        auto version = expectedVersion;
        if (request.reserved == 2)
        {
            if (!ResolvePackageUiBinding(
                    dataModel,
                    header,
                    "PublishPackage",
                    "EFvNSt3__110shared_ptrINS_8InstanceEEEb",
                    task->packageUiService,
                    task->packageMethod,
                    error))
            {
                ReleasePackageStateTask(task);
                response.status = 14;
                SetError(response, error);
                return response;
            }
            std::int64_t modifiedValue = 0;
            if (!ReadPackageInteger(
                    reinterpret_cast<std::uintptr_t>(task->modified.target) +
                        task->modified.fieldOffset,
                    task->modified.fieldWidth,
                    modifiedValue))
            {
                ReleasePackageStateTask(task);
                response.status = 14;
                SetError(response, "PackageLink changed before publishing");
                return response;
            }
            if (modifiedValue == -1)
            {
                ReleasePackageStateTask(task);
                response.status = 14;
                SetError(
                    response,
                    "Package is Up To Date; publishing requires the Changed state");
                return response;
            }
            if (!RunPackagePublish(task, error))
            {
                ReleasePackageStateTask(task);
                response.status = 14;
                SetError(response, error);
                return response;
            }
        }
        else if (request.reserved == 3)
        {
            if (!ReadPackageState(task, link, modified, hasNewVersion, version))
            {
                ReleasePackageStateTask(task);
                response.status = 14;
                SetError(response, "PackageLink changed before updating");
                return response;
            }
            changed = modified != -1 || hasNewVersion;
            if (changed)
            {
                if (!ResolvePackageUiBinding(
                        dataModel,
                        header,
                        "SetPackageVersion",
                        "EFNSt3__110shared_ptrINS_8InstanceEEES6_x",
                        task->packageUiService,
                        task->packageMethod,
                        error) ||
                    !RunPackageUpdate(task, error))
                {
                    ReleasePackageStateTask(task);
                    response.status = 14;
                    SetError(response, error);
                    return response;
                }
                while (true)
                {
                    ResolvedPackage updated{};
                    if (ResolvePackage(dataModel, segments, updated, error) &&
                        ReadPackageState(
                            task,
                            reinterpret_cast<std::uintptr_t>(updated.link.instance),
                            modified,
                            hasNewVersion,
                            version) &&
                        modified == -1 && !hasNewVersion &&
                        version >= expectedVersion)
                        break;
                    if (std::chrono::steady_clock::now() >= task->deadline)
                    {
                        ReleasePackageStateTask(task);
                        response.status = 14;
                        SetError(response, "Package did not finish updating before the deadline");
                        return response;
                    }
                    std::this_thread::sleep_for(std::chrono::milliseconds(25));
                }
            }
        }
        else
        {
            if (!SubmitPackageTask(
                    task,
                    RunPackageStateTask,
                    "Studio rejected the package DataModel task",
                    "Studio package action timed out",
                    error))
            {
                ReleasePackageStateTask(task);
                response.status = 14;
                SetError(response, error);
                return response;
            }
            changed = task->changed;
            modified = task->modifiedValue;
            hasNewVersion = task->hasNewVersionValue != 0;
            version = task->versionValue;
        }
        if (task->write && modified != task->desiredModifiedValue)
        {
            ReleasePackageStateTask(task);
            response.status = 14;
            SetError(
                response,
                "PackageLink.ModifiedState remained " + std::to_string(modified) +
                    ", expected " + std::to_string(task->desiredModifiedValue));
            return response;
        }
        if (request.reserved == 2)
        {
            while (true)
            {
                std::int64_t modifiedValue = 0;
                std::int64_t hasNewVersionValue = 0;
                std::int64_t versionValue = 0;
                if (!ReadPackageInteger(
                        reinterpret_cast<std::uintptr_t>(task->modified.target) +
                            task->modified.fieldOffset,
                        task->modified.fieldWidth,
                        modifiedValue) ||
                    !ReadPackageInteger(
                        reinterpret_cast<std::uintptr_t>(task->hasNewVersion.target) +
                            task->hasNewVersion.fieldOffset,
                        task->hasNewVersion.fieldWidth,
                        hasNewVersionValue) ||
                    !ReadPackageInteger(
                        reinterpret_cast<std::uintptr_t>(task->version.target) +
                            task->version.fieldOffset,
                        task->version.fieldWidth,
                        versionValue))
                {
                    ReleasePackageStateTask(task);
                    response.status = 14;
                    SetError(response, "PackageLink changed while publishing");
                    return response;
                }
                modified = static_cast<int>(modifiedValue);
                hasNewVersion = hasNewVersionValue != 0;
                version = versionValue;
                if (modified == -1 && version >= expectedVersion)
                {
                    changed = version > expectedVersion;
                    break;
                }
                if (std::chrono::steady_clock::now() >= task->deadline)
                {
                    ReleasePackageStateTask(task);
                    response.status = 14;
                    SetError(response, "Package did not finish publishing before the deadline");
                    return response;
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(25));
            }
        }
        response.outputSize = version > 0 ? static_cast<std::uint64_t>(version) : 0;
        response.elapsedMicros = changed ? 1 : 0;
        if (request.reserved == 1)
            SetError(
                response,
                hasNewVersion ? "Changed + New Version Available" : "Changed");
        else
            SetError(
                response,
                hasNewVersion ? "New Version Available" : "Up To Date");
        ReleasePackageStateTask(task);
    }
    response.status = 0;
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
    const mach_header_64* header = nullptr;
    std::intptr_t slide = 0;
    for (std::uint32_t index = 0; index < _dyld_image_count(); ++index)
    {
        const auto candidate = reinterpret_cast<const mach_header_64*>(
            _dyld_get_image_header(index));
        if (candidate && candidate->filetype == MH_EXECUTE)
        {
            header = candidate;
            slide = _dyld_get_image_vmaddr_slide(index);
            break;
        }
    }
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
    if (!FindDataModel(header, slide, title, dataModel, error))
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

static void HandleClient(int client)
{
    Request request{};
    Response response{Magic, 7, 0, 0, {}};
    if (!ReadExact(client, &request, sizeof(request)) || request.magic != Magic ||
        request.version != Version ||
        (request.command != 1 && request.command != 2) || request.pathLength == 0 ||
        request.pathLength >= PATH_MAX || request.titleLength >= PATH_MAX)
    {
        SetError(response, "invalid serializer request");
        WriteExact(client, &response, sizeof(response));
        return;
    }
    std::string path(request.pathLength, '\0');
    if (!ReadExact(client, path.data(), path.size()))
    {
        SetError(response, "incomplete serializer request payload");
        WriteExact(client, &response, sizeof(response));
        return;
    }
    if (request.command == 1 && path.front() != '/')
    {
        SetError(response, "serializer output path must be absolute");
        WriteExact(client, &response, sizeof(response));
        return;
    }
    std::string title(request.titleLength, '\0');
    if (!title.empty() && !ReadExact(client, title.data(), title.size()))
    {
        SetError(response, "invalid Studio title");
        WriteExact(client, &response, sizeof(response));
        return;
    }
    try
    {
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
    WriteExact(client, &response, sizeof(response));
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
        const int noSignal = 1;
        setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, sizeof(noSignal));
        HandleClient(client);
        close(client);
    }
    close(server);
    unlink(SocketPath);
    return nullptr;
}

__attribute__((constructor)) static void StartReniumHelper()
{
    unsetenv("DYLD_INSERT_LIBRARIES");
    pthread_t thread;
    if (pthread_create(&thread, nullptr, RunServer, nullptr) == 0)
        pthread_detach(thread);
}

__attribute__((destructor)) static void StopReniumHelper()
{
    if (SocketPath[0])
        unlink(SocketPath);
}
