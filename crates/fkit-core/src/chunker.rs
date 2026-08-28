//! Content-defined chunking (CDC), FastCDC-style.
//!
//! # Why not just split every 8 KiB?
//!
//! Because of the **boundary-shift problem**. Insert one byte at the front of a
//! file and every fixed-size boundary after it moves, so every chunk gets a new
//! hash and you re-upload the entire file:
//!
//! ```text
//!   fixed 4-byte chunks:   [abcd][efgh][ijkl]
//!   insert 'X' at front:   [Xabc][defg][hijk]   <- all three differ
//! ```
//!
//! CDC instead picks boundaries based on a rolling hash of *the content itself*.
//! A boundary lands wherever the last few dozen bytes hash to a value ending in
//! N zero bits. Insert a byte and only the chunk containing it changes; the
//! rolling hash re-synchronises within one window and every later boundary lands
//! in exactly the same place:
//!
//! ```text
//!   content-defined:       [abcd][efgh][ijkl]
//!   insert 'X' at front:   [Xabcd][efgh][ijkl]  <- only the first differs
//! ```
//!
//! That property is the whole reason a 400 MB file edited in the middle costs
//! one chunk to store, not 400 MB. Git has no equivalent: it re-stores the file
//! whole and only claws space back later during `gc` via delta compression.
//!
//! # The gear hash
//!
//! Each byte maps to a random 64-bit value. The rolling state is
//! `fp = (fp << 1) + GEAR[byte]`. Shifting left one bit per byte means a byte's
//! influence falls off the top after 64 bytes, so the fingerprint depends only
//! on a sliding ~64-byte window — with no explicit "remove the old byte" step.

use std::io::Read;

/// Never emit a chunk smaller than this (except the file's last chunk).
/// Guards against pathological input producing millions of tiny objects.
pub const MIN_SIZE: usize = 2 * 1024;
/// The target average. Boundary probability is tuned around this.
pub const AVG_SIZE: usize = 8 * 1024;
/// Hard ceiling: cut here even if no boundary was found.
pub const MAX_SIZE: usize = 64 * 1024;

// "Normalized chunking": use a *stricter* mask (more bits, boundaries rarer)
// before we reach the average size, and a *looser* one after. This pulls the
// chunk-size distribution toward AVG_SIZE instead of the long exponential tail
// a single mask produces. It is the main improvement FastCDC made over the
// original Rabin-based schemes.
// The bit *count* is what sets the expected distance to a boundary: a mask with
// N bits set fires with probability 2^-N per byte. So 15 bits => ~32 KiB
// expected in the strict phase (boundaries are rare, chunks are pushed toward
// the average), and 11 bits => ~2 KiB in the loose phase (boundaries become
// common once we are already past the average).
//
// Getting these counts wrong is subtle and quiet: an over-loose second mask
// fires within a few bytes of AVG_SIZE every time, which pins every chunk to
// almost exactly the average and silently reduces CDC to fixed-size chunking
// with jitter. Both masks are asserted in the tests below for that reason.
//
// The bits sit high in the word because the gear hash shifts left: bit `j`
// depends on roughly the last `j` bytes, so high bits mean a wide, well-mixed
// window.
const MASK_STRICT: u64 = 0x007F_FF00_0000_0000; // 15 bits, ~1/32768 per byte
const MASK_LOOSE: u64 = 0x007F_F000_0000_0000; // 11 bits, ~1/2048  per byte

/// 256 pseudorandom values, one per byte, generated at compile time with
/// splitmix64. Deterministic and baked into the binary: the table is part of
/// fkit's on-disk format, because changing it changes every chunk boundary.
const fn gear_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut state: u64 = 0x243F_6A88_85A3_08D3; // digits of pi, why not
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        t[i] = z;
        i += 1;
    }
    t
}

static GEAR: [u64; 256] = gear_table();

