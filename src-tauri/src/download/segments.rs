use std::path::{Path, PathBuf};

/// Splits `total_bytes` into `segment_count` contiguous, inclusive byte
/// ranges of as-even-as-possible size. Any remainder left over from the
/// integer division is absorbed entirely by the last segment.
///
/// `segment_count` is clamped to at most one segment per byte: a segment
/// can never cover a zero-byte range, since `(start, end)` has no valid
/// inclusive representation for an empty range in `u64` (computing `end`
/// would underflow). So when `total_bytes < segment_count` (e.g. a small
/// file downloaded with a fixed multi-segment default), fewer, larger
/// segments are returned instead -- callers should use the length of the
/// returned `Vec`, not the original `segment_count`, as the true number
/// of segments. Returns an empty `Vec` if there is nothing to split
/// (`total_bytes == 0` or `segment_count == 0`).
pub fn plan_segments(total_bytes: u64, segment_count: usize) -> Vec<(u64, u64)> {
    if total_bytes == 0 || segment_count == 0 {
        return Vec::new();
    }

    let segment_count = (segment_count as u64).min(total_bytes);
    let base = total_bytes / segment_count;
    let remainder = total_bytes % segment_count;

    let mut ranges = Vec::with_capacity(segment_count as usize);
    let mut start: u64 = 0;

    for index in 0..segment_count {
        let size = if index == segment_count - 1 {
            base + remainder
        } else {
            base
        };
        let end = start + size - 1;
        ranges.push((start, end));
        start = end + 1;
    }

    ranges
}

/// Returns the path of the `index`-th segment's partial-download file
/// alongside `dest`, e.g. `dest.part0`.
pub fn part_path(dest: &Path, index: usize) -> PathBuf {
    let mut os_string = dest.as_os_str().to_os_string();
    os_string.push(format!(".part{index}"));
    PathBuf::from(os_string)
}

/// Returns how many bytes of a segment have already been downloaded, by
/// checking the size of its part file on disk. Returns 0 if the part file
/// doesn't exist yet (nothing downloaded for that segment so far).
pub fn resume_offset(part_path: &Path) -> u64 {
    std::fs::metadata(part_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}
