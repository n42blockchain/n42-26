use jmt::SimpleHasher;

/// Blake3-based hasher for JMT.
///
/// Blake3 provides ~10-15x speedup over Keccak-256, SIMD acceleration,
/// and 256-bit output suitable for quantum resistance (128-bit post-quantum security).
#[derive(Debug, Clone)]
pub struct Blake3Hasher(blake3::Hasher);

impl SimpleHasher for Blake3Hasher {
    fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_hasher_deterministic() {
        let digest = |data: &[u8]| {
            let mut h = Blake3Hasher::new();
            h.update(data);
            h.finalize()
        };

        assert_eq!(digest(b"hello"), digest(b"hello"));
        assert_ne!(digest(b"hello"), digest(b"world"));
    }

    #[test]
    fn blake3_hasher_incremental() {
        let mut h1 = Blake3Hasher::new();
        h1.update(b"hello");
        h1.update(b"world");
        let r1 = h1.finalize();

        let mut h2 = Blake3Hasher::new();
        h2.update(b"helloworld");
        let r2 = h2.finalize();

        assert_eq!(r1, r2);
    }
}
