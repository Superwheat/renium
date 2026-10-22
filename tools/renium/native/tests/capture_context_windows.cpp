#include "../renium_capture_context.h"
#include <array>
#include <cassert>
#include <fstream>
#include <iostream>
#include <limits>
#include <string>
#include <vector>

static ReniumSerializerParams Fixture(std::uint32_t offset, std::uint64_t mask)
{
    ReniumSerializerParams p{};
    p.protocolVersion = ReniumSerializerVersion;
    p.captureMode = 1;
    p.contextFieldOffset = offset;
    p.dataModelInstanceSize = 0x188;
    p.contextPointerMask = mask;
    p.taskContext = 0x12345000;
    return p;
}

static void Refuses(ReniumSerializerParams p, const char* reason)
{
    try {
        // Invalid bindings must fail before any read, even at an unmapped model.
        CheckCaptureContext(p, nullptr);
        assert(false);
    } catch (const std::runtime_error& error) {
        assert(std::string(error.what()).find(reason) != std::string::npos);
        assert(p.contextRead == 0);
    }
}

static void UnitTests()
{
    for (const auto offset : {0x48u, 0x58u, 0xa0u, 0x180u}) {
        for (const auto tag : {0ull, 1ull, 7ull, 15ull, 255ull}) {
            auto p = Fixture(offset, ~tag);
            std::array<unsigned char, 0x188> model{};
            const auto raw = p.taskContext | tag;
            std::memcpy(model.data() + offset, &raw, sizeof(raw));
            CheckCaptureContext(p, model.data());
            assert(p.contextRead == 1 && p.contextObserved == p.taskContext);
            const auto expected = p.taskContext;
            const auto changed = (expected + 0x1000) | tag;
            std::memcpy(model.data() + offset, &changed, sizeof(changed));
            try {
                CheckCaptureContext(p, model.data());
                assert(false);
            } catch (const std::runtime_error& error) {
                assert(std::string(error.what()) == "DataModel task context changed before capture");
                assert(p.contextRead == 1 && p.contextObserved == expected + 0x1000);
                assert(p.taskContext == expected); // never refresh submission's pointer
            }
        }
    }
    for (auto offset : {0u, 7u, 9u, 0x181u, 0x188u, 0xfffffff8u, 0xffffffffu}) {
        auto p = Fixture(offset, ~7ull);
        Refuses(p, "Invalid DataModel capture task-context binding");
    }
    for (auto size : {0u, 7u, 0x47u, 0x4fu}) {
        auto p = Fixture(0x48, ~7ull);
        p.dataModelInstanceSize = size;
        Refuses(p, "Invalid DataModel capture task-context binding");
    }
    for (auto mask : {0ull, 0xffffull, ~5ull}) {
        Refuses(Fixture(0x48, mask), "Invalid DataModel capture task-context binding");
    }
    for (auto pointer : {0ull, 1ull, 0x12345001ull, 0x800000000000ull}) {
        auto p = Fixture(0x48, ~7ull);
        p.taskContext = pointer;
        Refuses(p, "Invalid DataModel capture task-context binding");
    }
    for (auto version : {0u, 1u, 3u, 0xffffffffu}) {
        auto p = Fixture(0x48, ~7ull);
        p.protocolVersion = version;
        Refuses(p, "Incompatible native serializer protocol");
    }
    auto p = Fixture(0x48, ~7ull);
    p.status = 0xE004;
    const char* error = "DataModel task context changed before capture";
    std::memcpy(p.error, error, std::strlen(error));
    auto response = MakeCaptureResponse(p, p.status);
    assert(response.base.version == 2 && CaptureResponseSize(p) == 352);
    assert(response.base.status == p.status && response.base.exitCode == p.status);
    assert(std::string(response.base.error) == error);
    // Observation shares the parameter prefix but keeps its original wire header.
    p.captureMode = 0;
    response = MakeCaptureResponse(p, p.status);
    assert(response.base.version == 1 && CaptureResponseSize(p) == 320);
}

static void PrintLayout()
{
    const std::size_t layout[] = {
        ReniumSerializerVersion, sizeof(ReniumSerializerParams),
        offsetof(ReniumSerializerParams, protocolVersion),
        offsetof(ReniumSerializerParams, taskContext),
        offsetof(ReniumSerializerParams, contextFieldOffset),
        offsetof(ReniumSerializerParams, dataModelInstanceSize),
        offsetof(ReniumSerializerParams, contextPointerMask),
        offsetof(ReniumSerializerParams, contextObserved),
        offsetof(ReniumSerializerParams, contextRead),
        sizeof(CaptureContextResponse), offsetof(CaptureContextResponse, fieldOffset),
        offsetof(CaptureContextResponse, instanceSize), offsetof(CaptureContextResponse, pointerMask),
        offsetof(CaptureContextResponse, observed), offsetof(CaptureContextResponse, read),
    };
    for (const auto value : layout) std::cout << value << ' ';
}

int main(int argc, char** argv)
{
    UnitTests();
    if (argc == 1) { PrintLayout(); return 0; }
    assert(argc == 3);
    ReniumSerializerParams p{};
    std::ifstream input(argv[1], std::ios::binary);
    input.read(reinterpret_cast<char*>(&p), sizeof(p));
    assert(input.gcount() == sizeof(p) && input.peek() == EOF);
    ValidateCaptureContextBinding(p);
    assert(p.contextRead == 0 && p.contextObserved == 0);
    std::vector<unsigned char> model(p.dataModelInstanceSize);
    const auto raw = p.taskContext | ~p.contextPointerMask;
    std::memcpy(model.data() + p.contextFieldOffset, &raw, sizeof(raw));
    CheckCaptureContext(p, model.data());
    p.status = 4;
    const auto response = MakeCaptureResponse(p, 0);
    std::ofstream output(argv[2], std::ios::binary);
    output.write(reinterpret_cast<const char*>(&p), sizeof(p));
    output.write(reinterpret_cast<const char*>(&response), sizeof(response));
    return output.good() ? 0 : 1;
}
