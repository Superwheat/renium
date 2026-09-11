// SmoothGrid v1 stores 32^3 cells in X/Z/Y order. Index compressed chunks;
// expand only the chunk being reconciled, so sparse worlds stay sparse.
#pragma once
#include <algorithm>
#include <array>
#include <cstdint>
#include <limits>
#include <map>
#include <memory>
#include <stdexcept>
#include <string>
#include <string_view>

namespace renium_terrain {
using Coordinate = std::array<std::int32_t, 3>;
using Cells = std::array<unsigned char, 32768 * 3>;
using Chunks = std::map<Coordinate, std::string_view>;

class Reader {
    std::string_view bytes;
public:
    std::size_t offset = 0;
    explicit Reader(std::string_view input) : bytes(input) {}
    unsigned char Byte() {
        if (offset == bytes.size()) throw std::runtime_error("Truncated Terrain SmoothGrid");
        return static_cast<unsigned char>(bytes[offset++]);
    }
    bool Done() const { return offset == bytes.size(); }
};

inline void ReadChunk(Reader& reader, Cells* output) {
    std::size_t cell = 0;
    while (cell < 32768) {
        const auto tag = reader.Byte();
        const auto material = static_cast<unsigned char>(tag & 63);
        const auto solid = static_cast<unsigned char>((tag & 64) ? reader.Byte() : (material ? 255 : 0));
        unsigned char liquid = 0;
        std::size_t count = 1;
        if (tag & 128) {
            const auto repeat = reader.Byte();
            if (repeat) count += repeat;
            else liquid = reader.Byte();
        }
        if (count > 32768 - cell) throw std::runtime_error("Terrain run exceeds its chunk");
        if (output) {
            for (std::size_t end = cell + count; cell < end; ++cell) {
                (*output)[cell * 3] = material;
                (*output)[cell * 3 + 1] = solid;
                (*output)[cell * 3 + 2] = liquid;
            }
        } else cell += count;
    }
}

inline Chunks Index(std::string_view bytes) {
    Chunks chunks;
    if (bytes.empty()) return chunks;
    Reader reader(bytes);
    if (reader.Byte() != 1 || reader.Byte() != 5)
        throw std::runtime_error("Unsupported Terrain SmoothGrid version or chunk size");
    Coordinate previous{};
    while (!reader.Done()) {
        std::array<std::uint32_t, 3> delta{};
        for (int byte = 0; byte < 4; ++byte)
            for (int axis = 0; axis < 3; ++axis)
                delta[axis] = (delta[axis] << 8) | reader.Byte();
        Coordinate coordinate{};
        for (int axis = 0; axis < 3; ++axis) {
            const auto signedDelta = static_cast<std::int64_t>(delta[axis]) -
                ((delta[axis] & 0x80000000u) ? 0x100000000ll : 0);
            const auto next = previous[axis] + signedDelta;
            if (next < (std::numeric_limits<std::int32_t>::min)() || next > (std::numeric_limits<std::int32_t>::max)())
                throw std::runtime_error("Terrain chunk coordinate overflow");
            coordinate[axis] = static_cast<std::int32_t>(next);
        }
        const auto start = reader.offset;
        ReadChunk(reader, nullptr);
        if (!chunks.emplace(coordinate, bytes.substr(start, reader.offset - start)).second)
            throw std::runtime_error("Duplicate Terrain chunk");
        previous = coordinate;
    }
    return chunks;
}

inline std::string_view Find(const Chunks& chunks, const Coordinate& coordinate) {
    const auto found = chunks.find(coordinate);
    return found == chunks.end() ? std::string_view{} : found->second;
}

inline void Decode(std::string_view raw, Cells& cells) {
    if (raw.empty()) cells.fill(0);
    else {
        Reader reader(raw);
        ReadChunk(reader, &cells);
        if (!reader.Done()) throw std::runtime_error("Trailing Terrain chunk bytes");
    }
}

inline std::string Encode(const Cells& cells) {
    std::string bytes;
    for (std::size_t cell = 0; cell < 32768;) {
        const auto at = cell * 3;
        const auto material = cells[at], solid = cells[at + 1], liquid = cells[at + 2];
        std::size_t count = 1;
        while (!liquid && count < 256 && cell + count < 32768 &&
            cells[at + count * 3] == material && cells[at + count * 3 + 1] == solid &&
            cells[at + count * 3 + 2] == liquid) ++count;
        const bool explicitSolid = solid != (material ? 255 : 0);
        bytes.push_back(static_cast<char>(material | (explicitSolid ? 64 : 0) | ((count > 1 || liquid) ? 128 : 0)));
        if (explicitSolid) bytes.push_back(static_cast<char>(solid));
        if (liquid) { bytes.push_back(0); bytes.push_back(static_cast<char>(liquid)); }
        else if (count > 1) bytes.push_back(static_cast<char>(count - 1));
        cell += count;
    }
    return bytes;
}

inline void Append(std::string& output, const Coordinate& coordinate, Coordinate& previous, std::string_view chunk) {
    std::array<std::uint32_t, 3> delta{};
    for (int axis = 0; axis < 3; ++axis) {
        const auto value = static_cast<std::int64_t>(coordinate[axis]) - previous[axis];
        if (value < (std::numeric_limits<std::int32_t>::min)() || value > (std::numeric_limits<std::int32_t>::max)())
            throw std::runtime_error("Terrain chunk delta overflow");
        delta[axis] = static_cast<std::uint32_t>(value);
    }
    for (int shift = 24; shift >= 0; shift -= 8)
        for (int axis = 0; axis < 3; ++axis)
            output.push_back(static_cast<char>(delta[axis] >> shift));
    output.append(chunk);
    previous = coordinate;
}

// Undo only our changed cells that still contain our written value. Preserve
// newer painting, including liquid occupancy in cells that also contain solids.
inline std::string Rollback(std::string_view before, std::string_view written, std::string_view current) {
    const auto old = Index(before), own = Index(written), live = Index(current);
    if (current == written) return std::string(before);
    if (before == written) return std::string(current);
    std::map<Coordinate, std::string> replacements;
    Chunks affected = old;
    affected.insert(own.begin(), own.end());
    auto cells = std::make_unique<std::array<Cells, 3>>();
    auto& oldCells = (*cells)[0];
    auto& ownCells = (*cells)[1];
    auto& liveCells = (*cells)[2];
    for (const auto& [coordinate, unused] : affected) {
        (void)unused;
        const auto original = Find(old, coordinate), applied = Find(own, coordinate);
        if (original == applied) continue;
        Decode(original, oldCells);
        Decode(applied, ownCells);
        Decode(Find(live, coordinate), liveCells);
        bool changed = false;
        for (std::size_t at = 0; at < liveCells.size(); at += 3) {
            const bool ownChanged = oldCells[at] != ownCells[at] || oldCells[at + 1] != ownCells[at + 1] || oldCells[at + 2] != ownCells[at + 2];
            if (ownChanged && liveCells[at] == ownCells[at] && liveCells[at + 1] == ownCells[at + 1] && liveCells[at + 2] == ownCells[at + 2]) {
                for (int channel = 0; channel < 3; ++channel) liveCells[at + channel] = oldCells[at + channel];
                changed = true;
            }
        }
        if (changed) {
            const bool empty = std::all_of(liveCells.begin(), liveCells.end(), [](unsigned char value) { return value == 0; });
            replacements.emplace(coordinate, empty ? std::string{} : Encode(liveCells));
        }
    }
    if (replacements.empty()) return std::string(current);
    Chunks result = live;
    for (const auto& [coordinate, raw] : replacements) {
        if (raw.empty()) result.erase(coordinate);
        else result[coordinate] = raw;
    }
    std::string output("\1\5", 2);
    Coordinate previous{};
    for (const auto& [coordinate, raw] : result) Append(output, coordinate, previous, raw);
    return output;
}
}
