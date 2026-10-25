#include <algorithm>
#include <cassert>
#include <cstdio>
#include <sys/mman.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include "../renium_studio_history.h"
#include "../renium_playtest_macos.h"
#include "../renium_test_client_macos.h"

using namespace renium_mac_test_client;
int main() {
    renium_mac_test_client::Request r{};
    r.table = 0x123456789abcdef0;
    r.baseTable = 0x23456789abcdef01;
    r.baseOffset = 120;
    mach_vm_address_t reserved = 0;
    constexpr auto size = 8ull * 1024 * 1024 * 1024;
    assert(mach_vm_allocate(mach_task_self(), &reserved, size, VM_FLAGS_ANYWHERE) == KERN_SUCCESS);
    // An object in a custom allocator's sparse arena, crossing a read boundary.
    auto object = reinterpret_cast<unsigned char*>(reserved + size - vm_page_size - 8);
    std::memcpy(object, &r.table, 8);
    std::memcpy(object + r.baseOffset, &r.baseTable, 8);
    assert(Opener(r) == object);
    auto duplicate = std::make_unique<unsigned char[]>(256);
    std::memcpy(duplicate.get(), object, r.baseOffset + 8);
    asm volatile("" : : "r"(duplicate.get()) : "memory");
    bool rejected = false;
    try { Opener(r); } catch (const std::runtime_error&) { rejected = true; }
    assert(rejected);
    std::memset(duplicate.get(), 0, 256);
    asm volatile("" : : "r"(duplicate.get()) : "memory");
    duplicate.reset();
    assert(Opener(r) == object);
    assert(mach_vm_deallocate(mach_task_self(), reserved, size) == KERN_SUCCESS);
    rejected = false;
    try { Opener(r); } catch (const std::runtime_error&) { rejected = true; }
    assert(rejected);
    // Process-owned C++ objects may also reside in shared mappings.
    auto shared = mmap(nullptr, 65536, PROT_READ | PROT_WRITE, MAP_ANON | MAP_SHARED, -1, 0);
    assert(shared != MAP_FAILED);
    std::memcpy(shared, &r.table, 8);
    std::memcpy(static_cast<unsigned char*>(shared) + r.baseOffset, &r.baseTable, 8);
    assert(Opener(r) == shared);
    assert(munmap(shared, 65536) == 0);
    std::puts("Mac client owner: sparse VM, shared mappings, custom allocator, boundary, ambiguity and unmapped owner passed");
}
