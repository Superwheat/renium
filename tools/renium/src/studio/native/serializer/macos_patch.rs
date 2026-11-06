//! Signing changes the launcher's signature and __LINKEDIT sizes. Compare all
//! remaining Mach-O bytes, including code, constants, UUID and load commands.
pub(super) fn launcher_payload(bytes: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    if bytes.len() > 4 * 1024 * 1024 || u32_at(0)? != 0xfeed_facf {
        return None;
    }
    let end = 32usize.checked_add(u32_at(20)? as usize)?;
    let mut cursor = 32;
    let mut signature = None;
    let mut linkedit = None;
    for _ in 0..u32_at(16)? {
        let command = u32_at(cursor)?;
        let size = u32_at(cursor + 4)? as usize;
        if size < 8 || cursor.checked_add(size)? > end || end > bytes.len() {
            return None;
        }
        if command == 0x19
            && size >= 72
            && bytes.get(cursor + 8..cursor + 24)? == b"__LINKEDIT\0\0\0\0\0\0"
            && linkedit.replace(cursor).is_some()
        {
            return None;
        }
        if command == 0x1d && (size != 16 || signature.replace(cursor).is_some()) {
            return None;
        }
        cursor += size;
    }
    if cursor != end {
        return None;
    }
    let signature = signature?;
    let start = u32_at(signature + 8)? as usize;
    let size = u32_at(signature + 12)? as usize;
    if start < end || start.checked_add(size)? != bytes.len() {
        return None;
    }
    let mut payload = bytes[..start].to_vec();
    payload[signature + 8..signature + 16].fill(0);
    let linkedit = linkedit?;
    payload[linkedit + 32..linkedit + 40].fill(0);
    payload[linkedit + 48..linkedit + 56].fill(0);
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(signature_len: u32) -> Vec<u8> {
        let mut bytes = vec![0; 160 + signature_len as usize];
        for (at, value) in [
            (0, 0xfeed_facfu32),
            (16, 2),
            (20, 88),
            (32, 0x19),
            (36, 72),
            (64, 4096),
            (80, 160 + signature_len),
            (104, 0x1d),
            (108, 16),
            (112, 160),
            (116, signature_len),
        ] {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[40..56].copy_from_slice(b"__LINKEDIT\0\0\0\0\0\0");
        bytes[128..144].copy_from_slice(b"verified payload");
        bytes
    }
    #[test]
    fn signing_can_change_only_signature_and_linkedit_sizes() {
        let original = image(16);
        let mut signed = image(128);
        signed[64..68].copy_from_slice(&8192u32.to_le_bytes());
        signed[160..].fill(0xaa);
        assert_eq!(launcher_payload(&original), launcher_payload(&signed));
        for at in [4, 24, 128, 143] {
            let mut tampered = signed.clone();
            tampered[at] ^= 1;
            assert_ne!(launcher_payload(&original), launcher_payload(&tampered));
        }
        signed[112..116].copy_from_slice(&32u32.to_le_bytes());
        assert!(launcher_payload(&signed).is_none());
        assert!(launcher_payload(b"not a Mach-O").is_none());
    }
}
