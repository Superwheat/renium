//! Host-independent decoding of macOS's 64-byte native identity rows.
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rbx_dom_weak::types::UniqueId;

use crate::studio::native::capture::AmbiguousCaptureUniqueIds;

pub(super) fn decode_identities(rows: &[u8]) -> Result<HashMap<String, UniqueId>> {
    anyhow::ensure!(
        rows.len().is_multiple_of(64),
        "Invalid native identity rows"
    );
    let mut result = HashMap::with_capacity(rows.len() / 64);
    let mut seen = HashSet::with_capacity(result.capacity());
    let mut ambiguous = AmbiguousCaptureUniqueIds {
        nil: 0,
        duplicates: 0,
    };
    for row in rows.chunks_exact(64) {
        let word = |at| u32::from_le_bytes(row[at..at + 4].try_into().unwrap());
        // The getter stores four LE words in display order, not an LE u64.
        let id = UniqueId::new(
            word(12),
            word(8),
            ((u64::from(word(0)) << 32) | u64::from(word(4))) as i64,
        );
        let text = &row[16..];
        let end = text
            .iter()
            .position(|b| *b == 0)
            .context("Unterminated native debug ID")?;
        let key = std::str::from_utf8(&text[..end])?;
        anyhow::ensure!(
            end > 0
                && text[end..].iter().all(|b| *b == 0)
                && result.insert(key.into(), id).is_none(),
            "Native identity capture contains an invalid or duplicate identity"
        );
        if id.is_nil() {
            ambiguous.nil += 1;
        } else if !seen.insert(id) {
            ambiguous.duplicates += 1;
        }
    }
    // Validate every row before allowing fallback; ambiguity must never mask
    // corrupt debug IDs. Without parent rows macOS cannot qualify reused IDs.
    if ambiguous.nil > 0 || ambiguous.duplicates > 0 {
        return Err(ambiguous.into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::native::capture::{
        CaptureUnavailable, capture_join_error, export_with_fallback,
    };
    use crate::studio::native::serializer::probes;

    fn row(serial: u32, debug_id: &str) -> [u8; 64] {
        let mut row = [0; 64];
        if serial != 0 {
            for (offset, word) in [(0, 0x12345678u32), (4, 0x87654321), (8, 10), (12, serial)] {
                row[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        row[16..16 + debug_id.len()].copy_from_slice(debug_id.as_bytes());
        row
    }

    #[test]
    fn unique_rows_preserve_native_word_order_and_debug_id_mapping() {
        let parsed = decode_identities(&[row(20, "0_42"), row(21, "0_41")].concat()).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["0_42"], UniqueId::new(20, 10, 0x1234567887654321));
        assert_eq!(parsed["0_41"], UniqueId::new(21, 10, 0x1234567887654321));
    }

    #[test]
    fn nil_and_duplicate_rows_fall_back_once_with_visible_counts() {
        for (serials, nil, duplicates) in [
            (vec![1, 0], 1, 0),
            (vec![1, 0, 0], 2, 0),
            (vec![1, 2, 2, 2], 0, 2),
            (vec![1, 0, 0, 2, 2], 2, 1),
        ] {
            let rows = serials
                .iter()
                .enumerate()
                .flat_map(|(index, serial)| row(*serial, &format!("0_{index}")))
                .collect::<Vec<_>>();
            let error = decode_identities(&rows).unwrap_err();
            let ambiguity = error.downcast_ref::<AmbiguousCaptureUniqueIds>().unwrap();
            assert_eq!((ambiguity.nil, ambiguity.duplicates), (nil, duplicates));

            // Separate evidence from both Windows join tests and live probes.
            let pid = u32::MAX - 1;
            let mut modes = Vec::new();
            let (identities, unavailable) = export_with_fallback(
                |native| {
                    modes.push(native);
                    if native {
                        decode_identities(&rows).map(Some).map_err(|error| {
                            capture_join_error(pid, 1, error.context("identity worker"))
                        })
                    } else {
                        // The plugin export keeps its validated identity carriers.
                        Ok(None)
                    }
                },
                |unavailable| assert_eq!(unavailable.attempts, 1),
            )
            .unwrap();
            assert_eq!(modes, [true, false]);
            assert!(identities.is_none());
            let unavailable = unavailable.unwrap();
            assert!(unavailable.reason.contains(&format!("{nil} nil instances")));
            assert!(
                unavailable
                    .reason
                    .contains(&format!("{duplicates} duplicate instances"))
            );
            let status = probes::cached(pid);
            let capture = status["operations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["operation"] == "capture")
                .unwrap();
            assert_eq!(capture["verdict"], "degraded");
            assert_eq!(capture["reason"], unavailable.reason);
        }
    }

    #[test]
    fn malformed_rows_remain_errors_even_after_ambiguous_unique_ids() {
        for serial in [0, 2] {
            let ambiguous = [row(1, "root"), row(serial, "left"), row(serial, "right")].concat();
            let valid = row(3, "last");
            let mut empty = valid;
            empty[16..].fill(0);
            let mut unterminated = valid;
            unterminated[16..].fill(b'x');
            let mut padding = valid;
            padding[63] = 1;
            let mut invalid_utf8 = valid;
            invalid_utf8[16] = 0xff;
            for suffix in [
                empty.to_vec(),
                unterminated.to_vec(),
                padding.to_vec(),
                invalid_utf8.to_vec(),
                row(3, "left").to_vec(),
                valid[..63].to_vec(),
            ] {
                let rows = [ambiguous.as_slice(), &suffix].concat();
                let error = export_with_fallback(
                    |native| {
                        assert!(native, "malformed rows must never restart the export");
                        decode_identities(&rows)
                            .map_err(|error| capture_join_error(u32::MAX - 2, 1, error))
                    },
                    |_| panic!("malformed rows must not fall back"),
                )
                .unwrap_err();
                assert!(!error.is::<CaptureUnavailable>());
                assert!(!error.is::<AmbiguousCaptureUniqueIds>());
            }
        }
    }
}
