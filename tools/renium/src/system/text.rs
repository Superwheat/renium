pub(crate) fn normalized_source_bytes(bytes: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut index = 0;
    std::iter::from_fn(move || {
        let byte = *bytes.get(index)?;
        if byte == b'\r' {
            index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
        }
        index += 1;
        Some(if byte == b'\r' { b'\n' } else { byte })
    })
}