/// Find the length of the first chunk in `buf`.
///
/// `buf` should hold at least `MAX_SIZE` bytes unless it is the tail of the
/// file. Returns a length in `1..=buf.len()`.
pub fn cut_point(buf: &[u8]) -> usize {
    let n = buf.len();
    if n <= MIN_SIZE {
        return n;
    }

    let mut fp: u64 = 0;
    let mut i = MIN_SIZE; // skip: we refuse to cut before MIN_SIZE anyway

    // Phase 1 — strict mask, up to the average size.
    let normal = AVG_SIZE.min(n);
    while i < normal {
        fp = (fp << 1).wrapping_add(GEAR[buf[i] as usize]);
        if fp & MASK_STRICT == 0 {
            return i + 1;
        }
        i += 1;
    }

    // Phase 2 — loose mask, from average up to the hard cap.
    let limit = MAX_SIZE.min(n);
    while i < limit {
        fp = (fp << 1).wrapping_add(GEAR[buf[i] as usize]);
        if fp & MASK_LOOSE == 0 {
            return i + 1;
        }
        i += 1;
    }

    limit
}

/// How much to buffer ahead. Compaction cost is paid once per bufferful rather
/// than once per chunk, so a larger window means proportionally less memmove.
const BUF_TARGET: usize = 1024 * 1024;

/// Streams a reader and yields content-defined chunks.
///
/// Streaming matters: we must be able to ingest a file larger than RAM, so only
/// a bounded window is held at a time.
pub struct Chunker<R: Read> {
    reader: R,
    buf: Vec<u8>,
    /// Offset of unconsumed data within `buf`.
    start: usize,
    eof: bool,
}

impl<R: Read> Chunker<R> {
    pub fn new(reader: R) -> Self {
        Chunker {
            reader,
            buf: Vec::with_capacity(MAX_SIZE * 2),
            start: 0,
            eof: false,
        }
    }

    /// Top the buffer up, compacting consumed bytes out of the way first.
    ///
    /// Compaction is the expensive part — it memmoves whatever is left — so it
    /// happens once per bufferful, not once per chunk. Draining on every chunk
    /// moved roughly `BUF_TARGET` bytes per `AVG_SIZE` of output, which is two
    /// orders of magnitude of pure memmove for nothing.
    fn fill(&mut self) -> std::io::Result<()> {
        if self.start > 0 {
            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(self.buf.len() - self.start);
            self.start = 0;
        }
        let mut tmp = [0u8; 128 * 1024];
        while !self.eof && self.buf.len() < BUF_TARGET {
            let n = self.reader.read(&mut tmp)?;
            if n == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&tmp[..n]);
            }
        }
        Ok(())
    }
}

