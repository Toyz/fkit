//! Streaming tar and zip of a tree.
//!
//! Both writers take an `impl Write` and never hold more than one chunk in
//! memory, so a 150 GiB tree costs the same as a 150 KiB one. That is the point:
//! a forge that buffers an archive before sending it can be knocked over by
//! asking for a big repository twice.
//!
//! Three things make an archive here cheaper than it looks.
//!
//! **The size is known before any work happens.** Every tree entry records the
//! bytes beneath it, so [`plan`] walks the directory objects — never the file
//! contents — and returns the exact uncompressed size. A request that would
//! exceed a limit is refused before a single chunk is read, and a plain `.tar`
//! can carry a real `Content-Length`.
//!
//! **The output is deterministic.** Fixed permissions, a fixed modification
//! time, no owner names, entries in tree order. The same tree produces the same
//! bytes on every machine, forever.
//!
//! **Which means it is content-addressed.** The archive of a tree is a pure
//! function of the tree hash and the format, so an `ETag` built from those two
//! is immutable and a conditional request can be answered without touching the
//! store at all.

use crate::hash::Hash;
use crate::ingest::{read_entries, read_file};
use crate::object::EntryKind;
use crate::store::Store;
use anyhow::{bail, Result};
use std::io::Write;

/// A file to be written into an archive, in the order it will appear.
#[derive(Debug, Clone)]
pub struct Item {
    /// Path within the archive, `/`-separated, no leading slash.
    pub path: String,
    pub hash: Hash,
    pub size: u64,
    pub kind: EntryKind,
}

/// Everything an archive of a tree will contain, and how big it will be.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Sum of file sizes. Not the archive size — see [`Plan::tar_size`].
    pub bytes: u64,
}

impl Plan {
    /// Exact size of the `.tar` this plan produces.
    ///
    /// tar is entirely predictable: a 512-byte header per entry, the content
    /// padded up to a 512-byte boundary, and two zero blocks at the end. So a
    /// plain tar can be served with a `Content-Length` and a real progress bar,
    /// which a compressed one cannot.
    pub fn tar_size(&self) -> u64 {
        let mut n = 0u64;
        for it in &self.items {
            n += 512; // header
            if let EntryKind::File { .. } = it.kind {
                n += it.size.div_ceil(512) * 512;
            }
            // A symlink's target lives in the header; a directory has no body.
        }
        n + 1024 // two zero blocks terminate the archive
    }
}

/// Walk a tree, listing what an archive would contain.
///
/// Reads directory objects only. File contents are never touched, so this is
/// cheap even for a repository whose contents are enormous — which is what lets
/// a size limit be enforced before doing any real work.
pub fn plan(store: &Store, tree: Hash, prefix: &str) -> Result<Plan> {
    let mut out = Plan::default();
    walk(store, tree, prefix, &mut out, 0)?;
    Ok(out)
}

/// Trees are built from directory objects, and a corrupt or hostile one could
/// name itself. Depth is bounded rather than trusted.
const MAX_DEPTH: usize = 100;

fn walk(store: &Store, tree: Hash, prefix: &str, out: &mut Plan, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("directory nesting deeper than {MAX_DEPTH} — refusing to archive");
    }
    for e in read_entries(store, tree)? {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        match e.kind {
            EntryKind::Dir => {
                out.items.push(Item { path: path.clone(), hash: e.hash, size: 0, kind: e.kind });
                walk(store, e.hash, &path, out, depth + 1)?;
            }
            _ => {
                out.bytes += e.size;
                out.items.push(Item { path, hash: e.hash, size: e.size, kind: e.kind });
            }
        }
    }
    Ok(())
}

/// A fixed timestamp for every entry.
///
/// The store does not record modification times — two identical trees are the
/// same tree regardless of when their files were touched — so there is nothing
/// truthful to write. A constant keeps the output byte-identical; callers who
/// want the commit's time can pass it instead.
pub const EPOCH: u64 = 0;

// ---- tar ----------------------------------------------------------------

