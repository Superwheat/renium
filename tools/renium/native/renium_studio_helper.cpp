#include <Windows.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <new>
#include <ostream>
#include <sstream>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>
#include "renium_studio_history.h"
#include "renium_studio_terrain.h"

#if defined(_M_IX86) || defined(_M_X64)
#include <immintrin.h>
#endif

struct SharedInstance
{
    void* instance;
    void* owner;
};

struct ReniumSerializerParams
{
    std::uint64_t moduleBase;
    std::uint64_t serializerRva;
    std::uint64_t contextBuilderRva;
    std::uint64_t contextDestroyRva;
    std::uint64_t rootCollectorRva;
    std::uint64_t deallocatorRva;
    std::uint64_t dataModel;
    std::uint64_t dataModelOwner;
    std::uint32_t count;
    std::uint32_t status;
    std::uint64_t outputSize;
    std::uint64_t contextMicros;
    std::uint64_t collectMicros;
    std::uint64_t serializeMicros;
    std::uint64_t writeMicros;
    std::uint64_t collectedCount;
    std::uint64_t collectedCapacityBytes;
    std::uint32_t requestedMxcsr;
    std::uint32_t initialMxcsr;
    std::uint32_t placeMode;
    std::uint32_t dataModelInstanceOffset;
    SharedInstance roots[256];
    wchar_t outputPath[520];
    char error[512];
    std::uint64_t taskContext;
    std::uint64_t submitTask;
    std::uint32_t timeoutMs;
    std::uint32_t childrenOffset;
    std::uint32_t selfOffset;
    std::uint32_t reserved;
    std::uint64_t deadlineTick;
    std::uint64_t queueMicros;
    std::uint64_t identityBinding;
    std::uint64_t identityGetter;
    std::uint64_t debugIdGetter;
    std::uint64_t identitySize;
    std::uint64_t identityMicros;
    std::uint32_t captureMode;
    std::uint32_t parentOffset;
    std::uint64_t window;
    std::uint32_t processId;
    std::uint32_t captureReserved;
};

static_assert(sizeof(ReniumSerializerParams) == 5904);
static_assert(offsetof(ReniumSerializerParams, status) == 68);
static_assert(offsetof(ReniumSerializerParams, placeMode) == 136);
static_assert(offsetof(ReniumSerializerParams, dataModelInstanceOffset) == 140);
static_assert(offsetof(ReniumSerializerParams, roots) == 144);
static_assert(offsetof(ReniumSerializerParams, outputPath) == 4240);
static_assert(offsetof(ReniumSerializerParams, error) == 5280);
static_assert(offsetof(ReniumSerializerParams, taskContext) == 5792);
static_assert(offsetof(ReniumSerializerParams, queueMicros) == 5832);
static_assert(offsetof(ReniumSerializerParams, identityBinding) == 5840);
static_assert(offsetof(ReniumSerializerParams, captureMode) == 5880);
static_assert(offsetof(ReniumSerializerParams, processId) == 5896);

// RCAP v1. Transport contains this header, RBXL, then headerless identity rows.
struct CaptureResponse
{
    std::uint32_t magic, version, status, exitCode;
    std::uint64_t bytes, identities, queueMicros, serializeMicros, identityMicros, writeMicros;
    char error[256];
};
static_assert(sizeof(CaptureResponse) == 320);

struct CaptureIdentityRow
{
    unsigned char id[16]; // Native UniqueId getter's four LE words, unchanged.
    char debugId[48];     // GetDebugId(32), NUL terminated and zero padded.
    std::uint32_t parent; // Earlier row index; UINT32_MAX for selected roots.
    std::uint32_t reserved;
};
static_assert(sizeof(CaptureIdentityRow) == 72);
static constexpr std::size_t CaptureMaxRows = 2000000;
static constexpr std::size_t CaptureMaxBytes = 512ull * 1024 * 1024;

// MSVC release basic_string returned by the engine. Never destroy its heap
// storage using this /MT helper's allocator.
struct EngineString
{
    union { char inlineBytes[16]; char* heap; } storage;
    std::size_t size;
    std::size_t capacity;
};
static_assert(sizeof(EngineString) == 32);

using ContextBuilder = void*(__fastcall*)(void*, void*);
using ContextDestroy = void(__fastcall*)(void*);
using RootCollector = void(__fastcall*)(void*, void*);
using Deallocator = void(__fastcall*)(void*, std::size_t);
using Serializer = void*(__fastcall*)(
    std::ostream*,
    void*,
    void*,
    void*,
    int,
    void*,
    void*,
    void*,
    void*,
    void*,
    void*);

struct RunState
{
    ReniumSerializerParams* params;
    ContextDestroy destroy;
    Deallocator deallocate;
    std::uint32_t ownerCount;
    void* owners[257];
    alignas(16) unsigned char context[0x100];
    alignas(16) unsigned char collectedRoots[0x40];
    std::ostringstream* stream;
    std::string* bytes;
    bool contextBuilt;
    bool rootsCollected;
    bool streamBuilt;
    bool bytesBuilt;
    bool fileCreated;
    HANDLE file;
    const std::atomic<bool>* cancelled;
    std::vector<void*>* pending;
    std::vector<CaptureIdentityRow>* identities;
    EngineString debugString;
    bool debugStringBuilt;
};

struct SharedVector
{
    SharedInstance* begin;
    SharedInstance* end;
    SharedInstance* capacity;
};

static void SetError(ReniumSerializerParams* params, const char* message)
{
    strncpy_s(params->error, message, _TRUNCATE);
}

static std::uint32_t GetMxcsr()
{
#if defined(_M_IX86) || defined(_M_X64)
    return _mm_getcsr();
#else
    return 0;
#endif
}

static void SetMxcsr(std::uint32_t value)
{
#if defined(_M_IX86) || defined(_M_X64)
    _mm_setcsr(value);
#else
    (void)value;
#endif
}

static bool AddOwnerReference(void* owner)
{
    if (!owner)
        return true;
    auto uses = reinterpret_cast<volatile long*>(
        reinterpret_cast<unsigned char*>(owner) + 8);
    auto current = *uses;
    while (current > 0)
    {
        const auto previous = InterlockedCompareExchange(uses, current + 1, current);
        if (previous == current)
            return true;
        current = previous;
    }
    return false;
}

static void ReleaseOwnerReference(void* owner)
{
    if (!owner)
        return;
    auto bytes = reinterpret_cast<unsigned char*>(owner);
    if (InterlockedDecrement(reinterpret_cast<volatile long*>(bytes + 8)) != 0)
        return;
    auto vtable = *reinterpret_cast<void***>(owner);
    reinterpret_cast<void(__fastcall*)(void*)>(vtable[0])(owner);
    if (InterlockedDecrement(reinterpret_cast<volatile long*>(bytes + 12)) == 0)
        reinterpret_cast<void(__fastcall*)(void*)>(vtable[1])(owner);
}

