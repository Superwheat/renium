export function reniumPluginReleaseUrl(version: string, assetName = "Renium.rbxm"): string {
  const normalized = version.trim();
  if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(normalized)) {
    throw new Error(`Invalid Renium extension version: ${version}`);
  }
  return `https://github.com/Superwheat/renium/releases/download/v${normalized}/${assetName}`;
}

export function isRobloxModel(bytes: Uint8Array): boolean {
  return bytes.length >= 16
    && bytes[0] === 0x3c
    && bytes[1] === 0x72
    && bytes[2] === 0x6f
    && bytes[3] === 0x62
    && bytes[4] === 0x6c
    && bytes[5] === 0x6f
    && bytes[6] === 0x78;
}
