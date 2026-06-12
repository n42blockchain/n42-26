//! Batched internal-node hashing: N-way (16 AVX-512 / 8 AVX2) single-block
//! blake3 compressions in transposed (SoA) form.
//!
//! `hash_node(left, right) = blake3(left || right)` is a 64-byte input — exactly
//! one blake3 block, i.e. ONE compression call with `CV = IV`, `block_len = 64`,
//! `flags = CHUNK_START | CHUNK_END | ROOT`, output = first 8 state words. That
//! makes batching trivial: run the compression across SIMD lanes, one independent
//! (left, right) pair per lane. This mirrors gov5's Go `compressNodes16AVX512` /
//! `compressNodes8AVX2` kernels.
//!
//! Correctness: the kernels are verified byte-for-byte against the `blake3` crate
//! in unit tests (random inputs), and the end-to-end gov5 cross-check tests pin
//! the whole pipeline. Runtime feature detection picks AVX-512 → AVX2 → scalar;
//! `TWIG_NO_SIMD=1` forces scalar (for A/B measurement and non-x86 parity).

use crate::{Hash, hash_node};

/// blake3 IV (also the CV for a single-chunk input).
#[cfg(target_arch = "x86_64")]
const IV: [u32; 8] = [
    0x6A09_E667,
    0xBB67_AE85,
    0x3C6E_F372,
    0xA54F_F53A,
    0x510E_527F,
    0x9B05_688C,
    0x1F83_D9AB,
    0x5BE0_CD19,
];

/// Per-round message word schedule (round r uses `m[MSG_SCHEDULE[r][i]]`),
/// from the blake3 reference implementation.
#[cfg(target_arch = "x86_64")]
const MSG_SCHEDULE: [[usize; 16]; 7] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8],
    [3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1],
    [10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6],
    [12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4],
    [9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7],
    [11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13],
];

/// CHUNK_START | CHUNK_END | ROOT — a complete single-block, single-chunk input.
#[cfg(target_arch = "x86_64")]
const FLAGS: u32 = 0x0B;
#[cfg(target_arch = "x86_64")]
const BLOCK_LEN: u32 = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kernel {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "x86_64")]
    Avx512,
}

fn kernel() -> Kernel {
    use std::sync::OnceLock;
    static K: OnceLock<u8> = OnceLock::new();
    let v = *K.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            if std::env::var_os("TWIG_NO_SIMD").is_none() {
                if std::arch::is_x86_feature_detected!("avx512f") {
                    return 2;
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    return 1;
                }
            }
        }
        0
    });
    match v {
        #[cfg(target_arch = "x86_64")]
        2 => Kernel::Avx512,
        #[cfg(target_arch = "x86_64")]
        1 => Kernel::Avx2,
        _ => Kernel::Scalar,
    }
}

/// Active kernel name (diagnostics / benches).
pub fn kernel_name() -> &'static str {
    match kernel() {
        Kernel::Scalar => "scalar",
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx2 => "avx2-8way",
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx512 => "avx512-16way",
    }
}

/// `nodes[p] = hash_node(nodes[2p], nodes[2p+1])` for every `p` in `parents`,
/// batched across SIMD lanes when available.
///
/// Callers MUST pass parents from a single heap level (no parent an ancestor of
/// another), so every read (children) is disjoint from every write (parents) —
/// both `Twig::recompute` (whole levels) and `Twig::fold_batch` (level-by-level)
/// satisfy this by construction.
pub(crate) fn hash_parents(nodes: &mut [Hash], parents: &[usize]) {
    match kernel() {
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx512 => unsafe { x86::hash_parents_avx512(nodes, parents) },
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx2 => unsafe { x86::hash_parents_avx2(nodes, parents) },
        Kernel::Scalar => hash_parents_scalar(nodes, parents),
    }
}

