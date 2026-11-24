//! Folded-multiply non-cryptographic hasher for internal Windows handle, PIDL, and module maps.
//! Modeled after `RustWTM/crates/wtm-sys/src/fasthash.rs` to eliminate SipHash overhead on hot paths.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x243f_6a88_85a3_08d3;
const K: u64 = 0x1319_8a2e_0370_7345;

/// Zero-allocation folded-multiply hasher for trusted internal keys (HMODULEs, PIDLs, cache hashes).
#[derive(Debug, Clone, Copy)]
pub struct FastHasher {
    hash: u64,
}

impl Default for FastHasher {
    fn default() -> Self {
        Self { hash: SEED }
    }
}

impl FastHasher {
    /// Mixes one 64-bit word using a full 128-bit product folded back so low bits carry full entropy.
    #[inline]
    pub fn add(&mut self, word: u64) {
        let product = u128::from(self.hash ^ word) * u128::from(K);
        self.hash = (product as u64) ^ ((product >> 64) as u64);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
            self.add(word);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << 59));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FastBuildHasher = BuildHasherDefault<FastHasher>;
pub type FastMap<K, V> = HashMap<K, V, FastBuildHasher>;
pub type FastSet<K> = HashSet<K, FastBuildHasher>;

/// Hashes an arbitrary byte slice deterministically using [`FastHasher`].
#[inline]
pub fn fast_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = FastHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash<T: Hash>(value: T) -> u64 {
        FastBuildHasher::default().hash_one(value)
    }

    #[test]
    fn spreads_aligned_pointers_over_low_bits() {
        let buckets: FastSet<u64> = (0..1024u64)
            .map(|i| hash(0xffff_a000_0000_0000u64 + i * 16) & 1023)
            .collect();
        assert!(buckets.len() > 600, "{} of 1024 buckets", buckets.len());
    }

    #[test]
    fn distinguishes_byte_strings_of_different_lengths() {
        assert_ne!(hash("a"), hash("a\0"));
        assert_ne!(hash([1u8, 2, 3].as_slice()), hash([1u8, 2].as_slice()));
        assert_eq!(hash((7u32, 9u64)), hash((7u32, 9u64)));
        assert_eq!(fast_hash_bytes(b"hello"), fast_hash_bytes(b"hello"));
        assert_ne!(fast_hash_bytes(b"hello"), fast_hash_bytes(b"hello\0"));
    }

    #[test]
    fn works_as_map_and_set() {
        let mut map: FastMap<(u32, u64), u32> = FastMap::default();
        let mut set: FastSet<u64> = FastSet::default();
        for i in 0..4_000u32 {
            let key = (i, u64::from(i) << 12);
            map.insert(key, i);
            set.insert(key.1);
        }
        assert!((0..4_000u32).all(|i| map[&(i, u64::from(i) << 12)] == i));
        assert_eq!(set.len(), 4_000);
    }
}
