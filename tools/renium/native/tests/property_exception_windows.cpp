// Include the production boundary so layout and decoder tests cannot drift
// behind a duplicate struct. This executable never loads or contacts Studio.
#include "../renium_studio_helper.cpp"
#include <cassert>
#include <cstdio>
#include <fcntl.h>
#include <io.h>
#pragma comment(lib, "user32.lib")

struct UnfamiliarException {};
static unsigned failure;
static void* ThrowPropertyFailure(void*, void*, void*)
{
    switch (failure) {
    case 0: return nullptr;
    case 1: throw std::runtime_error("standard refusal");
    case 2: throw static_cast<const char*>("literal refusal");
    case 3: throw std::string("short refusal");
    case 4: throw std::string(400, 'x');
    case 5: throw UnfamiliarException{};
    case 6: throw static_cast<const char*>(nullptr);
    case 7: throw reinterpret_cast<const char*>(1);
    case 8: RaiseException(EXCEPTION_ACCESS_VIOLATION, 0, 0, nullptr); break;
    case 9: RaiseException(EXCEPTION_ILLEGAL_INSTRUCTION, 0, 0, nullptr); break;
    }
    return nullptr;
}

static void WriteResult(const PropertyReadParams& result)
{
    assert(std::fwrite(&result, sizeof(result), 1, stdout) == 1);
}

static unsigned submissions;
static std::function<void()> delayed;
static bool SubmitRead(void*, std::function<void()>* work, std::uint32_t)
{
    ++submissions;
    (*work)();
    return true;
}
static bool DelayRead(void*, std::function<void()>* work, std::uint32_t)
{
    delayed = *work;
    return true;
}
static void* ReadIdentity(void*, void* output, void*)
{
    std::memset(output, 1, 16);
    return output;
}
static void* ReadValue(void*, void* output, void* target)
{
    const auto index = *static_cast<std::uint64_t*>(target);
    if (index == 1) throw std::runtime_error("standard refusal");
    if (index == 2) throw static_cast<const char*>("literal refusal");
    if (index == 3) RaiseException(EXCEPTION_ACCESS_VIOLATION, 0, 0, nullptr);
    return new(output) std::string(std::to_string(index));
}
static void BatchReads()
{
    renium_continuation::taskAbi.store(renium_continuation::Abi::Msvc);
    for (unsigned shift : {0u, 8u}) {
        std::uint64_t owner[]{0, 2}, descriptor = 43, targets[26][12]{};
        std::vector<std::uint64_t> wire((8 + 26 * sizeof(PropertyReadParams)) / 8);
        wire[0] = 26;
        auto rows = reinterpret_cast<PropertyReadParams*>(wire.data() + 1);
        for (unsigned i = 0; i < 26; ++i) {
            auto& p = rows[i];
            p.target = reinterpret_cast<std::uintptr_t>(targets[i]);
            p.owner = p.dataModelOwner = reinterpret_cast<std::uintptr_t>(owner);
            p.taskContext = 1; p.submitTask = reinterpret_cast<std::uintptr_t>(&SubmitRead);
            p.classDescriptor = 42; p.descriptorVtable = descriptor;
            p.descriptor = reinterpret_cast<std::uintptr_t>(&descriptor);
            p.classOffset = 16 + shift; p.selfOffset = 32 + shift; p.parentOffset = 64 + shift;
            targets[i][0] = i; targets[i][p.classOffset / 8] = 42;
            targets[i][p.selfOffset / 8] = p.target; targets[i][p.selfOffset / 8 + 1] = p.owner;
            targets[i][p.parentOffset / 8] = 99;
            p.ancestorCount = 2; p.ancestors[0] = p.target; p.ancestors[1] = 99;
            p.identityBinding = 1; p.identityGetter = reinterpret_cast<std::uintptr_t>(&ReadIdentity);
            p.getter = reinterpret_cast<std::uintptr_t>(&ReadValue); p.operation = 3; p.timeoutMs = 2000;
        }
        targets[4][rows[4].classOffset / 8] = 0;
        targets[5][rows[5].parentOffset / 8] = 0;
        targets[6][rows[6].selfOffset / 8 + 1] = 0;
        submissions = 0;
        assert(ReniumReadPropertyBatch(wire.data()) == 0 && submissions == 1);
        for (unsigned i = 0; i < 26; ++i) {
            auto single = std::make_unique<PropertyReadParams>(rows[i]);
            single->status = single->exceptionCode = single->outputSize = 0;
            std::memset(single->error, 0, sizeof(single->error));
            const auto exit = ReniumReadProperty(single.get());
            assert(exit == (rows[i].status == 4 ? 0 : rows[i].status));
            assert(single->status == rows[i].status && single->exceptionCode == rows[i].exceptionCode);
            assert(!std::strcmp(single->error, rows[i].error));
            assert(single->outputSize == rows[i].outputSize);
            assert(!std::memcmp(single->output, rows[i].output, single->outputSize));
            assert(!std::memcmp(single->identity, rows[i].identity, sizeof(single->identity)));
        }
        submissions = 0;
        rows[25].operation = 2;
        assert(ReniumReadPropertyBatch(wire.data()) == 0xE401 && submissions == 0);
        rows[25].operation = 3; rows[25].taskContext = 2;
        assert(ReniumReadPropertyBatch(wire.data()) == 0xE401 && submissions == 0);
        rows[25].taskContext = 1;
        wire[0] = 33;
        assert(ReniumReadPropertyBatch(wire.data()) == 0xE401);
        wire[0] = 26;
        const auto ownerBeforeTimeout = owner[1];
        for (unsigned i = 0; i < 26; ++i) {
            rows[i].submitTask = reinterpret_cast<std::uintptr_t>(&DelayRead);
            rows[i].timeoutMs = 1;
        }
        assert(ReniumReadPropertyBatch(wire.data()) == 0xE40F);
        wire.clear(); wire.shrink_to_fit(); // The late task owns its input.
        Sleep(20); // Let the coarse GetTickCount64 deadline expire before delivery.
        delayed(); delayed = {};
        assert(owner[1] == ownerBeforeTimeout);
    }
}

