#pragma once
#include <array>
#include <cstdint>
#include <map>
#include <optional>
#include <set>
#include <utility>
#include <vector>

// Bounded ARM64 data flow for the queue's forwarding method and the exact
// inline, nontrivial callback contract we supply. Unknown instructions or
// branches fail closed. Register allocation, stack frames, field placement and
// branch/call displacement are decoded, never tied to a Studio version.
namespace renium_task_context {
enum class Kind { Unknown, Argument, Stack, Field, Tag, Table, Method, Constant };
struct Value {
    Kind kind = Kind::Unknown;
    std::int64_t offset = 0;
    unsigned argument = 0;
    bool operator==(const Value& other) const { return kind == other.kind && offset == other.offset && argument == other.argument; }
    bool operator!=(const Value& other) const { return !(*this == other); }
    bool operator<(const Value& other) const {
        return std::make_pair(static_cast<int>(kind), std::make_pair(argument, offset)) <
            std::make_pair(static_cast<int>(other.kind), std::make_pair(other.argument, other.offset));
    }
};
inline Value Arg(unsigned n) { return {Kind::Argument, 0, n}; }
inline Value Constant(std::int64_t n) { return {Kind::Constant, n, 0}; }
inline Value Add(Value value, std::int64_t n) {
    if (value.kind != Kind::Argument && value.kind != Kind::Stack) return {};
    value.offset += n; return value;
}
struct Call { std::uintptr_t target; Value method; std::array<Value, 3> args; };
struct Flow {
    std::array<Value, 32> regs{};
    std::map<Value, Value> memory;
    std::vector<Call> calls;
    std::optional<bool> equal, lower;
    bool returned = false;
    Flow() { for (unsigned i = 0; i < 8; ++i) regs[i] = Arg(i); regs[31] = {Kind::Stack, 0, 0}; }
    Value Reg(unsigned r) const { return r == 31 ? Constant(0) : regs[r]; }
    Value Load(Value at) const {
        const auto found = memory.find(at);
        if (found != memory.end()) return found->second;
        if (at.kind == Kind::Argument) return {Kind::Field, at.offset, at.argument};
        return {};
    }
    void CallAt(std::uintptr_t target, Value method) {
        calls.push_back({target, method, {regs[0], regs[1], regs[2]}});
        for (unsigned i = 0; i <= 18; ++i) regs[i] = {};
        equal.reset(); lower.reset();
    }
};
inline std::int64_t Signed(std::uint32_t value, unsigned bits) {
    return (static_cast<std::int64_t>(value) ^ (std::int64_t{1} << (bits - 1))) - (std::int64_t{1} << (bits - 1));
}
inline bool Run(const std::vector<std::uint32_t>& words, std::uintptr_t address, Flow& f) {
    std::size_t pc = 0;
    for (unsigned budget = 0; budget < 256 && pc < words.size(); ++budget) {
        const auto w = words[pc]; const unsigned d = w & 31, n = (w >> 5) & 31, m = (w >> 16) & 31;
        const auto jump = [&](std::int64_t delta) -> bool {
            const auto next = static_cast<std::int64_t>(pc) + delta;
            if (next <= static_cast<std::int64_t>(pc) || next >= static_cast<std::int64_t>(words.size())) return false;
            pc = static_cast<std::size_t>(next); return true;
        };
        if (w == 0xd503201f) { ++pc; continue; }
        if (w == 0xd65f03c0) { f.returned = true; return true; }
        if ((w & 0xfffffc1f) == 0xd61f0000 || (w & 0xfffffc1f) == 0xd63f0000) {
            const auto method = f.Reg(n);
            if (method.kind != Kind::Method) return false;
            f.CallAt(0, method);
            if ((w & 0xfffffc1f) == 0xd61f0000) { f.returned = true; return true; }
        } else if ((w & 0xfc000000) == 0x94000000) {
            f.CallAt(address + pc * 4 + Signed(w & 0x3ffffff, 26) * 4, {});
        } else if ((w & 0xfc000000) == 0x14000000) {
            if (!jump(Signed(w & 0x3ffffff, 26))) return false;
            continue;
        } else if ((w & 0xff000010) == 0x54000000) {
            std::optional<bool> take;
            switch (w & 15) {
            case 0: take = f.equal; break;
            case 1: if (f.equal) take = !*f.equal; break;
            case 3: take = f.lower; break;
            default: return false;
            }
            if (!take) return false;
            if (*take) { if (!jump(Signed((w >> 5) & 0x7ffff, 19))) return false; continue; }
        } else if ((w & 0x7e000000) == 0x34000000) {
            const auto value = f.Reg(d);
            if (value.kind != Kind::Method && value.kind != Kind::Tag && value.kind != Kind::Constant) return false;
            const bool nonzero = value.kind != Kind::Constant || value.offset != 0;
            if (nonzero == static_cast<bool>(w & 0x1000000)) {
                if (!jump(Signed((w >> 5) & 0x7ffff, 19))) return false;
                continue;
            }
        } else if ((w & 0x7e000000) == 0x36000000) {
            const auto v = f.Reg(d); const unsigned bit = ((w >> 19) & 31) | ((w >> 26) & 32);
            if (v.kind != Kind::Tag || (bit != 1 && bit != 2)) return false;
            if (w & 0x1000000) { if (!jump(Signed((w >> 5) & 0x3fff, 14))) return false; continue; }
        } else if ((w & 0xffe0ffe0) == 0xaa0003e0) {
            f.regs[d] = f.Reg(m);
        } else if ((w & 0xffe0fc1f) == 0xeb00001f) {
            const auto a = f.Reg(n), b = f.Reg(m);
            if (a.kind != Kind::Argument || b.kind != Kind::Argument) return false;
            f.equal = a == b; f.lower.reset();
        } else if ((w & 0xff80001f) == 0xf100001f) {
            const auto a = f.Reg(n); const auto imm = (w >> 10) & 4095;
            if (a.kind != Kind::Tag || imm != 8) return false;
            f.equal = false; f.lower = false;
        } else if ((w & 0xffc00000) == 0x91000000 || (w & 0xffc00000) == 0xd1000000) {
            const auto imm = static_cast<std::int64_t>((w >> 10) & 4095);
            f.regs[d] = Add(f.regs[n], (w & 0x40000000) ? -imm : imm);
        } else if ((w & 0xfffffc00) == 0x927df000) { // AND Xd, Xn, ~7
            if (f.Reg(n).kind != Kind::Tag) return false;
            f.regs[d] = {Kind::Table, 0, f.Reg(n).argument};
        } else if ((w & 0xffc00000) == 0xf9400000 || (w & 0xffc00000) == 0xf9000000) {
            const auto offset = static_cast<std::int64_t>((w >> 10) & 4095) * 8;
            if (w & 0x400000) {
                if (f.regs[n].kind == Kind::Table) f.regs[d] = {Kind::Method, offset, f.regs[n].argument};
                else f.regs[d] = f.Load(Add(f.regs[n], offset));
            } else {
                const auto at = Add(f.regs[n], offset);
                if (at.kind == Kind::Unknown) return false;
                f.memory[at] = f.Reg(d);
            }
        } else if ((w & 0xffc0001f) == 0x3900001f) {
            const auto at = Add(f.regs[n], (w >> 10) & 4095);
            if (at.kind != Kind::Stack) return false;
            f.memory[at] = Constant(0);
        } else if ((w & 0x3a000000) == 0x28000000 && (w >> 30) == 2 && !(w & 0x04000000)) {
            const unsigned mode = (w >> 23) & 3, second = (w >> 10) & 31;
            const auto offset = Signed((w >> 15) & 127, 7) * 8;
            const auto at = Add(f.regs[n], mode == 1 ? 0 : offset);
            if (at.kind != Kind::Stack) return false;
            if (w & 0x400000) { f.regs[d] = f.Load(at); f.regs[second] = f.Load(Add(at, 8)); }
            else { f.memory[at] = f.Reg(d); f.memory[Add(at, 8)] = f.Reg(second); }
            if (mode == 1 || mode == 3) f.regs[n] = Add(f.regs[n], offset);
        } else return false;
        ++pc;
    }
    return false;
}
inline bool TaggedMove(const std::vector<std::uint32_t>& words) {
    Flow f; const Value tag{Kind::Tag, 0, 1}; f.memory[Add(Arg(1), 24)] = tag;
    if (!Run(words, 0, f) || f.calls.size() != 2) return false;
    const auto& move = f.calls[0]; const auto& destroy = f.calls[1];
    return move.method == Value{Kind::Method, 16, 1} && move.args[0] == Arg(0) && move.args[1] == Arg(1) &&
        destroy.method == Value{Kind::Method, 24, 1} && destroy.args[0] == Arg(1) &&
        f.Load(Add(Arg(0), 24)) == tag && f.Load(Add(Arg(1), 24)) == Constant(0);
}
inline bool TaggedDestroy(const std::vector<std::uint32_t>& words) {
    Flow f; f.memory[Add(Arg(0), 24)] = {Kind::Tag, 0, 0};
    return Run(words, 0, f) && f.calls.size() == 1 &&
        f.calls[0].method == Value{Kind::Method, 24, 0} && f.calls[0].args[0] == Arg(0) &&
        f.Load(Add(Arg(0), 24)) == Constant(0);
}
struct Dispatch { std::size_t modelOffset; std::uintptr_t move, queue, destroy; };
inline std::optional<Dispatch> ManagerDispatch(const std::vector<std::uint32_t>& words, std::uintptr_t address) {
    Flow f;
    if (!Run(words, address, f) || f.calls.size() != 3) return {};
    const auto& move = f.calls[0]; const auto& queue = f.calls[1]; const auto& destroy = f.calls[2];
    const auto model = queue.args[0]; const auto callback = move.args[0];
    if (!move.target || !queue.target || !destroy.target || move.args[1] != Arg(1) || callback.kind != Kind::Stack ||
        queue.args[1] != callback || queue.args[2] != Arg(2) || destroy.args[0] != callback ||
        model.kind != Kind::Field || model.argument != 0 || model.offset < 8 || model.offset % 8 ||
        f.Load(Add(callback, 24)) != Constant(0)) return {};
    return Dispatch{static_cast<std::size_t>(model.offset), move.target, queue.target, destroy.target};
}
inline bool ModelDispatch(const std::vector<std::uint32_t>& words, const Dispatch& dispatch) {
    Flow f;
    if (!Run(words, dispatch.queue, f) || f.calls.size() != 6) return false;
    const auto a = f.calls[0].args[0], b = f.calls[1].args[0];
    return a.kind == Kind::Stack && b.kind == Kind::Stack && a != b &&
        f.calls[0].target == dispatch.move && f.calls[0].args[1] == Arg(1) &&
        f.calls[1].target == dispatch.move && f.calls[1].args[1] == a &&
        f.calls[2].target && f.calls[2].args[0] == Arg(0) && f.calls[2].args[1] == b && f.calls[2].args[2] == Arg(2) &&
        f.calls[3].target == dispatch.destroy && f.calls[3].args[0] == Add(b, 32) &&
        f.calls[4].target == dispatch.destroy && f.calls[4].args[0] == b &&
        f.calls[5].target == dispatch.destroy && f.calls[5].args[0] == a &&
        f.Load(Add(a, 24)) == Constant(0) && f.Load(Add(b, 24)) == Constant(0) && f.Load(Add(b, 56)) == Constant(0);
}

template<class Read, class Object, class Code>
std::optional<std::uintptr_t> ContextForModel(std::uintptr_t instance, std::uintptr_t model, std::uintptr_t owner,
    std::size_t prefix, std::size_t managerOffset, std::size_t slot, Read read, Object object, Code code) {
    if (prefix > 4096 || managerOffset < 8 || managerOffset > 4096 || slot > 4096) return {};
    std::set<std::uintptr_t> matches;
    for (std::size_t field = 24; field + 8 <= prefix; field += 8) {
        std::uintptr_t context = 0, manager = 0, table = 0, method = 0, boundModel = 0, boundOwner = 0;
        std::vector<std::uint32_t> words;
        if (!read(instance + field, context)) continue;
        context &= ~std::uintptr_t{7};
        if (!object(context, "N3RBX13EngineContextE") || !read(context + managerOffset, manager) ||
            !object(manager, "N3RBX16DataModelManagerE") || !read(manager, table) ||
            !read(table + slot, method) || !code(method, words)) continue;
        const auto dispatch = ManagerDispatch(words, method);
        if (!dispatch || !read(manager + dispatch->modelOffset, boundModel) || boundModel != model ||
            !read(manager + dispatch->modelOffset + 8, boundOwner) || boundOwner != owner ||
            !code(dispatch->queue, words) || !ModelDispatch(words, *dispatch) ||
            !code(dispatch->move, words) || !TaggedMove(words) ||
            !code(dispatch->destroy, words) || !TaggedDestroy(words)) continue;
        matches.insert(context);
    }
    if (matches.size() != 1) return {};
    return *matches.begin();
}
}
