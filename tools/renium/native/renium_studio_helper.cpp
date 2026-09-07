#include <Windows.h>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
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
};
static_assert(sizeof(PropertyReadParams) == 132032);

struct PropertyReadTask
{
    PropertyReadParams result{};
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
        if (memcmp(p.identity, p.expectedIdentity, sizeof(p.identity)))
            throw std::runtime_error("property target was replaced; request access again");
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
        !params->identityBinding || !params->identityGetter || params->operation > 2 ||
        (params->operation == 2 && !params->setter) || params->inputSize > sizeof(params->input) ||
        params->parentOffset > 0x200 || params->selfOffset > 0x80 || params->ancestorCount < 2 || params->ancestorCount > 65 ||
        params->ancestors[0] != params->target ||
        !params->timeoutMs || params->timeoutMs > 3000)
        return 0xE401;
    auto modelOwner = reinterpret_cast<void*>(params->dataModelOwner);
    if (!AddOwnerReference(modelOwner)) return 0xE402;
    auto modelHold = std::shared_ptr<void>(modelOwner, ReleaseOwnerReference);
    auto task = std::make_shared<PropertyReadTask>();
    task->result = *params;
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
