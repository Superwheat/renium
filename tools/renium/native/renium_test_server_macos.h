#pragma once
#include "renium_playtest_macos.h"
#include <dlfcn.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/stat.h>

// An owned destination uses Studio's normal StartServer task and settings.
// Replace only the file consumed by its native model-loader callback.
namespace renium_mac_test_server {
using namespace renium_mac_playtest;
struct Input {
    std::uint32_t version, size, sourcePid, pathSize;
    std::uint64_t sourceSeconds, sourceMicros, slot, original, capture, path;
    unsigned char uuid[16];
};
static_assert(sizeof(Input) == 80);
inline Input input{};
inline std::string file;
inline std::atomic<bool> consumed{false};
// QString has a nontrivial destructor, so ARM64 returns it in x8. Declaring
// this as a raw pointer return would corrupt the loader's calling convention.
struct QtString {
    void* data = nullptr;
    ~QtString() {
        if (!data) return;
        auto refs = static_cast<std::atomic<int>*>(data);
        if (refs->load() != -1 && refs->fetch_sub(1) == 1) {
            const auto deallocate = reinterpret_cast<void(*)(void*, std::size_t, std::size_t)>(
                dlsym(RTLD_DEFAULT, "_ZN10QArrayData10deallocateEPS_mm"));
            if (deallocate) deallocate(data, 2, 8);
        }
    }
};
inline QtString (*fromUtf8)(const char*, int) = nullptr;
inline std::atomic<Producer> original{nullptr};

inline bool SourceAlive() {
    LoadRequest source{};
    source.hostPid = input.sourcePid;
    source.hostSeconds = input.sourceSeconds;
    source.hostMicros = input.sourceMicros;
    return HostAlive(source);
}

inline void Produce(void* capture, void** model) {
    if (!SourceAlive() || consumed.exchange(true))
        throw std::runtime_error("Destination test server lost its source owner");
    auto config = reinterpret_cast<unsigned char*>(Word(capture, input.capture));
    if (!config) throw std::runtime_error("Destination server has no native loading configuration");
    auto field = reinterpret_cast<void**>(config + input.path);
    auto replacement = fromUtf8(file.c_str(), static_cast<int>(file.size()));
    if (!replacement.data) throw std::runtime_error("Cannot create destination server file input");
    std::swap(*field, replacement.data);
    try { original.load()(capture, model); }
    catch (...) { std::swap(*field, replacement.data); throw; }
    std::swap(*field, replacement.data);
    Exchange(input.slot, reinterpret_cast<void*>(&Produce), reinterpret_cast<void*>(input.original));
}

inline void Install(const mach_header_64* header, std::intptr_t slide) {
    const char* requested = std::getenv("RENIUM_TEST_SERVER_INPUT");
    if (!requested) return;
    if (!header || header->filetype != MH_EXECUTE)
        throw std::runtime_error("Destination server has no executable image");
    const std::string path(requested);
    unsetenv("RENIUM_TEST_SERVER_INPUT"); // Never inherit this one-server lease.
    int fd = open(path.c_str(), O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) throw std::runtime_error("Cannot read destination server lease");
    struct Close { int fd; ~Close() { close(fd); } } closeFile{fd};
    struct stat stat{};
    if (fstat(fd, &stat) || !S_ISREG(stat.st_mode) || stat.st_uid != getuid()
        || (stat.st_mode & 077) || stat.st_size < static_cast<off_t>(sizeof(Input)) || stat.st_size > 4096
        || read(fd, &input, sizeof(input)) != sizeof(input) || input.version != 1 || input.size != sizeof(input)
        || !input.pathSize || sizeof(Input) + input.pathSize != static_cast<std::size_t>(stat.st_size)
        || input.capture >= 65536 || input.path >= 65536 || input.capture % 8 || input.path % 8 || !SourceAlive())
        throw std::runtime_error("Invalid destination server lease");
    file.resize(input.pathSize);
    if (read(fd, file.data(), file.size()) != static_cast<ssize_t>(file.size())
        || file.front() != '/' || file.find('\0') != std::string::npos)
        throw std::runtime_error("Invalid destination server file");
    const unsigned char* cursor = reinterpret_cast<const unsigned char*>(header + 1);
    std::uint64_t base = 0, textEnd = 0;
    bool uuid = false, slot = false;
    for (std::uint32_t i = 0; i < header->ncmds; ++i) {
        auto command = reinterpret_cast<const load_command*>(cursor);
        if (command->cmd == LC_UUID)
            uuid = !std::memcmp(reinterpret_cast<const uuid_command*>(command)->uuid, input.uuid, 16);
        if (command->cmd == LC_SEGMENT_64) {
            auto segment = reinterpret_cast<const segment_command_64*>(command);
            if (!std::strcmp(segment->segname, "__TEXT")) { base = segment->vmaddr + slide; textEnd = segment->vmsize; }
        }
        cursor += command->cmdsize;
    }
    if (!base) throw std::runtime_error("Destination server has no executable text segment");
    if (!uuid) throw std::runtime_error("Destination server executable differs from its source");
    if (!input.original || input.original >= textEnd)
        throw std::runtime_error("Destination server loader is outside executable text");
    cursor = reinterpret_cast<const unsigned char*>(header + 1);
    for (std::uint32_t i = 0; i < header->ncmds; ++i) {
        auto command = reinterpret_cast<const load_command*>(cursor);
        if (command->cmd == LC_SEGMENT_64) {
            auto segment = reinterpret_cast<const segment_command_64*>(command);
            const auto begin = segment->vmaddr + slide;
            if (segment->initprot & VM_PROT_READ && base + input.slot >= begin
                && base + input.slot + 8 <= begin + segment->vmsize) slot = true;
        }
        cursor += command->cmdsize;
    }
    if (!slot) throw std::runtime_error("Destination loader slot is outside the image");
    input.slot += base; input.original += base;
    fromUtf8 = reinterpret_cast<QtString(*)(const char*, int)>(dlsym(RTLD_DEFAULT, "_ZN7QString15fromUtf8_helperEPKci"));
    if (!fromUtf8) throw std::runtime_error("Qt cannot create destination loader input");
    original.store(reinterpret_cast<Producer>(input.original));
    if (!Exchange(input.slot, reinterpret_cast<void*>(input.original), reinterpret_cast<void*>(&Produce)))
        throw std::runtime_error("Destination server loader callback changed");
    unlink(path.c_str());
    std::thread([] {
        while (SourceAlive()) std::this_thread::sleep_for(std::chrono::milliseconds(100));
        // This process was created solely for this source test and has no Edit document.
        // Studio handles SIGTERM without necessarily exiting. The source has
        // completed its own shutdown; this server must not outlive that test.
        std::_Exit(0);
    }).detach();
}
}