impl<R: Read> Iterator for Chunker<R> {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        // Only refill when the tail is too short to guarantee a correct cut.
        // Above that, chunks are handed out straight from the buffer with no
        // copying at all.
        if self.buf.len() - self.start < MAX_SIZE
            && !self.eof
            && let Err(e) = self.fill()
        {
            return Some(Err(e));
        }
        let avail = &self.buf[self.start..];
        if avail.is_empty() {
            return None;
        }
        let len = cut_point(avail);
        let chunk = avail[..len].to_vec();
        self.start += len;
        Some(Ok(chunk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_all(data: &[u8]) -> Vec<Vec<u8>> {
        Chunker::new(data).map(|c| c.unwrap()).collect()
    }

    /// Pseudorandom but deterministic bytes — real-ish entropy so the chunker
    /// actually finds boundaries.
    fn pseudo_data(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut s = seed | 1;
        while out.len() < len {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn chunks_reassemble_to_the_original() {
        let data = pseudo_data(5 * 1024 * 1024, 42);
        let rejoined: Vec<u8> = chunk_all(&data).concat();
        assert_eq!(rejoined, data, "chunking must be lossless");
    }

    #[test]
    fn chunk_sizes_respect_bounds() {
        let data = pseudo_data(5 * 1024 * 1024, 7);
        let chunks = chunk_all(&data);
        assert!(chunks.len() > 1);
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= MAX_SIZE, "chunk {i} over max: {}", c.len());
            // Only the final chunk is allowed to be under the minimum.
            if i + 1 < chunks.len() {
                assert!(c.len() >= MIN_SIZE, "chunk {i} under min: {}", c.len());
            }
        }
    }

    /// Guards the exact failure that shipped here once: a mask whose bit count
    /// is far off makes the chunker degenerate without any test failing.
    #[test]
    fn masks_have_the_intended_bit_counts() {
        assert_eq!(MASK_STRICT.count_ones(), 15, "strict mask must be 15 bits");
        assert_eq!(MASK_LOOSE.count_ones(), 11, "loose mask must be 11 bits");
        assert_eq!(
            MASK_LOOSE & MASK_STRICT,
            MASK_LOOSE,
            "loose must be a subset of strict, so any strict boundary is also a loose one"
        );
    }

    /// Chunk sizes must actually *vary*. If normalization is too aggressive the
    /// average still looks right while every chunk is the same size — which is
    /// fixed-size chunking wearing a disguise.
    #[test]
    fn chunk_sizes_are_actually_variable() {
        let data = pseudo_data(8 * 1024 * 1024, 31337);
        let sizes: Vec<usize> = chunk_all(&data).iter().map(|c| c.len()).collect();
        let n = sizes.len() as f64;
        let mean = sizes.iter().sum::<usize>() as f64 / n;
        let var = sizes.iter().map(|&s| (s as f64 - mean).powi(2)).sum::<f64>() / n;
        let cv = var.sqrt() / mean; // coefficient of variation

        assert!(
            cv > 0.15,
            "chunk sizes barely vary (cv={cv:.3}, mean={mean:.0}) — \
             normalization is too aggressive and CDC has degenerated"
        );

        let pinned = sizes.iter().filter(|&&s| s.abs_diff(AVG_SIZE) < 64).count();
        assert!(
            (pinned as f64) < n * 0.5,
            "{pinned}/{n} chunks landed within 64 bytes of AVG_SIZE — degenerate"
        );
    }

    #[test]
    fn average_size_is_near_target() {
        let data = pseudo_data(8 * 1024 * 1024, 99);
        let chunks = chunk_all(&data);
        let avg = data.len() / chunks.len();
        // Loose bounds; we only care that normalization keeps us in the
        // neighbourhood rather than degenerating toward MIN or MAX.
        assert!(
            avg > AVG_SIZE / 2 && avg < AVG_SIZE * 2,
            "average chunk size {avg} is far from target {AVG_SIZE}"
        );
    }

    /// The property that justifies the whole module: a small edit in the middle
    /// of a file must leave the vast majority of chunk boundaries untouched.
    #[test]
    fn insertion_only_perturbs_local_chunks() {
        let original = pseudo_data(4 * 1024 * 1024, 1234);

        let mut edited = original.clone();
        edited.splice(2_000_000..2_000_000, *b"HELLO FKIT"); // insert 10 bytes

        let a = chunk_all(&original);
        let b = chunk_all(&edited);

        let set_a: std::collections::HashSet<_> =
            a.iter().map(|c| blake3::hash(c)).collect();
        let shared = b.iter().filter(|c| set_a.contains(&blake3::hash(c))).count();

        let reuse = shared as f64 / b.len() as f64;
        assert!(
            reuse > 0.95,
            "expected >95% chunk reuse after a 10-byte insert, got {:.1}% \
             ({shared}/{} chunks)",
            reuse * 100.0,
            b.len()
        );
    }

    /// Contrast test: this is what fixed-size chunking would have given us.
    #[test]
    fn fixed_size_chunking_would_have_failed_the_previous_test() {
        let original = pseudo_data(1024 * 1024, 5);
        let mut edited = original.clone();
        edited.splice(0..0, *b"X");

        let fixed = |d: &[u8]| -> Vec<blake3::Hash> {
            d.chunks(AVG_SIZE).map(blake3::hash).collect()
        };
        let set_a: std::collections::HashSet<_> = fixed(&original).into_iter().collect();
        let b = fixed(&edited);
        let shared = b.iter().filter(|h| set_a.contains(h)).count();

        assert!(
            shared <= 1,
            "fixed-size chunking should lose nearly all reuse, kept {shared}"
        );
    }
}