/// Write a ustar archive of `plan` into `w`.
///
/// `root` is prepended to every path, the way `git archive` does, so unpacking
/// produces one directory rather than spilling files into the current one.
pub fn write_tar<W: Write>(
    store: &Store,
    plan: &Plan,
    root: &str,
    mtime: u64,
    w: &mut W,
) -> Result<()> {
    for it in &plan.items {
        let name = join(root, &it.path);
        match it.kind {
            EntryKind::Dir => {
                w.write_all(&tar_header(&format!("{name}/"), 0, 0o755, b'5', "", mtime)?)?;
            }
            EntryKind::Symlink => {
                // The target is the file's content, and it is short by nature.
                let mut target = Vec::new();
                read_file(store, it.hash, &mut target)?;
                let target = String::from_utf8_lossy(&target).to_string();
                w.write_all(&tar_header(&name, 0, 0o777, b'2', &target, mtime)?)?;
            }
            EntryKind::File { exec } => {
                let mode = if exec { 0o755 } else { 0o644 };
                w.write_all(&tar_header(&name, it.size, mode, b'0', "", mtime)?)?;
                // Straight from the store into the socket: `read_file` writes
                // chunk by chunk, so the file is never assembled in memory.
                let mut counted = Counting { inner: &mut *w, n: 0 };
                read_file(store, it.hash, &mut counted)?;
                let written = counted.n;
                if written != it.size {
                    bail!(
                        "{}: tree says {} bytes, store produced {written}",
                        it.path, it.size
                    );
                }
                let pad = (512 - (written % 512)) % 512;
                // From a const, not a fresh allocation: a repository with
                // 100,000 files padded 100,000 times, and the allocator cost
                // showed up as tar running several times slower than zip for
                // the same bytes.
                w.write_all(&ZEROS[..pad as usize])?;
            }
        }
    }
    // Two zero blocks, then nothing.
    w.write_all(&[0u8; 1024])?;
    Ok(())
}

/// Counts what passes through, so a file whose content disagrees with the tree
/// cannot silently desynchronise the archive's block alignment.
struct Counting<W: Write> {
    inner: W,
    n: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// One 512-byte ustar header.
fn tar_header(
    name: &str,
    size: u64,
    mode: u32,
    kind: u8,
    link: &str,
    mtime: u64,
) -> Result<[u8; 512]> {
    let mut h = [0u8; 512];

    // ustar splits a long name across `prefix` (155) and `name` (100). Longer
    // than that needs a PAX extension, which is more machinery than a
    // repository path should ever require.
    let (prefix, name) = split_name(name)?;

    h[0..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], mode as u64, 7);
    octal(&mut h[108..116], 0, 7); // uid: nobody in particular
    octal(&mut h[116..124], 0, 7); // gid
    octal(&mut h[124..136], size, 11);
    octal(&mut h[136..148], mtime, 11);
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());

    // The checksum is computed with its own field read as spaces, then written
    // in. Six octal digits, a NUL and a space — the layout every extractor
    // expects, whatever the spec's wording allows.
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    octal(&mut h[148..155], sum as u64, 6);
    h[155] = b' ';

    Ok(h)
}

fn split_name(name: &str) -> Result<(&str, &str)> {
    if name.len() <= 100 {
        return Ok(("", name));
    }
    // Split on a `/` so both halves are whole path components, preferring the
    // latest one that leaves a usable name.
    for (i, _) in name.match_indices('/') {
        let (p, rest) = (&name[..i], &name[i + 1..]);
        if p.len() <= 155 && !rest.is_empty() && rest.len() <= 100 {
            return Ok((p, rest));
        }
    }
    bail!("path too long for a tar archive: {name}")
}

const ZEROS: [u8; 512] = [0u8; 512];

/// Right-aligned octal, NUL-terminated: the format every tar field uses.
///
/// Written by hand rather than through `format!`. Five of these run per header,
/// and a header runs per file — half a million short-lived allocations for a
/// large repository, to produce at most twelve digits.
fn octal(field: &mut [u8], value: u64, digits: usize) {
    field[..digits].fill(b'0');
    field[digits] = 0;
    let mut v = value;
    let mut i = digits;
    while i > 0 {
        i -= 1;
        field[i] = b'0' + (v & 0b111) as u8;
        v >>= 3;
        if v == 0 {
            break;
        }
    }
}

fn join(root: &str, path: &str) -> String {
    if root.is_empty() {
        path.to_string()
    } else {
        format!("{root}/{path}")
    }
}

// ---- zip ----------------------------------------------------------------

