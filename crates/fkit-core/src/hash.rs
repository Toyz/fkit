//! A `Hash` is the *name* of every piece of data in fkit.
//!
//! We use BLAKE3 (32 bytes). Beyond being fast, BLAKE3 is itself internally a
//! Merkle tree, which is a nice bit of symmetry for a system built on them.
//!
//! The golden rule of a content-addressed store: **the name is derived from the
//! bytes**. You never choose an id; you compute it. Two identical things are
//! automatically the same object, everywhere, forever.

use std::fmt;

pub const HASH_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; HASH_LEN]);

impl Hash {
    /// Hash bytes with a one-byte domain-separation tag.
    ///
    /// Why the tag? Without it, a `Chunk` whose raw bytes happen to equal the
    /// encoding of a `Tree` would collide and be indistinguishable. Prefixing
    /// the type means each object kind lives in its own hash namespace. Git
    /// does the same thing with its `"blob 42\0"` header.
    pub fn of(tag: u8, bytes: &[u8]) -> Hash {
        let mut h = blake3::Hasher::new();
        h.update(&[tag]);
        h.update(bytes);
        Hash(*h.finalize().as_bytes())
    }

    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(HASH_LEN * 2);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Hash> {
        if s.len() != HASH_LEN * 2 {
            return None;
        }
        let mut out = [0u8; HASH_LEN];
        let b = s.as_bytes();
        for i in 0..HASH_LEN {
            let hi = (b[i * 2] as char).to_digit(16)?;
            let lo = (b[i * 2 + 1] as char).to_digit(16)?;
            out[i] = ((hi << 4) | lo) as u8;
        }
        Some(Hash(out))
    }

    /// First 10 hex chars, for human-readable output.
    pub fn short(self) -> String {
        self.to_hex()[..10].to_string()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}