int main(int argc, char**)
{
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    _set_error_mode(_OUT_TO_STDERR);
    _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
    if (argc > 1) { BatchReads(); return 0; }
    assert(_setmode(_fileno(stdout), _O_BINARY) != -1);
    const std::uint64_t layout[]{sizeof(PropertyReadParams), offsetof(PropertyReadParams, status),
        offsetof(PropertyReadParams, exceptionCode), offsetof(PropertyReadParams, error),
        offsetof(PropertyReadParams, error) + sizeof(PropertyReadParams::error)};
    assert(std::fwrite(layout, sizeof(layout), 1, stdout) == 1);
    for (failure = 0; failure < 10; ++failure) {
        PropertyReadTask task;
        auto& p = task.result;
        std::uint64_t owner[]{0, 2};
        std::uint64_t target[]{1, 0, reinterpret_cast<std::uintptr_t>(owner)};
        target[1] = reinterpret_cast<std::uintptr_t>(target);
        std::uint64_t descriptor = 1;
        p.target = target[1]; p.owner = target[2]; p.selfOffset = sizeof(std::uint64_t);
        p.classDescriptor = 1; p.descriptorVtable = 1;
        p.descriptor = reinterpret_cast<std::uintptr_t>(&descriptor);
        p.identityGetter = reinterpret_cast<std::uintptr_t>(&ThrowPropertyFailure);
        task.deadline = GetTickCount64() + 10000;
        ReadPropertyCaught(&task);
        assert(p.status == (failure == 0 ? 4 : failure == 1 ? 0xE404 : 0xE405));
        WriteResult(p);
    }

    // Valid image-resident metadata first, then invalidate each link. Stack
    // metadata is readable but must still be refused as outside the image.
    static std::remove_const_t<ThrowInfo> info{};
    static struct { int count; int first; } types{};
    static std::remove_const_t<CatchableType> type{};
    const auto image = reinterpret_cast<std::uintptr_t>(GetModuleHandleW(nullptr));
    const auto rva = [image](const void* address) {
        const auto offset = reinterpret_cast<std::uintptr_t>(address) - image;
        assert(offset && offset <= INT_MAX);
        return static_cast<int>(offset);
    };
    for (unsigned invalid = 0; invalid < 15; ++invalid) {
        info = {}; info.pCatchableTypeArray = rva(&types);
        types = {1, rva(&type)};
        type = {}; type.pType = rva(&typeid(UnfamiliarException));
        EXCEPTION_RECORD record{};
        record.ExceptionCode = EH_EXCEPTION_NUMBER;
        record.NumberParameters = EH_EXCEPTION_PARAMETERS;
        record.ExceptionInformation[0] = EH_MAGIC_NUMBER1;
        record.ExceptionInformation[2] = reinterpret_cast<std::uintptr_t>(&info);
        record.ExceptionInformation[3] = image;
        auto outside = info;
        switch (invalid) {
        case 1: record.NumberParameters = 1; break;
        case 2: record.ExceptionInformation[0] = 0; break;
        case 3: record.ExceptionInformation[2] = 1; break;
        case 4: record.ExceptionInformation[2] = reinterpret_cast<std::uintptr_t>(&outside); break;
        case 5: record.ExceptionInformation[3] = image + 4096; break;
        case 6: info.pCatchableTypeArray = INT_MAX; break;
        case 7: types.count = 0; break;
        case 8: types.first = -1; break;
        case 9: type.pType = INT_MAX; break;
        case 10: info.pForwardCompat = 1; break;
        case 11: type.pType = rva(&typeid(std::string)); type.thisDisplacement.pdisp = -1; break; // wrong size
        case 12: type.pType = rva(&typeid(const char*)); type.sizeOrOffset = sizeof(char*); break; // wrong adjustment
        case 13:
            type.pType = rva(&typeid(const char*)); type.sizeOrOffset = sizeof(char*);
            type.thisDisplacement.pdisp = -1; record.ExceptionInformation[1] = 1; break; // unreadable object
        case 14: {
            static char unterminated[sizeof(TypeDescriptor) + 256];
            std::memset(unterminated, 'x', sizeof(unterminated));
            type.pType = rva(unterminated); break;
        }
        }
        EXCEPTION_POINTERS exception{&record, nullptr};
        PropertyReadParams result{};
        assert(PropertyExceptionFilter(&exception, &result) == EXCEPTION_EXECUTE_HANDLER);
        assert(invalid ? !std::strcmp(result.error, "Studio raised an exception while reading the property")
            : std::strstr(result.error, "UnfamiliarException") != nullptr);
        WriteResult(result);
    }
    return 0;
}