static std::uint64_t ElapsedMicros(
    const LARGE_INTEGER& start,
    const LARGE_INTEGER& finish,
    const LARGE_INTEGER& frequency)
{
    return static_cast<std::uint64_t>(
        (finish.QuadPart - start.QuadPart) * 1000000 / frequency.QuadPart);
}

static void CheckSerializerDeadline(const RunState* state)
{
    if (state->cancelled->load(std::memory_order_acquire) ||
        GetTickCount64() >= state->params->deadlineTick)
        throw std::runtime_error("Native snapshot cancelled or exceeded its deadline");
}

static void CheckCaptureWindow(const ReniumSerializerParams* params)
{
    DWORD pid = 0;
    const auto window = reinterpret_cast<HWND>(params->window);
    // Title is checked from the host before and after capture. GetWindowTextW
    // inside the owning process can dispatch WM_GETTEXT; never do that while
    // holding the DataModel lock.
    if (params->processId != GetCurrentProcessId() ||
        !GetWindowThreadProcessId(window, &pid) || pid != params->processId)
        throw std::runtime_error("Studio process or window identity changed before capture");
}

static std::size_t CaptureChildCount(const SharedVector* children)
{
    if (!children) return 0;
    const auto begin = reinterpret_cast<std::uintptr_t>(children->begin);
    const auto end = reinterpret_cast<std::uintptr_t>(children->end);
    const auto capacity = reinterpret_cast<std::uintptr_t>(children->capacity);
    if ((!begin && (end || capacity)) || begin % alignof(SharedInstance) ||
        end < begin || capacity < end || (end - begin) % sizeof(SharedInstance) ||
        (capacity - begin) % sizeof(SharedInstance) ||
        (capacity - begin) / sizeof(SharedInstance) > CaptureMaxRows)
        throw std::runtime_error("Invalid native capture children vector");
    return (end - begin) / sizeof(SharedInstance);
}

static void DestroyDebugString(RunState* state)
{
    if (!state->debugStringBuilt) return;
    state->debugStringBuilt = false; // Never retry if the engine free itself faults.
    auto& text = state->debugString;
    if (text.capacity >= 16)
    {
        auto allocation = static_cast<void*>(text.storage.heap);
        auto size = text.capacity + 1;
        if (size >= 0x1000)
        {
            allocation = reinterpret_cast<void**>(allocation)[-1];
            size += 0x27;
        }
        state->deallocate(allocation, size);
    }
}

static void CaptureIdentities(RunState* state)
{
    const auto params = state->params;
    state->pending = new std::vector<void*>;
    state->identities = new std::vector<CaptureIdentityRow>;
    auto& pending = *state->pending;
    auto& rows = *state->identities;
    pending.reserve(params->count);
    rows.reserve(params->count);
    for (std::uint32_t index = 0; index < params->count; ++index)
    {
        auto root = params->roots[index].instance;
        const auto self = reinterpret_cast<const SharedInstance*>(
            static_cast<unsigned char*>(root) + params->selfOffset);
        if (self->instance != root || self->owner != params->roots[index].owner)
            throw std::runtime_error("Selected service self identity changed before capture");
        if (*reinterpret_cast<void**>(static_cast<unsigned char*>(root) + params->parentOffset) !=
            reinterpret_cast<void*>(params->dataModel + params->dataModelInstanceOffset))
            throw std::runtime_error("Selected service parent changed before capture");
        pending.push_back(root);
        rows.push_back({{}, {}, UINT32_MAX, 0});
    }
    for (std::size_t index = 0; index < pending.size(); ++index)
    {
        CheckSerializerDeadline(state);
        auto instance = static_cast<unsigned char*>(pending[index]);
        using IdentityGetter = void*(__fastcall*)(void*, void*, void*);
        const auto identity = reinterpret_cast<IdentityGetter>(params->identityGetter)(
            reinterpret_cast<void*>(params->identityBinding), rows[index].id, instance);
        if (identity != rows[index].id)
            throw std::runtime_error("Native UniqueId return-buffer ABI changed");
        using DebugIdGetter = void*(__fastcall*)(void*, void*, std::int32_t);
        const auto textResult = reinterpret_cast<DebugIdGetter>(params->debugIdGetter)(
            instance, &state->debugString, 32);
        state->debugStringBuilt = true;
        const auto& text = state->debugString;
        if (textResult != &text || !text.size || text.size >= sizeof(rows[index].debugId) ||
            text.size > text.capacity)
            throw std::runtime_error("Native GetDebugId returned an invalid string");
        const auto data = text.capacity < 16 ? text.storage.inlineBytes : text.storage.heap;
        if (memchr(data, '\0', text.size) || data[text.size] != '\0')
            throw std::runtime_error("Native GetDebugId returned invalid text");
        memcpy(rows[index].debugId, data, text.size);
        DestroyDebugString(state);

        const auto children = *reinterpret_cast<const SharedVector* const*>(instance + params->childrenOffset);
        const auto count = CaptureChildCount(children);
        if (count > CaptureMaxRows - pending.size())
            throw std::runtime_error("Native capture exceeds its instance bound");
        for (std::size_t childIndex = 0; childIndex < count; ++childIndex)
        {
            const auto& child = children->begin[childIndex];
            if (!child.instance || !child.owner)
                throw std::runtime_error("Native capture child has no ownership");
            auto childBytes = static_cast<unsigned char*>(child.instance);
            const auto self = reinterpret_cast<const SharedInstance*>(childBytes + params->selfOffset);
            if (self->instance != child.instance || self->owner != child.owner ||
                *reinterpret_cast<void* const*>(childBytes + params->parentOffset) != instance)
                throw std::runtime_error("Native capture child identity or parent changed");
            pending.push_back(child.instance);
            rows.push_back({{}, {}, static_cast<std::uint32_t>(index), 0});
        }
    }
    params->identitySize = rows.size() * sizeof(CaptureIdentityRow);
}

static bool WriteSnapshotBytes(RunState* state, const void* bytes, std::size_t size)
{
    auto data = static_cast<const unsigned char*>(bytes);
    while (size)
    {
        CheckSerializerDeadline(state);
        const auto chunk = static_cast<DWORD>(size > 1024 * 1024 ? 1024 * 1024 : size);
        DWORD written = 0;
        if (!WriteFile(state->file, data, chunk, &written, nullptr) || written != chunk)
            return false;
        data += written;
        size -= written;
    }
    return true;
}

