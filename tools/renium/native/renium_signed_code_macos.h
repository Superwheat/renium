// File-backed, ad-hoc signed executable pages. ARM64 macOS validates replacement
// mappings too; an anonymous RX copy of a signed engine page is not sufficient.
#pragma once
#include <CommonCrypto/CommonDigest.h>
#include <mach-o/loader.h>
#include <mach/mach.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <algorithm>
#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>

class ReniumSignedCode {
    int descriptor = -1;
    std::string filename;
    std::size_t page = static_cast<std::size_t>(sysconf(_SC_PAGESIZE));
    std::size_t mappedSize = 0;
    static void BigEndian(std::vector<unsigned char>& bytes, std::size_t offset, std::uint32_t value) {
        for (int i = 0; i < 4; ++i) bytes[offset + i] = static_cast<unsigned char>(value >> (24 - i * 8));
    }
    void Write(const std::vector<unsigned char>& bytes) {
        std::size_t offset = 0;
        while (offset < bytes.size()) {
            const auto count = write(descriptor, bytes.data() + offset, bytes.size() - offset);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0) throw std::runtime_error("Cannot write signed Studio code page");
            offset += static_cast<std::size_t>(count);
        }
    }
public:
    explicit ReniumSignedCode(const std::vector<unsigned char>& instructions) {
        if (instructions.empty() || instructions.size() > page * 2)
            throw std::runtime_error("Invalid Studio code page size");
        mappedSize = (instructions.size() + page - 1) & ~(page - 1);
        std::vector<unsigned char> code(page + mappedSize);
        std::memcpy(code.data() + page, instructions.data(), instructions.size());
        constexpr std::size_t directory = 20, header = 44;
        constexpr char identifier[] = "renium.history";
        const std::size_t hashes = header + sizeof(identifier), count = code.size() / 4096;
        std::vector<unsigned char> signature(directory + hashes + count * CC_SHA256_DIGEST_LENGTH);
        mach_header_64 image{};
        image.magic = MH_MAGIC_64;
#if defined(__aarch64__)
        image.cputype = CPU_TYPE_ARM64; image.cpusubtype = CPU_SUBTYPE_ARM64_ALL;
#else
        image.cputype = CPU_TYPE_X86_64; image.cpusubtype = CPU_SUBTYPE_X86_64_ALL;
#endif
        image.filetype = MH_DYLIB; image.ncmds = 4; image.sizeofcmds = 72 * 2 + 48 + 16;
        image.flags = MH_NOUNDEFS | MH_DYLDLINK;
        std::memcpy(code.data(), &image, sizeof(image));
        std::size_t offset = sizeof(image);
        segment_command_64 segment{};
        segment.cmd = LC_SEGMENT_64; segment.cmdsize = sizeof(segment);
        std::strcpy(segment.segname, "__TEXT"); segment.vmsize = code.size(); segment.filesize = code.size();
        segment.maxprot = VM_PROT_READ | VM_PROT_EXECUTE; segment.initprot = segment.maxprot;
        std::memcpy(code.data() + offset, &segment, sizeof(segment)); offset += sizeof(segment);
        std::strcpy(segment.segname, "__LINKEDIT"); segment.vmaddr = code.size(); segment.vmsize = page;
        segment.fileoff = code.size(); segment.filesize = signature.size();
        segment.maxprot = VM_PROT_READ; segment.initprot = VM_PROT_READ;
        std::memcpy(code.data() + offset, &segment, sizeof(segment)); offset += sizeof(segment);
        dylib_command library{};
        library.cmd = LC_ID_DYLIB; library.cmdsize = 48; library.dylib.name.offset = 24;
        std::memcpy(code.data() + offset, &library, sizeof(library));
        constexpr char libraryName[] = "renium-history.dylib";
        std::memcpy(code.data() + offset + 24, libraryName, sizeof(libraryName)); offset += 48;
        linkedit_data_command link{};
        link.cmd = LC_CODE_SIGNATURE; link.cmdsize = sizeof(link);
        link.dataoff = static_cast<std::uint32_t>(code.size()); link.datasize = static_cast<std::uint32_t>(signature.size());
        std::memcpy(code.data() + offset, &link, sizeof(link));
        BigEndian(signature, 0, 0xfade0cc0); BigEndian(signature, 4, static_cast<std::uint32_t>(signature.size()));
        BigEndian(signature, 8, 1); BigEndian(signature, 16, directory);
        BigEndian(signature, directory, 0xfade0c02);
        BigEndian(signature, directory + 4, static_cast<std::uint32_t>(signature.size() - directory));
        BigEndian(signature, directory + 8, 0x20001); BigEndian(signature, directory + 12, 2); // CS_ADHOC
        BigEndian(signature, directory + 16, static_cast<std::uint32_t>(hashes)); BigEndian(signature, directory + 20, header);
        BigEndian(signature, directory + 28, static_cast<std::uint32_t>(count));
        BigEndian(signature, directory + 32, static_cast<std::uint32_t>(code.size()));
        signature[directory + 36] = CC_SHA256_DIGEST_LENGTH; signature[directory + 37] = 2;
        signature[directory + 39] = 12; // SHA256 hashes of 4 KiB code-signing pages.
        std::memcpy(signature.data() + directory + header, identifier, sizeof(identifier));
        for (std::size_t i = 0; i < count; ++i)
            CC_SHA256(code.data() + i * 4096, 4096, signature.data() + directory + hashes + i * CC_SHA256_DIGEST_LENGTH);
        const auto temporary = std::getenv("TMPDIR");
        filename = std::string(temporary && temporary[0] == '/' ? temporary : "/tmp") + "/renium-history-XXXXXX";
        descriptor = mkstemp(filename.data());
        if (descriptor < 0) throw std::runtime_error("Cannot create signed Studio code page");
        try {
            if (fchmod(descriptor, 0700) != 0) throw std::runtime_error("Cannot prepare executable Studio code page");
            Write(code); Write(signature);
            fsignatures_t signedFile{};
            signedFile.fs_blob_start = reinterpret_cast<void*>(code.size()); signedFile.fs_blob_size = signature.size();
            if (fcntl(descriptor, F_ADDFILESIGS, &signedFile) < 0)
                throw std::runtime_error("macOS rejected the Studio code page signature");
        } catch (...) { close(descriptor); descriptor = -1; unlink(filename.c_str()); throw; }
    }
    ReniumSignedCode(const ReniumSignedCode&) = delete;
    ReniumSignedCode& operator=(const ReniumSignedCode&) = delete;
    ~ReniumSignedCode() { if (descriptor >= 0) { close(descriptor); unlink(filename.c_str()); } }
    void* Map(void* address) const {
        return mmap(address, mappedSize, PROT_READ | PROT_EXEC, MAP_PRIVATE | (address ? MAP_FIXED : 0), descriptor, static_cast<off_t>(page));
    }
};
