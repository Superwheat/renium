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
