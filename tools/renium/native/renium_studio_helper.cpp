#include <Windows.h>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <new>
#include <ostream>
#include <sstream>
#include <string>

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
    std::uint32_t reserved;
    SharedInstance roots[256];
    wchar_t outputPath[520];
    char error[512];
};

static_assert(sizeof(ReniumSerializerParams) == 5792);
static_assert(offsetof(ReniumSerializerParams, status) == 68);
static_assert(offsetof(ReniumSerializerParams, placeMode) == 136);
static_assert(offsetof(ReniumSerializerParams, roots) == 144);
static_assert(offsetof(ReniumSerializerParams, outputPath) == 4240);
static_assert(offsetof(ReniumSerializerParams, error) == 5280);

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

static DWORD RunCore(RunState* state)
{
    auto params = state->params;
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

    SharedVector roots{};
    if (!params->placeMode)
    {
        QueryPerformanceCounter(&started);
        builder(state->context, reinterpret_cast<void*>(params->dataModel));
        state->contextBuilt = true;
        QueryPerformanceCounter(&finished);
        params->contextMicros = ElapsedMicros(started, finished, frequency);

        roots = {
            params->roots,
            params->roots + params->count,
            params->roots + params->count,
        };
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
        auto instance = reinterpret_cast<unsigned char*>(params->dataModel) + 0x1c8;
        auto roots = *reinterpret_cast<void**>(instance + 0x70);
        SharedVector emptyRoots{};
        serializer(
            static_cast<std::ostream*>(state->stream),
            instance,
            roots ? roots : &emptyRoots,
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
    params->status = 3;

    state->bytes = new std::string(state->stream->str());
    state->bytesBuilt = true;
    params->outputSize = state->bytes->size();
    HANDLE file = CreateFileW(
        params->outputPath,
        GENERIC_WRITE,
        0,
        nullptr,
        CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL,
        nullptr);
    if (file == INVALID_HANDLE_VALUE)
    {
        params->status = 0xE002;
        SetError(params, "CreateFileW failed");
        return params->status;
    }

    QueryPerformanceCounter(&started);
    std::size_t position = 0;
    BOOL wrote = TRUE;
    while (position < state->bytes->size())
    {
        const auto remaining = state->bytes->size() - position;
        const auto chunk = static_cast<DWORD>(
            remaining > MAXDWORD ? MAXDWORD : remaining);
        DWORD written = 0;
        wrote = WriteFile(
            file,
            state->bytes->data() + position,
            chunk,
            &written,
            nullptr);
        if (!wrote || written != chunk)
            break;
        position += written;
    }
    if (wrote)
        wrote = FlushFileBuffers(file);
    CloseHandle(file);
    QueryPerformanceCounter(&finished);
    params->writeMicros = ElapsedMicros(started, finished, frequency);
    if (!wrote || position != state->bytes->size())
    {
        DeleteFileW(params->outputPath);
        params->status = 0xE003;
        SetError(params, "WriteFile failed");
        return params->status;
    }

    params->status = 4;
    return 0;
}

static void CleanupCore(RunState* state)
{
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

extern "C" __declspec(dllexport) DWORD WINAPI ReniumRun(
    ReniumSerializerParams* params)
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
    params->initialMxcsr = GetMxcsr();
    params->error[0] = '\0';
    if (!params->moduleBase ||
        !params->serializerRva ||
        !params->contextBuilderRva ||
        !params->contextDestroyRva ||
        !params->rootCollectorRva ||
        !params->deallocatorRva ||
        !params->dataModel ||
        !params->count ||
        params->count > 256 ||
        !params->outputPath[0])
    {
        params->status = 0xE001;
        SetError(params, "invalid parameters");
        return params->status;
    }

    RunState state{};
    state.params = params;
    const auto result = RunCppCaught(&state);
    CleanupCaught(&state);
    SetMxcsr(params->initialMxcsr);
    return result;
}