/// Write a zip archive of `plan` into `w`.
///
/// Entries are stored, not deflated. The store already compresses what
/// compresses, so a second pass over the same bytes mostly burns CPU to
/// re-discover that; and storing keeps the writer streaming without buffering
/// an entry to learn its compressed length.
///
/// Sizes and CRCs go in a data descriptor after each entry, because the CRC is
/// only known once the bytes have been read — and reading them twice, or
/// holding them, is exactly what this is avoiding.
pub fn write_zip<W: Write>(
    store: &Store,
    plan: &Plan,
    root: &str,
    w: &mut W,
) -> Result<()> {
    let mut central = Vec::new();
    let mut offset: u64 = 0;
    let mut count: u64 = 0;

    for it in &plan.items {
        let name = match it.kind {
            EntryKind::Dir => format!("{}/", join(root, &it.path)),
            _ => join(root, &it.path),
        };
        let nb = name.as_bytes();
        let zip64 = it.size >= u32::MAX as u64;

        // Bit 3: sizes and CRC follow the data. Bit 11: the name is UTF-8.
        let flags: u16 = 0b1000 | (1 << 11);

        let mut local = Vec::with_capacity(30 + nb.len());
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&(if zip64 { 45u16 } else { 20u16 }).to_le_bytes());
        local.extend_from_slice(&flags.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // stored
        local.extend_from_slice(&0u16.to_le_bytes()); // time
        local.extend_from_slice(&0u16.to_le_bytes()); // date
        local.extend_from_slice(&0u32.to_le_bytes()); // crc, in the descriptor
        local.extend_from_slice(&0u32.to_le_bytes()); // compressed
        local.extend_from_slice(&0u32.to_le_bytes()); // uncompressed
        local.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // no extra field
        local.extend_from_slice(nb);
        w.write_all(&local)?;

        let local_offset = offset;
        offset += local.len() as u64;

        let mut written = 0u64;
        let mut crc = Crc32::new();
        match it.kind {
            EntryKind::Dir => {}
            _ => {
                let mut sink = Hashing { inner: &mut *w, crc: &mut crc, n: 0 };
                read_file(store, it.hash, &mut sink)?;
                written = sink.n;
            }
        }
        offset += written;

        // The data descriptor. Zip64 widens the two size fields to 64 bits,
        // which is the only reason a >4 GiB entry works at all.
        let crc = crc.finish();
        let mut desc = Vec::with_capacity(24);
        desc.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        desc.extend_from_slice(&crc.to_le_bytes());
        if zip64 {
            desc.extend_from_slice(&written.to_le_bytes());
            desc.extend_from_slice(&written.to_le_bytes());
        } else {
            desc.extend_from_slice(&(written as u32).to_le_bytes());
            desc.extend_from_slice(&(written as u32).to_le_bytes());
        }
        w.write_all(&desc)?;
        offset += desc.len() as u64;

        central.extend_from_slice(&central_entry(nb, crc, written, local_offset, flags, it.kind));
        count += 1;
    }

    let cd_offset = offset;
    w.write_all(&central)?;
    let cd_len = central.len() as u64;

    // Always end with the zip64 records plus the classic ones. An archive that
    // needs them and lacks them is broken; one that does not need them and has
    // them is read correctly by everything, and it costs 76 bytes.
    let mut end = Vec::new();
    end.extend_from_slice(&0x0606_4b50u32.to_le_bytes()); // zip64 end of central directory
    end.extend_from_slice(&44u64.to_le_bytes()); // size of this record after here
    end.extend_from_slice(&45u16.to_le_bytes()); // made by
    end.extend_from_slice(&45u16.to_le_bytes()); // needed
    end.extend_from_slice(&0u32.to_le_bytes()); // this disk
    end.extend_from_slice(&0u32.to_le_bytes()); // disk with cd
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&cd_len.to_le_bytes());
    end.extend_from_slice(&cd_offset.to_le_bytes());

    end.extend_from_slice(&0x0706_4b50u32.to_le_bytes()); // zip64 locator
    end.extend_from_slice(&0u32.to_le_bytes());
    end.extend_from_slice(&(cd_offset + cd_len).to_le_bytes());
    end.extend_from_slice(&1u32.to_le_bytes());

    end.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end of central directory
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&(count.min(0xFFFF) as u16).to_le_bytes());
    end.extend_from_slice(&(count.min(0xFFFF) as u16).to_le_bytes());
    end.extend_from_slice(&(cd_len.min(u32::MAX as u64) as u32).to_le_bytes());
    end.extend_from_slice(&(cd_offset.min(u32::MAX as u64) as u32).to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes()); // no comment
    w.write_all(&end)?;
    Ok(())
}

