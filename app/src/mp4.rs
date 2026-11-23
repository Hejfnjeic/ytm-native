//! MP4 box reading: the few helpers that find boxes and read big-endian numbers in them.

pub(crate) fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The complete boxes in `b`: type, contents.
pub(crate) fn boxes(b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let size = be32(b, at)? as usize;
        let body = b.get(at + 8..at + size).filter(|_| size >= 8)?;
        let kind = &b[at + 4..at + 8];
        at += size;
        Some((kind, body))
    })
}

/// The first box along `path`.
pub(crate) fn find<'a>(b: &'a [u8], path: &[&str]) -> Option<&'a [u8]> {
    let (head, rest) = path.split_first()?;
    let body = boxes(b).find(|(kind, _)| *kind == head.as_bytes())?.1;
    if rest.is_empty() { Some(body) } else { find(body, rest) }
}