static DWORD RunCore(RunState* state)
{
    auto params = state->params;
    CheckSerializerDeadline(state);
    if (params->captureMode) CheckCaptureWindow(params);
    auto builder = reinterpret_cast<ContextBuilder>(
        params->moduleBase + params->contextBuilderRva);
    auto collectRoots = reinterpret_cast<RootCollector>(
        params->moduleBase + params->rootCollectorRva);
    auto serializer = reinterpret_cast<Serializer>(
        params->moduleBase + params->serializerRva);
    state->destroy = reinterpret_cast<ContextDestroy>(
        params->moduleBase + params->contextDestroyRva);
    state->deallocate = reinterpret_cast<Deallocator>(
        params->moduleBase + params->deallocatorRva);

    // Recheck captured roots under the DataModel lock before touching their
    // owners. A service may have been removed after the host's discovery.
    const auto model = params->dataModel + params->dataModelInstanceOffset;
    const auto identity = reinterpret_cast<const SharedInstance*>(model + params->selfOffset);
    if (identity->instance != reinterpret_cast<void*>(model) ||
        identity->owner != reinterpret_cast<void*>(params->dataModelOwner))
        throw std::runtime_error("DataModel identity changed before native snapshot");
    if (params->captureMode &&
        (*reinterpret_cast<const std::uintptr_t*>(model + 0x58) & ~std::uintptr_t(7)) != params->taskContext)
        throw std::runtime_error("DataModel task context changed before capture");
    const auto children = *reinterpret_cast<const SharedVector* const*>(model + params->childrenOffset);
    if (!children || !children->begin || children->end < children->begin ||
        children->end - children->begin > 256)
        throw std::runtime_error("DataModel root layout changed before native snapshot");
    for (std::uint32_t index = 0; index < params->count; ++index)
    {
        const auto& expected = params->roots[index];
        bool present = false;
        for (auto child = children->begin; child != children->end; ++child)
            if (child->instance == expected.instance && child->owner == expected.owner)
            {
                present = true;
                break;
            }
        if (!present) throw std::runtime_error("Service identity changed before native snapshot");
    }

    if (params->dataModelOwner)
    {
        auto owner = reinterpret_cast<void*>(params->dataModelOwner);
        if (!AddOwnerReference(owner))
        {
            params->status = 0xE006;
            SetError(params, "DataModel owner expired");
            return params->status;
        }
        state->owners[state->ownerCount++] = owner;
    }

    for (std::uint32_t index = 0; index < params->count; ++index)
    {
        auto owner = params->roots[index].owner;
        if (!AddOwnerReference(owner))
        {
            params->status = 0xE007;
            SetError(params, "root owner expired");
            return params->status;
        }
        if (owner)
            state->owners[state->ownerCount++] = owner;
    }

    if (params->requestedMxcsr)
        SetMxcsr(params->requestedMxcsr);

    LARGE_INTEGER frequency{};
    LARGE_INTEGER started{};
    LARGE_INTEGER finished{};
    QueryPerformanceFrequency(&frequency);

    SharedVector roots{
        params->roots,
        params->roots + params->count,
        params->roots + params->count,
    };
    if (!params->placeMode)
    {
        QueryPerformanceCounter(&started);
        builder(state->context, reinterpret_cast<void*>(params->dataModel));
        state->contextBuilt = true;
        QueryPerformanceCounter(&finished);
        params->contextMicros = ElapsedMicros(started, finished, frequency);

        QueryPerformanceCounter(&started);
        collectRoots(state->collectedRoots, &roots);
        state->rootsCollected = true;
        QueryPerformanceCounter(&finished);
        params->collectMicros = ElapsedMicros(started, finished, frequency);

        auto collected = reinterpret_cast<void**>(state->collectedRoots);
        params->collectedCount =
            (reinterpret_cast<std::uintptr_t>(collected[1]) -
             reinterpret_cast<std::uintptr_t>(collected[0])) /
            sizeof(void*);
        params->collectedCapacityBytes =
            reinterpret_cast<std::uintptr_t>(collected[2]) -
            reinterpret_cast<std::uintptr_t>(collected[0]);
    }

    params->status = 2;
    state->stream = new std::ostringstream(std::ios::binary | std::ios::out);
    state->streamBuilt = true;
    QueryPerformanceCounter(&started);
    if (params->placeMode)
    {
        // Rust already validated this build's layout and captured these roots.
        auto instance = reinterpret_cast<unsigned char*>(params->dataModel) +
            params->dataModelInstanceOffset;
        serializer(
            static_cast<std::ostream*>(state->stream),
            instance,
            &roots,
            nullptr,
            0x40,
            nullptr,
            nullptr,
            nullptr,
            nullptr,
            nullptr,
            nullptr);
    }
    else
    {
        serializer(
            static_cast<std::ostream*>(state->stream),
            nullptr,
            &roots,
            state->context,
            0,
            state->collectedRoots,
            nullptr,
            nullptr,
            nullptr,
            nullptr,
            nullptr);
    }
    QueryPerformanceCounter(&finished);
    params->serializeMicros = ElapsedMicros(started, finished, frequency);
    CheckSerializerDeadline(state);
    if (params->captureMode)
    {
        const auto size = state->stream->view().size();
        if (size < 32 || size > CaptureMaxBytes)
            throw std::runtime_error("Native capture exceeds its byte bound");
        QueryPerformanceCounter(&started);
        CaptureIdentities(state);
        QueryPerformanceCounter(&finished);
        params->identityMicros = ElapsedMicros(started, finished, frequency);
        CheckCaptureWindow(params);
    }
    params->status = 3;

    if (!params->captureMode)
    {
        state->bytes = new std::string(state->stream->str());
        state->bytesBuilt = true;
    }
    const auto payload = params->captureMode ? state->stream->view() : std::string_view(*state->bytes);
    params->outputSize = payload.size();
    CheckSerializerDeadline(state);
    state->file = CreateFileW(
        params->outputPath,
        GENERIC_WRITE,
        params->captureMode ? FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE : 0,
        nullptr,
        params->captureMode ? OPEN_EXISTING : CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL,
        nullptr);
    if (state->file == INVALID_HANDLE_VALUE)
    {
        state->file = nullptr;
        params->status = 0xE002;
        SetError(params, "CreateFileW failed");
        return params->status;
    }
    state->fileCreated = !params->captureMode;
    if (params->captureMode)
    {
        LARGE_INTEGER start{};
        start.QuadPart = sizeof(CaptureResponse);
        if (!SetFilePointerEx(state->file, start, nullptr, FILE_BEGIN))
            throw std::runtime_error("Cannot position native capture transport");
    }

    QueryPerformanceCounter(&started);
    bool wrote = WriteSnapshotBytes(state, payload.data(), payload.size());
    if (wrote && params->captureMode)
        wrote = WriteSnapshotBytes(state, state->identities->data(), params->identitySize);
    if (wrote && !params->captureMode)
        wrote = FlushFileBuffers(state->file) != FALSE;
    CloseHandle(state->file);
    state->file = nullptr;
    QueryPerformanceCounter(&finished);
    params->writeMicros = ElapsedMicros(started, finished, frequency);
    if (!wrote)
    {
        if (state->fileCreated) DeleteFileW(params->outputPath);
        params->status = 0xE003;
        SetError(params, "WriteFile failed");
        return params->status;
    }

    CheckSerializerDeadline(state);
    params->status = 4;
    return 0;
}

static void DestroyDebugStringCaught(RunState* state);

