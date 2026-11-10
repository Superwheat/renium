#include <array>
#include <cassert>
#include "../renium_signal_append.h"

static void* expectedSignal;
static void* expectedNode;
static unsigned calls;

static void __fastcall AppendNode(void* signal, void* node) {
    assert(signal == expectedSignal && node == expectedNode);
    ++calls;
    InterlockedIncrement(static_cast<LONG*>(node));
}

static void* __fastcall AppendConnection(void* signal, void** output, void* node) {
    AppendNode(signal, node);
    *output = node;
    InterlockedIncrement(static_cast<LONG*>(node) + 1);
    return output;
}

int main() {
    std::array<LONG, 18> node{};
    int signal = 0;
    expectedSignal = &signal;
    expectedNode = node.data();
    for (unsigned abi = 0; abi != 2; ++abi) {
        node.fill(0);
        node[1] = 1; // allocation's existing weak reference
        void* connection = nullptr;
        const auto function = abi == 0 ? reinterpret_cast<std::uintptr_t>(&AppendNode)
                                      : reinterpret_cast<std::uintptr_t>(&AppendConnection);
        AppendSignal(abi, function, &signal, reinterpret_cast<unsigned char*>(node.data()), &connection);
        assert(connection == node.data() && node[0] == 1 && node[1] == 2);
        assert(calls == abi + 1);
        // Releasing the connection must return the initial weak count exactly.
        assert(InterlockedDecrement(static_cast<LONG*>(connection) + 1) == 1);
    }
    bool refused = false;
    try { AppendSignal(2, 0, nullptr, nullptr, nullptr); }
    catch (const std::runtime_error&) { refused = true; }
    assert(refused && calls == 2);
}
