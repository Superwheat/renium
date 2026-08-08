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
    std::uint64_t factoryRva;
    std::uint64_t executeRva;
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
    std::string name;
};

static constexpr std::uint32_t Magic = 0x4d4e4552;
static constexpr std::uint32_t Version = 1;
static constexpr std::size_t DataModelInstanceOffset = 0x1c8;
static constexpr std::size_t InstanceClassDescriptorOffset = 0x18;
static constexpr std::size_t InstanceChildrenOffset = 0x70;
static constexpr std::size_t InstanceNameOffset = 0x98;
static std::mutex SerializeMutex;
static char SocketPath[sizeof(((sockaddr_un*)nullptr)->sun_path)]{};
static void* CachedDataModel = nullptr;

static_assert(sizeof(Request) == 32);
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
    if (size > 1024 * 1024 || (size && data < 0x10000))
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

static bool ReadInstanceName(std::uintptr_t instance, std::string& value)
{
    std::uintptr_t name = 0;
    if (ReadValue(instance + InstanceNameOffset, name) && name &&
        ReadLibcppString(name, value))
        return true;
    return ReadLibcppString(instance + InstanceNameOffset, value);
}

static bool IsDataModel(std::uintptr_t outer)
{
    const auto instance = outer + DataModelInstanceOffset;
    std::uintptr_t self = 0;
    std::uintptr_t vtable = 0;
    std::uintptr_t typeInfo = 0;
    std::uintptr_t typeName = 0;
    std::string name;
    return ReadValue(instance + 8, self) && self == instance &&
        ReadValue(instance, vtable) && vtable &&
        ReadValue(vtable - sizeof(void*), typeInfo) && typeInfo &&
        ReadValue(typeInfo + sizeof(void*), typeName) && typeName &&
        ReadCString(typeName, name, 128) && name == "N3RBX9DataModelE";
}

static bool ReadChildren(std::uintptr_t instance, std::vector<SharedInstance>& children)
{
    std::uintptr_t vector = 0;
    std::uintptr_t begin = 0;
    std::uintptr_t end = 0;
    std::uintptr_t capacity = 0;
    if (!ReadValue(instance + InstanceChildrenOffset, vector) || !vector)
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

static bool HasRequiredRoots(std::uintptr_t outer)
{
    std::vector<SharedInstance> children;
    if (!ReadChildren(outer + DataModelInstanceOffset, children))
        return false;
    bool workspace = false;
    bool players = false;
    bool materialService = false;
    bool studioData = false;
    for (const auto& child : children)
    {
        std::string name;
        if (!child.instance ||
            !ReadInstanceClass(reinterpret_cast<std::uintptr_t>(child.instance), name))
            continue;
        workspace = workspace || name == "Workspace";
        players = players || name == "Players";
        materialService = materialService || name == "MaterialService";
        studioData = studioData || name == "StudioData";
    }
    return workspace && players && materialService && studioData;
}

static void AddCandidates(
    const mach_header_64* header,
    std::intptr_t slide,
    std::vector<DataModelCandidate>& candidates)
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
                        if (outer < 0x10000 || !seen.insert(outer).second ||
                            !IsDataModel(outer) || !HasRequiredRoots(outer))
                            continue;
                        std::string name;
                        ReadInstanceName(outer + DataModelInstanceOffset, name);
                        candidates.push_back({reinterpret_cast<void*>(outer), std::move(name)});
                    }
                }
            }
        }
        if (load->cmdsize < sizeof(load_command))
            break;
        command += load->cmdsize;
    }
}

static bool FindDataModel(void*& output, std::string& error)
{
    if (CachedDataModel)
    {
        const auto cached = reinterpret_cast<std::uintptr_t>(CachedDataModel);
        if (IsDataModel(cached) && HasRequiredRoots(cached))
        {
            output = CachedDataModel;
            return true;
        }
        CachedDataModel = nullptr;
    }
    const auto header = reinterpret_cast<const mach_header_64*>(_dyld_get_image_header(0));
    if (!header || header->magic != MH_MAGIC_64)
    {
        error = "Studio main image is not a 64-bit Mach-O";
        return false;
    }
    std::vector<DataModelCandidate> candidates;
    AddCandidates(header, _dyld_get_image_vmaddr_slide(0), candidates);
    if (candidates.size() != 1)
    {
        error = "active Studio DataModel selection returned " +
            std::to_string(candidates.size()) + " candidates";
        return false;
    }
    output = candidates[0].outer;
    CachedDataModel = output;
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

static bool ValidateRva(
    const mach_header_64* header,
    std::intptr_t slide,
    std::uint64_t rva,
    std::uintptr_t& address)
{
    auto command = reinterpret_cast<const unsigned char*>(header) + sizeof(*header);
    for (std::uint32_t index = 0; index < header->ncmds; ++index)
    {
        const auto load = reinterpret_cast<const load_command*>(command);
        if (load->cmd == LC_SEGMENT_64)
        {
            const auto segment = reinterpret_cast<const segment_command_64*>(load);
            if (std::strcmp(segment->segname, "__TEXT") == 0 &&
                rva >= segment->vmaddr - 0x100000000ULL &&
                rva < segment->vmaddr - 0x100000000ULL + segment->vmsize)
            {
                address = static_cast<std::uintptr_t>(0x100000000ULL + rva + slide);
                return true;
            }
        }
        if (load->cmdsize < sizeof(load_command))
            return false;
        command += load->cmdsize;
    }
    return false;
}

static void SetError(Response& response, const std::string& error)
{
    std::snprintf(response.error, sizeof(response.error), "%s", error.c_str());
}

static Response Serialize(const Request& request, const std::string& path)
{
    Response response{Magic, 1, 0, 0, {}};
    std::lock_guard lock(SerializeMutex);
    const auto started = std::chrono::steady_clock::now();
    const auto header = reinterpret_cast<const mach_header_64*>(_dyld_get_image_header(0));
    const auto slide = _dyld_get_image_vmaddr_slide(0);
    std::uintptr_t factoryAddress = 0;
    std::uintptr_t executeAddress = 0;
    if (!header || !ValidateRva(header, slide, request.factoryRva, factoryAddress) ||
        !ValidateRva(header, slide, request.executeRva, executeAddress))
    {
        response.status = 2;
        SetError(response, "serializer trace points outside Studio's text segment");
        return response;
    }
    void* dataModel = nullptr;
    std::string error;
    if (!FindDataModel(dataModel, error))
    {
        response.status = 3;
        SetError(response, error);
        return response;
    }
    using FromUtf8 = StudioQString (*)(const char*, int);
    using Factory =
        StudioShared (*)(void*, const StudioQString*, void**, const bool*, const bool*);
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
    const bool direct = false;
    const bool secondary = false;
    auto state = reinterpret_cast<Factory>(factoryAddress)(
        nullptr,
        &output,
        &dataModel,
        &direct,
        &secondary);
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
        request.pathLength >= PATH_MAX)
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
    try
    {
        response = Serialize(request, path);
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
