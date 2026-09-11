// Build the existing native-setter A/B fixture from current source, without
// maintaining a second copy of the editor implementation.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const bridge = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const source = fs.readFileSync(path.join(bridge, 'BridgeEditorSync.module.lua'), 'utf8');
const call = '\t\tqueueNativeRootWrite(instance, propertyName, rawValue, change, ctx, stats)';
assert.equal(source.split(call).length, 2, 'Native-setter diagnostic needs updating for the current editor implementation');
const output = path.resolve(bridge, '../../audit/release-readiness/no-native-writes-editor.module.lua');
fs.mkdirSync(path.dirname(output), {recursive: true});
fs.writeFileSync(output, '-- DIAGNOSTIC ONLY: skips explicit native root setters.\n' + source.replace(call, '\t\tstats.noops += 1'));
console.log('Prepared diagnostic-no-native.project.json. This fixture intentionally skips native setters; do not install it for ordinary sync.');