static void CleanupCore(RunState* state)
{
    if (state->file) { CloseHandle(state->file); state->file = nullptr; }
    DestroyDebugStringCaught(state);
    delete state->identities;
    state->identities = nullptr;
    delete state->pending;
    state->pending = nullptr;
    if (state->bytesBuilt)
    {
        delete state->bytes;
        state->bytes = nullptr;
        state->bytesBuilt = false;
    }
    if (state->streamBuilt)
    {
        delete state->stream;
        state->stream = nullptr;
        state->streamBuilt = false;
    }
    if (state->rootsCollected)
    {
        auto collected = reinterpret_cast<void**>(state->collectedRoots);
        if (collected[0])
        {
            auto allocation = collected[0];
            auto allocationSize = static_cast<std::size_t>(
                state->params->collectedCapacityBytes);
            if (allocationSize >= 0x1000)
            {
                allocationSize += 0x27;
                allocation = reinterpret_cast<void**>(allocation)[-1];
            }
            state->deallocate(allocation, allocationSize);
        }
        state->rootsCollected = false;
    }
    if (state->contextBuilt)
    {
        state->destroy(state->context);
        state->contextBuilt = false;
    }
    while (state->ownerCount)
        ReleaseOwnerReference(state->owners[--state->ownerCount]);
}

static int RecordException(
    ReniumSerializerParams* params,
    EXCEPTION_POINTERS* exception)
{
    const auto code = exception->ExceptionRecord->ExceptionCode;
    if (code == 0xE06D7363)
        return EXCEPTION_CONTINUE_SEARCH;
    const auto address = exception->ExceptionRecord->ExceptionAddress;
    params->status = 0xE100 | (code & 0xFF);
    sprintf_s(
        params->error,
        "structured exception 0x%08X at %p",
        static_cast<unsigned>(code),
        address);
    return EXCEPTION_EXECUTE_HANDLER;
}

static void DestroyDebugStringCaught(RunState* state)
{
    __try { DestroyDebugString(state); }
    __except (RecordException(state->params, GetExceptionInformation()))
    {
        state->debugStringBuilt = false; // Do not retry a faulting engine free.
    }
}

static DWORD RunCaught(RunState* state)
{
    DWORD result = 0;
    __try
    {
        result = RunCore(state);
    }
    __except (RecordException(state->params, GetExceptionInformation()))
    {
        result = state->params->status;
    }
    return result;
}

static DWORD RunCppCaught(RunState* state)
{
    try
    {
        return RunCaught(state);
    }
    catch (const std::exception& exception)
    {
        state->params->status = 0xE004;
        SetError(state->params, exception.what());
        return state->params->status;
    }
    catch (...)
    {
        state->params->status = 0xE005;
        SetError(state->params, "unknown C++ exception");
        return state->params->status;
    }
}

static void CleanupCaught(RunState* state)
{
    __try
    {
        CleanupCore(state);
    }
    __except (RecordException(state->params, GetExceptionInformation()))
    {
    }
}

static DWORD RunSerializer(ReniumSerializerParams* params, const std::atomic<bool>* cancelled)
{
    if (!params)
        return 0xE000;
    params->status = 1;
    params->outputSize = 0;
    params->contextMicros = 0;
    params->collectMicros = 0;
    params->serializeMicros = 0;
    params->writeMicros = 0;
    params->collectedCount = 0;
    params->collectedCapacityBytes = 0;
    params->identitySize = 0;
    params->identityMicros = 0;
    params->initialMxcsr = GetMxcsr();
    params->error[0] = '\0';
    if (!params->moduleBase ||
        !params->serializerRva ||
        !params->contextBuilderRva ||
        !params->contextDestroyRva ||
        !params->rootCollectorRva ||
        !params->deallocatorRva ||
        !params->dataModel ||
        params->dataModelInstanceOffset > 0x800 ||
        params->dataModelInstanceOffset % 8 != 0 ||
        params->childrenOffset > 0x200 || params->childrenOffset % 8 != 0 ||
        params->selfOffset > 0x80 || params->selfOffset % 8 != 0 ||
        !params->count ||
        params->count > 256 ||
        !params->outputPath[0] || params->outputPath[519] || params->captureMode > 1 ||
        (params->captureMode && (!params->placeMode || !params->identityBinding ||
            !params->identityGetter || !params->debugIdGetter || !params->window ||
            !params->processId || params->captureReserved ||
            params->parentOffset > 0x200 || params->parentOffset % 8)))
    {
        params->status = 0xE001;
        SetError(params, "invalid parameters");
        return params->status;
    }

    RunState state{};
    state.params = params;
    state.cancelled = cancelled;
    const auto result = RunCppCaught(&state);
    CleanupCaught(&state);
    SetMxcsr(params->initialMxcsr);
    if ((result || params->status != 4) && state.fileCreated) DeleteFileW(params->outputPath);
    return result ? result : (params->status == 4 ? 0 : params->status);
}

struct PackageActionParams
{
    std::uint64_t taskContext;
    std::uint64_t target;
    std::uint64_t owner;
    std::uint64_t window;
    std::uint64_t action;
    std::uint32_t timeoutMs;
    std::uint32_t status;
    std::uint32_t exceptionCode;
    std::uint32_t enabled;
    std::uint32_t uiThreadId;
    std::uint32_t found;
    char actionName[96];
    char error[256];
    std::uint64_t dataModel;
    std::uint64_t dataModelOwner;
    std::uint64_t moduleBase;
    std::uint64_t value;
    std::uint64_t operationRva;
    std::uint64_t submitTaskRva;
    std::uint64_t deadlineTick;
    std::uint64_t actionDeadlineTick;
    std::uint64_t reservedRva;
    std::uint32_t mode;
    std::uint32_t resultValid;
    unsigned char result[128];
    unsigned char reserved[64];
};

static_assert(sizeof(PackageActionParams) == 688);

using SubmitDataModelTask = bool(__fastcall*)(
    void*,
    std::function<void()>*,
    std::uint32_t);
using SetPackageModifiedState = void(__fastcall*)(void*, std::uint32_t);
using PublishPackage = void(__fastcall*)(
    void*,
    const SharedInstance*,
    bool,
    const std::function<void()>*,
    const std::function<void(std::string)>*);
using SetPackageVersion = void(__fastcall*)(
    void*,
    const std::shared_ptr<void>*,
    std::int64_t,
    const std::function<void(std::shared_ptr<void>)>*,
    const std::function<void(std::string)>*);

static void SetPackageActionError(PackageActionParams* params, const char* message)
{
    strncpy_s(params->error, message, _TRUNCATE);
}

static DWORD RemainingMilliseconds(std::uint64_t deadline)
{
    const auto now = GetTickCount64();
    if (now >= deadline)
        return 0;
    const auto remaining = deadline - now;
    return remaining > MAXDWORD ? MAXDWORD : static_cast<DWORD>(remaining);
}

struct SerializerTask
{
    ReniumSerializerParams result{};
    HANDLE completed = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    bool delivered = false;
    std::atomic<bool> cancelled{false};
    ~SerializerTask()
    {
        if (!delivered && result.status == 4 && !result.captureMode) DeleteFileW(result.outputPath);
        if (completed) CloseHandle(completed);
    }
};