fn central_entry(
    name: &[u8],
    crc: u32,
    size: u64,
    offset: u64,
    flags: u16,
    kind: EntryKind,
) -> Vec<u8> {
    let big = size >= u32::MAX as u64 || offset >= u32::MAX as u64;
    let mut extra = Vec::new();
    if big {
        extra.extend_from_slice(&0x0001u16.to_le_bytes());
        extra.extend_from_slice(&24u16.to_le_bytes());
        extra.extend_from_slice(&size.to_le_bytes());
        extra.extend_from_slice(&size.to_le_bytes());
        extra.extend_from_slice(&offset.to_le_bytes());
    }

    // Unix permissions live in the high half of the external attributes; the
    // low byte's 0x10 marks a directory for DOS-era readers.
    let mode: u32 = match kind {
        EntryKind::Dir => 0o040755,
        EntryKind::Symlink => 0o120777,
        EntryKind::File { exec: true } => 0o100755,
        EntryKind::File { exec: false } => 0o100644,
    };
    let external = (mode << 16) | if matches!(kind, EntryKind::Dir) { 0x10 } else { 0 };

    let mut c = Vec::with_capacity(46 + name.len() + extra.len());
    c.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    c.extend_from_slice(&(0x0300u16 | 45).to_le_bytes()); // made by unix
    c.extend_from_slice(&(if big { 45u16 } else { 20u16 }).to_le_bytes());
    c.extend_from_slice(&flags.to_le_bytes());
    c.extend_from_slice(&0u16.to_le_bytes()); // stored
    c.extend_from_slice(&0u16.to_le_bytes());
    c.extend_from_slice(&0u16.to_le_bytes());
    c.extend_from_slice(&crc.to_le_bytes());
    let field = |v: u64| if big { u32::MAX } else { v as u32 };
    c.extend_from_slice(&field(size).to_le_bytes());
    c.extend_from_slice(&field(size).to_le_bytes());
    c.extend_from_slice(&(name.len() as u16).to_le_bytes());
    c.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    c.extend_from_slice(&0u16.to_le_bytes()); // comment
    c.extend_from_slice(&0u16.to_le_bytes()); // disk
    c.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    c.extend_from_slice(&external.to_le_bytes());
    c.extend_from_slice(&field(offset).to_le_bytes());
    c.extend_from_slice(name);
    c.extend_from_slice(&extra);
    c
}

/// Passes bytes through while accumulating their CRC — one read, not two.
struct Hashing<'a, W: Write> {
    inner: W,
    crc: &'a mut Crc32,
    n: u64,
}

impl<W: Write> Write for Hashing<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// CRC-32 as zip wants it: reflected, polynomial 0xEDB88320.
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            let i = ((self.0 ^ *b as u32) & 0xFF) as usize;
            self.0 = (self.0 >> 8) ^ CRC_TABLE[i];
        }
    }

    pub fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// Built at compile time rather than shipped as a literal: the polynomial is
/// the thing worth reading, and the 256 words are just its consequence.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_known_vector() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn crc32_is_the_same_whatever_the_write_boundaries() {
        // The zip writer feeds it one store chunk at a time, so a CRC that
        // depended on how the bytes arrived would be wrong for large files
        // only — the hardest case to notice.
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut whole = Crc32::new();
        whole.update(&data);
        let mut split = Crc32::new();
        for part in data.chunks(7) {
            split.update(part);
        }
        assert_eq!(whole.finish(), split.finish());
    }

    #[test]
    fn a_tar_header_checksums_the_way_tar_expects() {
        let h = tar_header("a.txt", 3, 0o644, b'0', "", 0).unwrap();
        // The checksum is over the header with its own field blanked.
        let mut copy = h;
        copy[148..156].fill(b' ');
        let want: u32 = copy.iter().map(|b| *b as u32).sum();
        let text = std::str::from_utf8(&h[148..154]).unwrap();
        assert_eq!(u32::from_str_radix(text, 8).unwrap(), want);
        assert_eq!(&h[257..262], b"ustar");
        assert_eq!(h[156], b'0');
    }

    #[test]
    fn long_paths_split_across_prefix_and_name() {
        let deep = format!("{}/{}", "d".repeat(120), "f".repeat(40));
        let (prefix, name) = split_name(&deep).unwrap();
        assert_eq!(prefix.len(), 120);
        assert_eq!(name.len(), 40);

        // A single component too long for the name field cannot be split.
        assert!(split_name(&"x".repeat(120)).is_err());
    }

    #[test]
    fn octal_fields_are_padded_and_terminated() {
        let mut f = [0xAAu8; 8];
        octal(&mut f, 0o644, 7);
        assert_eq!(&f, b"0000644\0");
    }
}
