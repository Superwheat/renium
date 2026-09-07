// Exercise the actual macOS ABI guard against an adjacent plausible object.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

if (process.platform === 'win32') process.exit(0);
const here = path.dirname(fileURLToPath(import.meta.url));
const helper = fs.readFileSync(path.join(here, '../native/renium_studio_helper_macos.cpp'), 'utf8');
const start = helper.indexOf('static bool IsPackageSubobject(');
const end = helper.indexOf('static bool ResolvePackagePropertyTargets(', start);
assert(start >= 0 && end > start);
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'renium-package-layout-'));
try {
  const source = path.join(root, 'test.cpp');
  const binary = path.join(root, 'test');
  fs.writeFileSync(source, `
#include <cassert>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <utility>
#include <vector>
static std::vector<std::pair<std::uintptr_t,std::size_t>> regions;
template<class T> static bool ReadValue(std::uintptr_t address, T& output) {
  for (auto region : regions)
    if (address >= region.first && address-region.first <= region.second-sizeof(T)) {
      std::memcpy(&output, reinterpret_cast<void*>(address), sizeof(T)); return true;
    }
  return false;
}
static bool LikelyPointer(std::uintptr_t value) { return value > 0x10000; }
${helper.slice(start, end)}
int main() {
  std::uintptr_t object[128]{}, primary[3]{}, secondary[3]{}, adjacent[3]{};
  for (auto region : {std::pair{object,sizeof(object)}, {primary,sizeof(primary)},
                     {secondary,sizeof(secondary)}, {adjacent,sizeof(adjacent)}})
    regions.push_back({reinterpret_cast<std::uintptr_t>(region.first),region.second});
  const auto link = reinterpret_cast<std::uintptr_t>(object);
  primary[1] = link; object[0] = reinterpret_cast<std::uintptr_t>(primary+2);
  for (std::size_t adjustment : {0x58,0xb0,0x120}) {
    secondary[0] = -adjustment; secondary[1] = primary[1];
    object[adjustment/8] = reinterpret_cast<std::uintptr_t>(secondary+2);
    assert(IsPackageSubobject(link,adjustment));
    adjacent[0] = secondary[0]; adjacent[1] = primary[1];
    object[0x1d0/8] = reinterpret_cast<std::uintptr_t>(adjacent+2);
    assert(!IsPackageSubobject(link,0x1d0)); // Same class, different complete object.
    secondary[1] = link+8; assert(!IsPackageSubobject(link,adjustment));
    object[adjustment/8] = 1; assert(!IsPackageSubobject(link,adjustment));
  }
  assert(!IsPackageSubobject(link,0x1008));
}
`);
  for (const [command, args] of [['c++', ['-std=c++17', source, '-o', binary]], [binary, []]]) {
    const result = spawnSync(command, args, {encoding:'utf8', timeout:20_000});
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }
  console.log('Package subobject guard rejects adjacent objects and follows relocated bases');
} finally {
  fs.rmSync(root, {recursive:true, force:true});
}
