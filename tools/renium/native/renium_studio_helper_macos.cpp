#define _DARWIN_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <dlfcn.h>
#include <exception>
#include <limits.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <mutex>
#include <pthread.h>
#include <string>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
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
static constexpr std::uint32_t Version = 3;
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
static std::string CachedDataModelTitle;

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

static bool ReadInstanceClass(std::uintptr_t instance, std::string& value)
{
    std::uintptr_t descriptor = 0;
    if (!ReadValue(instance + InstanceClassDescriptorOffset, descriptor) || !descriptor)
        return false;
    std::uintptr_t name = 0;
    if (ReadValue(descriptor + 8, name) && name &&
        ReadLibcppString(name, value) && !value.empty())
        return true;
    return ReadLibcppString(descriptor + 8, value) && !value.empty();
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
    for (std::size_t offset = InstanceNameOffsetMin;
         offset <= InstanceNameOffsetMax;
         offset += sizeof(void*))
    {
        std::uintptr_t indirect = 0;
        if (ReadValue(instance + offset, indirect) && indirect &&
            ReadLibcppString(indirect, value) &&
            std::find(expectedNames.begin(), expectedNames.end(), value) != expectedNames.end())
            return true;
        if (ReadLibcppString(instance + offset, value) &&
            std::find(expectedNames.begin(), expectedNames.end(), value) != expectedNames.end())
            return true;
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
        ReadCString(typeName, name, 128);
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
        request.version != Version || request.command != 1 || request.pathLength == 0 ||
        request.pathLength >= PATH_MAX || request.titleLength >= PATH_MAX)
    {
        SetError(response, "invalid serializer request");
        WriteExact(client, &response, sizeof(response));
        return;
    }
    std::string path(request.pathLength, '\0');
    if (!ReadExact(client, path.data(), path.size()) || path.front() != '/')
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
        response = Serialize(request, path, title);
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