static DWORD QueueSerializer(ReniumSerializerParams* params)
{
    if (!params || !params->taskContext || !params->submitTask || !params->dataModelOwner ||
        !params->timeoutMs || params->timeoutMs > 15000)
        return 0xE008;
    auto owner = reinterpret_cast<void*>(params->dataModelOwner);
    if (!AddOwnerReference(owner)) throw std::runtime_error("DataModel owner expired");
    // Hold through submission/wait, not in the DataModel's own queue.
    auto modelHold = std::shared_ptr<void>(owner, ReleaseOwnerReference);
    auto task = std::make_shared<SerializerTask>();
    task->result = *params;
    task->result.status = 1;
    task->result.deadlineTick = GetTickCount64() + params->timeoutMs;
    if (!task->completed) throw std::runtime_error("Cannot create native snapshot completion event");
    LARGE_INTEGER queued{}, frequency{};
    QueryPerformanceCounter(&queued);
    QueryPerformanceFrequency(&frequency);
    std::function<void()> work{[task, queued, frequency]() {
        LARGE_INTEGER started{};
        QueryPerformanceCounter(&started);
        task->result.queueMicros = ElapsedMicros(queued, started, frequency);
        if (!RemainingMilliseconds(task->result.deadlineTick))
        {
            task->result.status = 0xE009;
            SetError(&task->result, "Native snapshot expired before Studio could run it");
        }
        else RunSerializer(&task->result, &task->cancelled);
        SetEvent(task->completed);
    }};
    auto submit = reinterpret_cast<SubmitDataModelTask>(params->submitTask);
    if (!submit(reinterpret_cast<void*>(params->taskContext), &work, 1))
        throw std::runtime_error("Studio rejected the native snapshot task");
    const auto remaining = RemainingMilliseconds(task->result.deadlineTick);
    if (!remaining || WaitForSingleObject(task->completed, remaining) != WAIT_OBJECT_0)
    {
        task->cancelled.store(true, std::memory_order_release);
        throw std::runtime_error("Native snapshot exceeded its response deadline");
    }
    *params = task->result;
    task->delivered = params->status == 4;
    return task->delivered ? 0 : params->status;
}