fn hash_parents_scalar(nodes: &mut [Hash], parents: &[usize]) {
    for &p in parents {
        nodes[p] = hash_node(&nodes[2 * p], &nodes[2 * p + 1]);
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::*;
    use core::arch::x86_64::*;

    /// Gather one lane's 64-byte block (two adjacent child hashes) into the
    /// word-major transposed message buffer: `msg_t[w * LANES + lane]`.
    #[inline(always)]
    unsafe fn fill_lane<const LANES: usize>(
        msg_t: &mut [u32],
        nodes: &[Hash],
        lane: usize,
        parent: usize,
    ) {
        unsafe {
            let src = nodes.as_ptr().add(2 * parent) as *const u32;
            for w in 0..16 {
                msg_t[w * LANES + lane] = src.add(w).read_unaligned();
            }
        }
    }

    /// Scatter one lane's 32-byte output hash back to `nodes[parent]`.
    #[inline(always)]
    unsafe fn store_lane<const LANES: usize>(
        out_t: &[u32],
        nodes: &mut [Hash],
        lane: usize,
        parent: usize,
    ) {
        unsafe {
            let dst = nodes.as_mut_ptr().add(parent) as *mut u32;
            for w in 0..8 {
                dst.add(w).write_unaligned(out_t[w * LANES + lane]);
            }
        }
    }

    // ── AVX-512: 16 lanes ──

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn hash_parents_avx512(nodes: &mut [Hash], parents: &[usize]) {
        let mut i = 0usize;
        let mut msg_t = [0u32; 16 * 16];
        let mut out_t = [0u32; 8 * 16];
        while i + 16 <= parents.len() {
            unsafe {
                for l in 0..16 {
                    fill_lane::<16>(&mut msg_t, nodes, l, parents[i + l]);
                }
                compress16(&msg_t, &mut out_t);
                for l in 0..16 {
                    store_lane::<16>(&out_t, nodes, l, parents[i + l]);
                }
            }
            i += 16;
        }
        hash_parents_scalar(nodes, &parents[i..]);
    }

    /// 16 independent single-block blake3 compressions (CV=IV, len=64,
    /// flags=CHUNK_START|CHUNK_END|ROOT), transposed: word w of lane l lives at
    /// `[w * 16 + l]`. Output is the first 8 state words xor the second 8.
    #[target_feature(enable = "avx512f")]
    unsafe fn compress16(msg_t: &[u32; 256], out_t: &mut [u32; 128]) {
        unsafe {
            let mut m = [_mm512_setzero_si512(); 16];
            for (w, mv) in m.iter_mut().enumerate() {
                *mv = _mm512_loadu_si512(msg_t.as_ptr().add(w * 16) as *const _);
            }
            macro_rules! bc {
                ($x:expr) => {
                    _mm512_set1_epi32($x as i32)
                };
            }
            let mut v = [
                bc!(IV[0]),
                bc!(IV[1]),
                bc!(IV[2]),
                bc!(IV[3]),
                bc!(IV[4]),
                bc!(IV[5]),
                bc!(IV[6]),
                bc!(IV[7]),
                bc!(IV[0]),
                bc!(IV[1]),
                bc!(IV[2]),
                bc!(IV[3]),
                _mm512_setzero_si512(), // counter_lo
                _mm512_setzero_si512(), // counter_hi
                bc!(BLOCK_LEN),
                bc!(FLAGS),
            ];
            // G mixes lanes-parallel; rotr(n) == rotl(32-n) (VPROLD is AVX-512F).
            macro_rules! G {
                ($a:literal,$b:literal,$c:literal,$d:literal,$mx:expr,$my:expr) => {
                    v[$a] = _mm512_add_epi32(_mm512_add_epi32(v[$a], v[$b]), $mx);
                    v[$d] = _mm512_rol_epi32::<16>(_mm512_xor_si512(v[$d], v[$a]));
                    v[$c] = _mm512_add_epi32(v[$c], v[$d]);
                    v[$b] = _mm512_rol_epi32::<20>(_mm512_xor_si512(v[$b], v[$c]));
                    v[$a] = _mm512_add_epi32(_mm512_add_epi32(v[$a], v[$b]), $my);
                    v[$d] = _mm512_rol_epi32::<24>(_mm512_xor_si512(v[$d], v[$a]));
                    v[$c] = _mm512_add_epi32(v[$c], v[$d]);
                    v[$b] = _mm512_rol_epi32::<25>(_mm512_xor_si512(v[$b], v[$c]));
                };
            }
            for s in MSG_SCHEDULE.iter() {
                G!(0, 4, 8, 12, m[s[0]], m[s[1]]);
                G!(1, 5, 9, 13, m[s[2]], m[s[3]]);
                G!(2, 6, 10, 14, m[s[4]], m[s[5]]);
                G!(3, 7, 11, 15, m[s[6]], m[s[7]]);
                G!(0, 5, 10, 15, m[s[8]], m[s[9]]);
                G!(1, 6, 11, 12, m[s[10]], m[s[11]]);
                G!(2, 7, 8, 13, m[s[12]], m[s[13]]);
                G!(3, 4, 9, 14, m[s[14]], m[s[15]]);
            }
            for i in 0..8 {
                let o = _mm512_xor_si512(v[i], v[i + 8]);
                _mm512_storeu_si512(out_t.as_mut_ptr().add(i * 16) as *mut _, o);
            }
        }
    }

    // ── AVX2: 8 lanes ──

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn hash_parents_avx2(nodes: &mut [Hash], parents: &[usize]) {
        let mut i = 0usize;
        let mut msg_t = [0u32; 16 * 8];
        let mut out_t = [0u32; 8 * 8];
        while i + 8 <= parents.len() {
            unsafe {
                for l in 0..8 {
                    fill_lane::<8>(&mut msg_t, nodes, l, parents[i + l]);
                }
                compress8(&msg_t, &mut out_t);
                for l in 0..8 {
                    store_lane::<8>(&out_t, nodes, l, parents[i + l]);
                }
            }
            i += 8;
        }
        hash_parents_scalar(nodes, &parents[i..]);
    }

    /// 8-lane variant of [`compress16`] (AVX2 has no native rotate: shift+or).
    #[target_feature(enable = "avx2")]
    unsafe fn compress8(msg_t: &[u32; 128], out_t: &mut [u32; 64]) {
        unsafe {
            let mut m = [_mm256_setzero_si256(); 16];
            for (w, mv) in m.iter_mut().enumerate() {
                *mv = _mm256_loadu_si256(msg_t.as_ptr().add(w * 8) as *const _);
            }
            macro_rules! bc {
                ($x:expr) => {
                    _mm256_set1_epi32($x as i32)
                };
            }
            let mut v = [
                bc!(IV[0]),
                bc!(IV[1]),
                bc!(IV[2]),
                bc!(IV[3]),
                bc!(IV[4]),
                bc!(IV[5]),
                bc!(IV[6]),
                bc!(IV[7]),
                bc!(IV[0]),
                bc!(IV[1]),
                bc!(IV[2]),
                bc!(IV[3]),
                _mm256_setzero_si256(),
                _mm256_setzero_si256(),
                bc!(BLOCK_LEN),
                bc!(FLAGS),
            ];
            // rotr by N == (x >> N) | (x << 32-N); shift imms are const generics.
            macro_rules! rotr {
                ($x:expr, $r:literal, $l:literal) => {
                    _mm256_or_si256(_mm256_srli_epi32::<$r>($x), _mm256_slli_epi32::<$l>($x))
                };
            }
            macro_rules! G {
                ($a:literal,$b:literal,$c:literal,$d:literal,$mx:expr,$my:expr) => {
                    v[$a] = _mm256_add_epi32(_mm256_add_epi32(v[$a], v[$b]), $mx);
                    v[$d] = rotr!(_mm256_xor_si256(v[$d], v[$a]), 16, 16);
                    v[$c] = _mm256_add_epi32(v[$c], v[$d]);
                    v[$b] = rotr!(_mm256_xor_si256(v[$b], v[$c]), 12, 20);
                    v[$a] = _mm256_add_epi32(_mm256_add_epi32(v[$a], v[$b]), $my);
                    v[$d] = rotr!(_mm256_xor_si256(v[$d], v[$a]), 8, 24);
                    v[$c] = _mm256_add_epi32(v[$c], v[$d]);
                    v[$b] = rotr!(_mm256_xor_si256(v[$b], v[$c]), 7, 25);
                };
            }
            for s in MSG_SCHEDULE.iter() {
                G!(0, 4, 8, 12, m[s[0]], m[s[1]]);
                G!(1, 5, 9, 13, m[s[2]], m[s[3]]);
                G!(2, 6, 10, 14, m[s[4]], m[s[5]]);
                G!(3, 7, 11, 15, m[s[6]], m[s[7]]);
                G!(0, 5, 10, 15, m[s[8]], m[s[9]]);
                G!(1, 6, 11, 12, m[s[10]], m[s[11]]);
                G!(2, 7, 8, 13, m[s[12]], m[s[13]]);
                G!(3, 4, 9, 14, m[s[14]], m[s[15]]);
            }
            for i in 0..8 {
                let o = _mm256_xor_si256(v[i], v[i + 8]);
                _mm256_storeu_si256(out_t.as_mut_ptr().add(i * 8) as *mut _, o);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    fn random_nodes(n: usize) -> Vec<Hash> {
        let mut rng = rand::rng();
        let mut v = vec![[0u8; 32]; n];
        for h in &mut v {
            rng.fill_bytes(h);
        }
        v
    }

    /// Expected results straight from the blake3 crate (the scalar reference).
    fn expected(nodes: &[Hash], parents: &[usize]) -> Vec<Hash> {
        parents
            .iter()
            .map(|&p| hash_node(&nodes[2 * p], &nodes[2 * p + 1]))
            .collect()
    }

    fn check(parents_len: usize) {
        // Heap-like layout: parents in [1, 64), children at 2p / 2p+1 < 128.
        let nodes = random_nodes(128);
        let parents: Vec<usize> = (1..1 + parents_len).collect();
        let want = expected(&nodes, &parents);

        // Dispatcher (whatever this CPU picks).
        let mut got = nodes.clone();
        hash_parents(&mut got, &parents);
        for (i, &p) in parents.iter().enumerate() {
            assert_eq!(got[p], want[i], "dispatcher lane {i} (parent {p})");
        }

        // Force each wide kernel when the CPU supports it.
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                let mut got = nodes.clone();
                unsafe { x86::hash_parents_avx2(&mut got, &parents) };
                for (i, &p) in parents.iter().enumerate() {
                    assert_eq!(got[p], want[i], "avx2 lane {i} (parent {p})");
                }
            }
            if std::arch::is_x86_feature_detected!("avx512f") {
                let mut got = nodes.clone();
                unsafe { x86::hash_parents_avx512(&mut got, &parents) };
                for (i, &p) in parents.iter().enumerate() {
                    assert_eq!(got[p], want[i], "avx512 lane {i} (parent {p})");
                }
            }
        }
    }

    #[test]
    fn kernels_match_blake3_crate() {
        // Cover full batches, tails, and sub-width sizes across many random runs.
        for len in [1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 48, 63] {
            for _ in 0..50 {
                check(len);
            }
        }
    }

    #[test]
    fn all_zero_inputs_match() {
        // The all-null subtree case (null_level) must also agree.
        let nodes = vec![[0u8; 32]; 128];
        let parents: Vec<usize> = (1..33).collect();
        let want = expected(&nodes, &parents);
        let mut got = nodes.clone();
        hash_parents(&mut got, &parents);
        for (i, &p) in parents.iter().enumerate() {
            assert_eq!(got[p], want[i]);
        }
    }
}
