#pragma once
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <stdexcept>

static constexpr std::uint32_t ReniumSerializerVersion = 2;

struct SharedInstance
{
    void* instance;
    void* owner;
};

// v2 appends the structural context binding. The version occupies a formerly
// reserved v1 word, so old requests can be rejected before reading the suffix.
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
    std::uint32_t protocolVersion;
    std::uint32_t contextFieldOffset;
    std::uint32_t dataModelInstanceSize;
    std::uint64_t contextPointerMask;
    std::uint64_t contextObserved;
    std::uint32_t contextRead;
    std::uint32_t contextReserved;
};

static_assert(sizeof(ReniumSerializerParams) == 5936);
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
static_assert(offsetof(ReniumSerializerParams, protocolVersion) == 5900);
static_assert(offsetof(ReniumSerializerParams, contextFieldOffset) == 5904);
static_assert(offsetof(ReniumSerializerParams, dataModelInstanceSize) == 5908);
static_assert(offsetof(ReniumSerializerParams, contextPointerMask) == 5912);
static_assert(offsetof(ReniumSerializerParams, contextObserved) == 5920);
static_assert(offsetof(ReniumSerializerParams, contextRead) == 5928);

// RCAP v1 is still used by attribute observation. Capture alone uses v2 with
// a context receipt. The RBXL and identity-row formats are unchanged.
struct CaptureResponse
{
    std::uint32_t magic, version, status, exitCode;
    std::uint64_t bytes, identities, queueMicros, serializeMicros, identityMicros, writeMicros;
    char error[256];
};
static_assert(sizeof(CaptureResponse) == 320);

struct CaptureContextResponse
{
    CaptureResponse base;
    std::uint32_t fieldOffset, instanceSize;
    std::uint64_t pointerMask, observed;
    std::uint32_t read, reserved;
};
static_assert(sizeof(CaptureContextResponse) == 352);
static_assert(offsetof(CaptureContextResponse, fieldOffset) == 320);
static_assert(offsetof(CaptureContextResponse, instanceSize) == 324);
static_assert(offsetof(CaptureContextResponse, pointerMask) == 328);
static_assert(offsetof(CaptureContextResponse, observed) == 336);
static_assert(offsetof(CaptureContextResponse, read) == 344);

inline CaptureContextResponse MakeCaptureResponse(const ReniumSerializerParams& p, std::uint32_t result)
{
    CaptureContextResponse response{{0x50414352, p.captureMode ? ReniumSerializerVersion : 1, p.status, result,
        p.outputSize, p.identitySize, p.queueMicros, p.serializeMicros, p.identityMicros, p.writeMicros, {}},
        p.contextFieldOffset, p.dataModelInstanceSize, p.contextPointerMask, p.contextObserved, p.contextRead, 0};
    std::memcpy(response.base.error, p.error, sizeof(response.base.error) - 1);
    return response;
}

inline std::uint32_t CaptureResponseSize(const ReniumSerializerParams& p)
{
    return p.captureMode ? sizeof(CaptureContextResponse) : sizeof(CaptureResponse);
}

inline void ValidateSerializerProtocol(const ReniumSerializerParams& p)
{
    if (p.protocolVersion != ReniumSerializerVersion)
        throw std::runtime_error("Incompatible native serializer protocol; use the matching Renium helper");
}

inline void ValidateCaptureContextBinding(const ReniumSerializerParams& p)
{
    ValidateSerializerProtocol(p);
    const auto offset = p.contextFieldOffset;
    const auto size = p.dataModelInstanceSize;
    const auto mask = p.contextPointerMask;
    const auto alignment = ~mask + 1;
    if (size < 8 || offset < 8 || offset % 8 || offset > size - 8 ||
        !mask || !alignment || (alignment & (alignment - 1)) ||
        p.taskContext < 0x10000 || p.taskContext >= 0x0000800000000000 ||
        (p.taskContext & mask) != p.taskContext)
        throw std::runtime_error("Invalid DataModel capture task-context binding");
}

// Invoked after the existing DataModel identity check, inside its queued task.
// Keep the expected pointer from submission; never refresh it under the lock.
inline void CheckCaptureContext(ReniumSerializerParams& p, const void* model)
{
    ValidateCaptureContextBinding(p);
    std::uint64_t raw = 0;
    std::memcpy(&raw, static_cast<const unsigned char*>(model) + p.contextFieldOffset, sizeof(raw));
    p.contextObserved = raw & p.contextPointerMask;
    p.contextRead = 1;
    if (p.contextObserved != p.taskContext)
        throw std::runtime_error("DataModel task context changed before capture");
}