static DWORD SerializerBoundary(ReniumSerializerParams* params)
{
    try { return QueueSerializer(params); }
    catch (const std::exception& error)
    {
        params->status = 0xE00A;
        SetError(params, error.what());
        return params->status;
    }
    catch (...)
    {
        params->status = 0xE00B;
        SetError(params, "Unknown native snapshot queue failure");
        return params->status;
    }
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumRun(ReniumSerializerParams* params)
{
    if (!params) return 0xE000;
    __try { return SerializerBoundary(params); }
    __except (RecordException(params, GetExceptionInformation())) { return params->status; }
}

static DWORD PublishCaptureResponse(ReniumSerializerParams* params, DWORD result)
{
    CaptureResponse response{0x50414352, 1, params->status, result,
        params->outputSize, params->identitySize, params->queueMicros,
        params->serializeMicros, params->identityMicros, params->writeMicros, {}};
    strncpy_s(response.error, params->error, _TRUNCATE);
    // Also publish errors, but never recreate the host's cancelled transport.
    const auto file = CreateFileW(params->outputPath, GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
        OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) return 0xE00C;
    DWORD written = 0;
    BOOL ok = FALSE;
    __try { ok = WriteFile(file, &response, sizeof(response), &written, nullptr); }
    __finally { CloseHandle(file); }
    return ok && written == sizeof(response) ? result : 0xE00D;
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumCaptureRun(ReniumSerializerParams* params)
{
    if (!params) return 0xE000;
    DWORD result = 0xE001;
    __try
    {
        __try
        {
            if (params->captureMode == 1)
                result = PublishCaptureResponse(params, ReniumRun(params));
        }
        __except (RecordException(params, GetExceptionInformation())) { result = params->status; }
    }
    __finally
    {
        // QueueSerializer copied the request before submitting; queued work
        // never borrows this input, even when its response deadline expires.
        VirtualFree(params, 0, MEM_RELEASE);
    }
    return result;
}

#include "renium_studio_observation.h"

struct PackageModifiedStateTask
{
    volatile LONG references;
    HANDLE completed;
    void* target;
    void* owner;
    SetPackageModifiedState setter;
    std::uint32_t value;
    std::uint64_t deadlineTick;
    std::uint32_t status;
    std::uint32_t exceptionCode;
    char error[256];
};

static void ReleasePackageModifiedStateTask(PackageModifiedStateTask* task)
{
    if (InterlockedDecrement(&task->references) == 0)
    {
        CloseHandle(task->completed);
        delete task;
    }
}

static int RecordPackageModifiedStateException(
    PackageModifiedStateTask* task,
    EXCEPTION_POINTERS* exception)
{
    task->exceptionCode = exception->ExceptionRecord->ExceptionCode;
    task->status = 0xE30B;
    sprintf_s(
        task->error,
        "PackageLink.ModifiedState raised exception 0x%08X at %p",
        static_cast<unsigned>(task->exceptionCode),
        exception->ExceptionRecord->ExceptionAddress);
    return EXCEPTION_EXECUTE_HANDLER;
}

static void SetPackageModifiedStateCaught(PackageModifiedStateTask* task)
{
    if (!RemainingMilliseconds(task->deadlineTick))
    {
        task->status = 0xE30F;
        strncpy_s(task->error, "package DataModel write task timed out", _TRUNCATE);
        return;
    }
    __try
    {
        task->setter(task->target, task->value);
        task->status = 4;
    }
    __except (RecordPackageModifiedStateException(task, GetExceptionInformation()))
    {
    }
}

static void RunPackageModifiedStateTask(PackageActionParams* params)
{
    if (!AddOwnerReference(reinterpret_cast<void*>(params->owner)))
    {
        params->status = 0xE30A;
        SetPackageActionError(params, "package root owner expired");
        return;
    }
    auto task = new (std::nothrow) PackageModifiedStateTask{};
    if (!task)
    {
        ReleaseOwnerReference(reinterpret_cast<void*>(params->owner));
        params->status = 0xE30C;
        SetPackageActionError(params, "could not allocate package write task");
        return;
    }
    task->references = 2;
    task->completed = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    task->target = reinterpret_cast<void*>(params->target);
    task->owner = reinterpret_cast<void*>(params->owner);
    task->setter = reinterpret_cast<SetPackageModifiedState>(
        params->moduleBase + params->operationRva);
    task->value = static_cast<std::uint32_t>(params->value);
    task->deadlineTick = params->deadlineTick;
    task->status = 2;
    if (!task->completed)
    {
        params->exceptionCode = GetLastError();
        params->status = 0xE30D;
        SetPackageActionError(params, "could not create package write completion event");
        ReleaseOwnerReference(task->owner);
        task->references = 1;
        ReleasePackageModifiedStateTask(task);
        return;
    }

    std::function<void()> writeTask{[task]() {
        SetPackageModifiedStateCaught(task);
        ReleaseOwnerReference(task->owner);
        SetEvent(task->completed);
        ReleasePackageModifiedStateTask(task);
    }};
    auto submit = reinterpret_cast<SubmitDataModelTask>(
        params->moduleBase + params->submitTaskRva);
    if (!submit(reinterpret_cast<void*>(params->taskContext), &writeTask, 1))
    {
        params->status = 0xE30E;
        SetPackageActionError(params, "Studio rejected the package DataModel write task");
        ReleaseOwnerReference(task->owner);
        ReleasePackageModifiedStateTask(task);
        ReleasePackageModifiedStateTask(task);
        return;
    }

    const auto remaining = RemainingMilliseconds(task->deadlineTick);
    const auto waited = remaining
        ? WaitForSingleObject(task->completed, remaining < 4000 ? remaining : 4000)
        : WAIT_TIMEOUT;
    if (waited == WAIT_OBJECT_0)
    {
        params->status = task->status;
        params->exceptionCode = task->exceptionCode;
        strncpy_s(params->error, task->error, _TRUNCATE);
        params->result[0] = task->status == 4 ? 1 : 0;
        params->resultValid = task->status == 4 ? 1 : 0;
    }
    else
    {
        params->status = 0xE30F;
        params->exceptionCode = waited;
        SetPackageActionError(params, "package DataModel write task timed out");
    }
    ReleasePackageModifiedStateTask(task);
}

struct PackageOperationCompletion
{
    HANDLE completed = nullptr;
    std::mutex mutex;
    bool succeeded = false;
    std::string error;

    ~PackageOperationCompletion()
    {
        if (completed)
            CloseHandle(completed);
    }
};

struct PackageOperationTask
{
    volatile LONG references = 2;
    HANDLE dispatched = nullptr;
    SharedInstance root{};
    void* service = nullptr;
    std::uintptr_t operation = 0;
    std::int64_t version = 0;
    std::uint32_t mode = 0;
    std::uint64_t deadlineTick = 0;
    std::uint32_t status = 2;
    std::uint32_t exceptionCode = 0;
    char error[256]{};
    std::shared_ptr<PackageOperationCompletion> completion;
};

static void ReleasePackageOperationTask(PackageOperationTask* task)
{
    if (InterlockedDecrement(&task->references) == 0)
    {
        if (task->dispatched)
            CloseHandle(task->dispatched);
        delete task;
    }
}

static void CallPackageOperation(PackageOperationTask* task)
{
    if (!RemainingMilliseconds(task->deadlineTick))
    {
        task->status = 0xE30F;
        strncpy_s(task->error, "package operation task timed out", _TRUNCATE);
        return;
    }
    const auto completion = task->completion;
    std::function<void(std::string)> failed{[completion](std::string message) {
        {
            std::lock_guard lock(completion->mutex);
            completion->error = std::move(message);
        }
        SetEvent(completion->completed);
    }};
    if (task->mode == 7)
    {
        std::function<void()> succeeded{[completion]() {
            {
                std::lock_guard lock(completion->mutex);
                completion->succeeded = true;
            }
            SetEvent(completion->completed);
        }};
        reinterpret_cast<PublishPackage>(task->operation)(
            task->service,
            &task->root,
            false,
            &succeeded,
            &failed);
    }
    else
    {
        std::function<void(std::shared_ptr<void>)> succeeded{
            [completion](std::shared_ptr<void>) {
                {
                    std::lock_guard lock(completion->mutex);
                    completion->succeeded = true;
                }
                SetEvent(completion->completed);
            }};
        reinterpret_cast<SetPackageVersion>(task->operation)(
            task->service,
            reinterpret_cast<const std::shared_ptr<void>*>(&task->root),
            task->version,
            &succeeded,
            &failed);
    }
    task->status = 4;
}

static void CallPackageOperationCppCaught(PackageOperationTask* task)
{
    try
    {
        CallPackageOperation(task);
    }
    catch (const std::exception& exception)
    {
        task->status = 0xE30B;
        strncpy_s(task->error, exception.what(), _TRUNCATE);
    }
    catch (...)
    {
        task->status = 0xE30B;
        strncpy_s(task->error, "Studio package operation raised an unknown exception", _TRUNCATE);
    }
}

static int RecordPackageOperationException(
    PackageOperationTask* task,
    EXCEPTION_POINTERS* exception)
{
    task->exceptionCode = exception->ExceptionRecord->ExceptionCode;
    task->status = 0xE30B;
    sprintf_s(
        task->error,
        "Studio package operation raised exception 0x%08X at %p",
        static_cast<unsigned>(task->exceptionCode),
        exception->ExceptionRecord->ExceptionAddress);
    return EXCEPTION_EXECUTE_HANDLER;
}

static void CallPackageOperationCaught(PackageOperationTask* task)
{
    __try
    {
        CallPackageOperationCppCaught(task);
    }
    __except (RecordPackageOperationException(task, GetExceptionInformation()))
    {
    }
}

static void RunPackageOperationTask(PackageActionParams* params)
{
    std::shared_ptr<PackageOperationCompletion> completion;
    try
    {
        completion = std::make_shared<PackageOperationCompletion>();
    }
    catch (...)
    {
        params->status = 0xE30C;
        SetPackageActionError(params, "could not allocate package operation completion");
        return;
    }
    completion->completed = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (!completion->completed)
    {
        params->exceptionCode = GetLastError();
        params->status = 0xE30D;
        SetPackageActionError(params, "could not create package operation completion event");
        return;
    }
    if (!AddOwnerReference(reinterpret_cast<void*>(params->owner)))
    {
        params->status = 0xE30A;
        SetPackageActionError(params, "package root owner expired");
        return;
    }
    auto task = new (std::nothrow) PackageOperationTask{};
    if (!task)
    {
        ReleaseOwnerReference(reinterpret_cast<void*>(params->owner));
        params->status = 0xE30C;
        SetPackageActionError(params, "could not allocate package operation task");
        return;
    }
    task->dispatched = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    task->root = {
        reinterpret_cast<void*>(params->target),
        reinterpret_cast<void*>(params->owner)};
    task->service = reinterpret_cast<void*>(params->value);
    task->operation = params->moduleBase + params->operationRva;
    task->version = static_cast<std::int64_t>(params->reservedRva);
    task->mode = params->mode;
    task->deadlineTick = params->deadlineTick;
    task->completion = completion;
    if (!task->dispatched)
    {
        params->exceptionCode = GetLastError();
        params->status = 0xE30D;
        SetPackageActionError(params, "could not create package operation task event");
        ReleaseOwnerReference(task->root.owner);
        task->references = 1;
        ReleasePackageOperationTask(task);
        return;
    }

    std::function<void()> operationTask{[task]() {
        CallPackageOperationCaught(task);
        ReleaseOwnerReference(task->root.owner);
        SetEvent(task->dispatched);
        ReleasePackageOperationTask(task);
    }};
    auto submit = reinterpret_cast<SubmitDataModelTask>(
        params->moduleBase + params->submitTaskRva);
    if (!submit(reinterpret_cast<void*>(params->taskContext), &operationTask, 1))
    {
        params->status = 0xE30E;
        SetPackageActionError(params, "Studio rejected the package operation task");
        ReleaseOwnerReference(task->root.owner);
        ReleasePackageOperationTask(task);
        ReleasePackageOperationTask(task);
        return;
    }

    auto remaining = RemainingMilliseconds(task->deadlineTick);
    const auto dispatched = remaining
        ? WaitForSingleObject(task->dispatched, remaining)
        : WAIT_TIMEOUT;
    if (dispatched != WAIT_OBJECT_0)
    {
        params->status = 0xE30F;
        params->exceptionCode = dispatched;
        SetPackageActionError(params, "Studio package operation task timed out");
        ReleasePackageOperationTask(task);
        return;
    }
    params->status = task->status;
    params->exceptionCode = task->exceptionCode;
    strncpy_s(params->error, task->error, _TRUNCATE);
    ReleasePackageOperationTask(task);
    if (params->status != 4)
        return;

    remaining = RemainingMilliseconds(params->deadlineTick);
    const auto completed = remaining
        ? WaitForSingleObject(completion->completed, remaining)
        : WAIT_TIMEOUT;
    if (completed != WAIT_OBJECT_0)
    {
        params->status = 0xE30F;
        params->exceptionCode = completed;
        SetPackageActionError(params, "Package operation did not finish before the deadline");
        return;
    }
    std::lock_guard completionLock(completion->mutex);
    if (!completion->succeeded)
    {
        params->status = 0xE310;
        SetPackageActionError(
            params,
            completion->error.empty()
                ? "Studio rejected the package operation"
                : completion->error.c_str());
        return;
    }
    params->result[0] = 1;
    params->resultValid = 1;
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumPackageAction(
    PackageActionParams* params)
{
    if (!params)
        return 0xE301;
    if ((params->mode != 6 && params->mode != 7 && params->mode != 8) ||
        !params->taskContext || !params->target || !params->owner ||
        !params->moduleBase || !params->timeoutMs || !params->operationRva ||
        !params->submitTaskRva ||
        ((params->mode == 7 || params->mode == 8) && !params->value) ||
        (params->mode == 8 && !params->reservedRva))
    {
        params->status = 0xE301;
        SetPackageActionError(params, "invalid package action parameters");
        return params->status;
    }
    params->status = 1;
    params->exceptionCode = 0;
    params->window = 0;
    params->action = 0;
    params->enabled = 0;
    params->uiThreadId = 0;
    params->found = 0;
    params->error[0] = '\0';
    params->resultValid = 0;
    params->deadlineTick = GetTickCount64() + params->timeoutMs;
    params->actionDeadlineTick = 0;
    if (params->mode == 6)
        RunPackageModifiedStateTask(params);
    else
        RunPackageOperationTask(params);
    return params->status == 4 ? 0 : params->status;
}

// C++ ABI boundary only: the host resolves/validates reflection descriptors and
// owns authorization. The engine supplies the string conversion and runs it on
// the selected DataModel's queue, never on this remote entry thread.
struct PropertyReadParams
{
    std::uint64_t taskContext;
    std::uint64_t submitTask;
    std::uint64_t target;
    std::uint64_t owner;
    std::uint64_t dataModelOwner;
    std::uint64_t descriptor;
    std::uint64_t getter;
    std::uint64_t classDescriptor;
    std::uint64_t descriptorVtable;
    std::uint64_t classOffset;
    std::uint32_t timeoutMs;
    std::uint32_t status;
    std::uint32_t outputSize;
    std::uint32_t exceptionCode;
    char output[65536];
    char error[256];
    std::uint64_t identityBinding;
    std::uint64_t identityGetter;
    std::uint64_t setter;
    std::uint32_t operation; // 0 identity only, 1 read, 2 write/read-back
    std::uint32_t inputSize;
    unsigned char identity[16];
    unsigned char expectedIdentity[16];
    char input[65536];
    std::uint64_t parentOffset;
    std::uint32_t ancestorCount;
    std::uint32_t reserved;
    std::uint64_t ancestors[65]; // target first; selected DataModel last
    std::uint64_t selfOffset;
    std::uint64_t extraInput;
    std::uint64_t extraInputSize;
};
static_assert(sizeof(PropertyReadParams) == 132048);

struct PropertyReadTask
{
    PropertyReadParams result{};
    std::string extraInput;
    HANDLE completed = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    std::uint64_t deadline = 0;
    ~PropertyReadTask() { if (completed) CloseHandle(completed); }
};

static void ReadPropertyCore(PropertyReadTask* task)
{
    auto& p = task->result;
    if (!RemainingMilliseconds(task->deadline))
    {
        p.status = 0xE40F;
        strncpy_s(p.error, "property read expired before Studio could run it", _TRUNCATE);
        return;
    }
    if (*reinterpret_cast<std::uint64_t*>(p.target + p.classOffset) != p.classDescriptor ||
        *reinterpret_cast<std::uint64_t*>(p.descriptor) != p.descriptorVtable)
    {
        p.status = 0xE402;
        strncpy_s(p.error, "property identity changed before execution", _TRUNCATE);
        return;
    }
    try
    {
        for (std::uint32_t i = 0; i + 1 < p.ancestorCount; ++i)
            if (*reinterpret_cast<std::uint64_t*>(p.ancestors[i] + p.parentOffset) != p.ancestors[i + 1])
                throw std::runtime_error("property target hierarchy changed before execution");
        if (*reinterpret_cast<std::uint64_t*>(p.target + p.selfOffset) != p.target ||
            *reinterpret_cast<std::uint64_t*>(p.target + p.selfOffset + 8) != p.owner)
            throw std::runtime_error("property target ownership changed before execution");
        // Only pin the target after validating it under the DataModel lock.
        // The raw pointer from an earlier lookup may already have been deleted.
        auto owner = reinterpret_cast<void*>(p.owner);
        if (!AddOwnerReference(owner))
            throw std::runtime_error("property target was destroyed before execution");
        auto targetHold = std::shared_ptr<void>(owner, ReleaseOwnerReference);
        using IdentityGetter = void*(__fastcall*)(void*, void*, void*);
        reinterpret_cast<IdentityGetter>(p.identityGetter)(
            reinterpret_cast<void*>(p.identityBinding), p.identity,
            reinterpret_cast<void*>(p.target));
        if (!p.operation)
        {
            p.status = 4;
            return;
        }
        // Operation 3 captures the first identity and read together. It cannot
        // write; subsequent ordinary reads/writes still require that identity.
        if (p.operation != 3 && memcmp(p.identity, p.expectedIdentity, sizeof(p.identity)))
            throw std::runtime_error("property target was replaced; request access again");
        if (p.operation == 6)
        {
            renium_history::Binding binding{};
            if (p.inputSize) {
                if (p.inputSize <= sizeof(binding)) throw std::runtime_error("Missing history registration token");
                std::memcpy(&binding, p.input, sizeof(binding));
                renium_history::Register(binding, reinterpret_cast<void*>(p.target), p.ancestors[p.ancestorCount - 1],
                    reinterpret_cast<void*>(p.dataModelOwner),
                    std::string(p.input + sizeof(binding), p.inputSize - sizeof(binding)),
                    [](std::uintptr_t address, void* output, std::size_t size) {
                        SIZE_T copied = 0;
                        return ReadProcessMemory(GetCurrentProcess(), reinterpret_cast<const void*>(address), output, size, &copied) && copied == size;
                    });
            }
            binding = renium_history::GetBinding();
            std::memcpy(p.output, &binding, sizeof(binding));
            p.outputSize = sizeof(binding);
            p.status = 4;
            return;
        }
        if (p.operation == 8)
        {
            if (p.inputSize) {
                if (p.inputSize != sizeof(renium_terrain_observation::Request)) throw std::runtime_error("Invalid Terrain notification request");
                renium_terrain_observation::Request request{};
                std::memcpy(&request, p.input, sizeof(request));
                renium_terrain_observation::Install(reinterpret_cast<void*>(p.target), reinterpret_cast<void*>(p.owner), request,
                    p.classOffset, p.selfOffset, p.parentOffset,
                    [](std::uintptr_t address, void* output, std::size_t size) {
                        SIZE_T copied = 0;
                        return ReadProcessMemory(GetCurrentProcess(), reinterpret_cast<const void*>(address), output, size, &copied) && copied == size;
                    }, AddOwnerReference, ReleaseOwnerReference);
            }
            const auto binding = renium_terrain_observation::GetBinding(reinterpret_cast<void*>(p.target));
            std::memcpy(p.output, &binding, sizeof(binding));
            p.outputSize = sizeof(binding);
            p.status = 4;
            return;
        }
        if (p.operation == 7)
        {
            const auto changed = renium_terrain::Apply(reinterpret_cast<void*>(p.target), targetHold,
                p.ancestors[p.ancestorCount - 1], task->extraInput,
                [](std::uintptr_t address, void* output, std::size_t size) {
                    SIZE_T copied = 0;
                    return ReadProcessMemory(GetCurrentProcess(), reinterpret_cast<const void*>(address), output, size, &copied) && copied == size;
                });
            p.output[0] = changed.changed ? 1 : 0;
            std::memcpy(p.output + 1, changed.fingerprint.data(), changed.fingerprint.size());
            p.outputSize = 65; p.status = 4;
            return;
        }
        if (p.operation == 2)
        {
            const std::string input(p.input, p.inputSize);
            using Setter = bool(__fastcall*)(void*, void*, const std::string*);
            if (!reinterpret_cast<Setter>(p.setter)(
                    reinterpret_cast<void*>(p.descriptor), reinterpret_cast<void*>(p.target), &input))
                throw std::runtime_error("Studio rejected this property's text value");
        }
        // MSVC puts a member's hidden nontrivial return buffer after `this`.
        // The descriptor constructs the returned std::string in that buffer.
        alignas(std::string) unsigned char storage[sizeof(std::string)];
        using Getter = void*(__fastcall*)(void*, void*, void*);
        reinterpret_cast<Getter>(p.getter)(
            reinterpret_cast<void*>(p.descriptor), storage, reinterpret_cast<void*>(p.target));
        auto value = reinterpret_cast<std::string*>(storage);
        if (value->size() > sizeof(p.output))
        {
            p.status = 0xE403;
            strncpy_s(p.error, "property value exceeds the 64 KiB response limit", _TRUNCATE);
        }
        else
        {
            p.outputSize = static_cast<std::uint32_t>(value->size());
            memcpy(p.output, value->data(), value->size());
            p.status = 4;
        }
        value->~basic_string();
    }
    catch (const std::exception& error)
    {
        p.status = 0xE404;
        strncpy_s(p.error, error.what(), _TRUNCATE);
    }
}

static void ReadPropertyCaught(PropertyReadTask* task)
{
    __try { ReadPropertyCore(task); }
    __except (EXCEPTION_EXECUTE_HANDLER)
    {
        task->result.status = 0xE405;
        task->result.exceptionCode = GetExceptionCode();
        strncpy_s(task->result.error, "Studio raised an exception while reading the property", _TRUNCATE);
    }
}

static DWORD RunPropertyRead(PropertyReadParams* params)
{
    if (!params || !params->taskContext || !params->submitTask || !params->target ||
        !params->owner || !params->dataModelOwner || !params->descriptor || !params->getter ||
        !params->classDescriptor || !params->descriptorVtable || params->classOffset > 0x100 ||
        !params->identityBinding || !params->identityGetter || (params->operation > 3 && params->operation != 6 && params->operation != 7 && params->operation != 8) ||
        (params->operation == 2 && !params->setter) || params->inputSize > sizeof(params->input) ||
        params->extraInputSize > 128 * 1024 * 1024 ||
        (params->operation == 7 ? (!params->extraInput || !params->extraInputSize) : params->extraInputSize != 0) ||
        params->parentOffset > 0x200 || params->selfOffset > 0x80 || params->ancestorCount < 2 || params->ancestorCount > 65 ||
        params->ancestors[0] != params->target ||
        !params->timeoutMs || params->timeoutMs > 3000)
        return 0xE401;
    auto modelOwner = reinterpret_cast<void*>(params->dataModelOwner);
    if (!AddOwnerReference(modelOwner)) return 0xE402;
    auto modelHold = std::shared_ptr<void>(modelOwner, ReleaseOwnerReference);
    auto task = std::make_shared<PropertyReadTask>();
    task->result = *params;
    if (params->operation == 7) task->extraInput.assign(reinterpret_cast<const char*>(params->extraInput), static_cast<std::size_t>(params->extraInputSize));
    task->deadline = GetTickCount64() + params->timeoutMs;
    if (!task->completed) return 0xE406;
    // Keep the DataModel alive through submission/wait, but do not put a strong
    // reference to it inside its own queue (which could form a shutdown cycle).
    std::function<void()> readTask{[task]() {
        ReadPropertyCaught(task.get());
        SetEvent(task->completed);
    }};
    auto submit = reinterpret_cast<SubmitDataModelTask>(params->submitTask);
    if (!submit(reinterpret_cast<void*>(params->taskContext), &readTask, 1)) return 0xE407;
    const auto remaining = RemainingMilliseconds(task->deadline);
    if (!remaining || WaitForSingleObject(task->completed, remaining) != WAIT_OBJECT_0)
        return 0xE40F;
    *params = task->result;
    return params->status == 4 ? 0 : params->status;
}

static DWORD PropertyReadBoundary(PropertyReadParams* params)
{
    try { return RunPropertyRead(params); }
    catch (const std::exception& error)
    {
        if (params) strncpy_s(params->error, error.what(), _TRUNCATE);
        return 0xE408;
    }
    catch (...) { return 0xE409; }
}

extern "C" __declspec(dllexport) DWORD WINAPI ReniumReadProperty(PropertyReadParams* params)
{
    __try { return PropertyReadBoundary(params); }
    __except (EXCEPTION_EXECUTE_HANDLER)
    {
        params->exceptionCode = GetExceptionCode();
        strncpy_s(params->error, "Studio target expired before the property task was submitted", _TRUNCATE);
        return 0xE40A;
    }
}

#include "renium_studio_reader.h"
