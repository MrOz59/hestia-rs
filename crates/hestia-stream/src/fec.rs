//! Reed–Solomon erasure coding over GF(2^8), compatible with nanors (the
//! library Sunshine/Hermes and moonlight-common-c use for video FEC).
//!
//! nanors builds a systematic code from a Cauchy matrix: parity shard `j` is
//! `sum_i C[j][i] * data_i` with `C[j][i] = 1 / ((ps + i) XOR j)` in the field
//! with polynomial 0x11D. Any `ds` of the `ds + ps` shards recover the data;
//! we remove the known data from the parity equations and solve only for the
//! missing data shards.

/// Maximum data + parity shards per block.
pub const MAX_SHARDS: usize = 255;

const POLY: u16 = 0x11d;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

const fn build_tables() -> Tables {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    Tables { exp, log }
}

static TABLES: Tables = build_tables();

#[inline]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        TABLES.exp[TABLES.log[a as usize] as usize + TABLES.log[b as usize] as usize]
    }
}

#[inline]
pub fn gf_inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    TABLES.exp[255 - TABLES.log[a as usize] as usize]
}

/// `dst ^= c * src`
fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    match c {
        0 => {}
        1 => dst.iter_mut().zip(src).for_each(|(d, s)| *d ^= s),
        _ => {
            let lc = TABLES.log[c as usize] as usize;
            for (d, &s) in dst.iter_mut().zip(src) {
                if s != 0 {
                    *d ^= TABLES.exp[lc + TABLES.log[s as usize] as usize];
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FecError {
    BadShape,
    NotEnoughShards,
    Singular,
}

impl std::fmt::Display for FecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadShape => f.write_str("invalid shard layout"),
            Self::NotEnoughShards => f.write_str("not enough shards to recover the block"),
            Self::Singular => f.write_str("decoding matrix is singular"),
        }
    }
}

impl std::error::Error for FecError {}

/// Coefficient of data shard `i` in parity shard `j`.
#[inline]
fn cauchy(parity_shards: usize, j: usize, i: usize) -> u8 {
    gf_inv(((parity_shards + i) ^ j) as u8)
}

fn check_shape(data_shards: usize, parity_shards: usize) -> Result<(), FecError> {
    if data_shards == 0 || parity_shards == 0 || data_shards + parity_shards > MAX_SHARDS {
        Err(FecError::BadShape)
    } else {
        Ok(())
    }
}

/// Computes parity shards. `shards` holds `data_shards` data shards followed
/// by `parity_shards` buffers that get overwritten; all must be the same length.
pub fn encode(shards: &mut [Vec<u8>], data_shards: usize, parity_shards: usize) -> Result<(), FecError> {
    check_shape(data_shards, parity_shards)?;
    if shards.len() != data_shards + parity_shards {
        return Err(FecError::BadShape);
    }
    let len = shards[0].len();
    if shards.iter().any(|s| s.len() != len) {
        return Err(FecError::BadShape);
    }
    let (data, parity) = shards.split_at_mut(data_shards);
    for (j, p) in parity.iter_mut().enumerate() {
        p.iter_mut().for_each(|b| *b = 0);
        for (i, d) in data.iter().enumerate() {
            mul_add(p, d, cauchy(parity_shards, j, i));
        }
    }
    Ok(())
}

/// Recovers missing data shards in place. `shards[k]` is `Some` for every
/// received shard (data `0..ds`, then parity `ds..ds+ps`); recovered data
/// shards are filled in with `shard_len` bytes. Parity shards are not rebuilt.
pub fn reconstruct(
    shards: &mut [Option<Vec<u8>>],
    data_shards: usize,
    parity_shards: usize,
    shard_len: usize,
) -> Result<(), FecError> {
    check_shape(data_shards, parity_shards)?;
    if shards.len() != data_shards + parity_shards {
        return Err(FecError::BadShape);
    }
    let (data, parity) = shards.split_at_mut(data_shards);
    let missing: Vec<usize> = (0..data_shards).filter(|&i| data[i].is_none()).collect();
    if missing.is_empty() {
        return Ok(());
    }

    let available: Vec<_> = parity
        .iter()
        .enumerate()
        .filter_map(|(j, shard)| shard.as_deref().map(|shard| (j, shard)))
        .take(missing.len())
        .collect();
    if available.len() < missing.len() {
        return Err(FecError::NotEnoughShards);
    }

    // One erasure needs only one parity equation. Scale the residual in its
    // own buffer and keep it as the recovered shard.
    if let ([index], [(j, shard)]) = (missing.as_slice(), available.as_slice()) {
        let mut recovered = parity_residual(data, parity_shards, *j, shard, shard_len);
        let scale = gf_inv(cauchy(parity_shards, *j, *index));
        if scale != 1 {
            recovered.iter_mut().for_each(|byte| *byte = gf_mul(*byte, scale));
        }
        data[*index] = Some(recovered);
        return Ok(());
    }

    // Each available parity shard gives an equation whose coefficients are
    // the Cauchy columns of the missing data. The matrix is k × k for k
    // erasures, even when the block contains hundreds of data shards.
    let n = missing.len();
    let mut matrix = Vec::with_capacity(n * n);
    for &(j, _) in &available {
        matrix.extend(missing.iter().map(|&i| cauchy(parity_shards, j, i)));
    }
    let inverse = invert(&mut matrix, n)?;
    let residuals: Vec<_> = available
        .into_iter()
        .map(|(j, shard)| parity_residual(data, parity_shards, j, shard, shard_len))
        .collect();

    for (&index, coefficients) in missing.iter().zip(inverse.chunks_exact(n)) {
        let mut recovered = vec![0u8; shard_len];
        for (residual, &coefficient) in residuals.iter().zip(coefficients) {
            mul_add(&mut recovered, residual, coefficient);
        }
        data[index] = Some(recovered);
    }
    Ok(())
}

/// Removes known data contributions from a received parity shard. Short
/// shards have implicit zero padding, as in the wire format.
fn parity_residual(
    data: &[Option<Vec<u8>>],
    parity_shards: usize,
    j: usize,
    shard: &[u8],
    shard_len: usize,
) -> Vec<u8> {
    let mut residual = vec![0u8; shard_len];
    let parity_len = shard.len().min(shard_len);
    residual[..parity_len].copy_from_slice(&shard[..parity_len]);
    for (i, src) in data.iter().enumerate() {
        if let Some(src) = src {
            mul_add(&mut residual, src, cauchy(parity_shards, j, i));
        }
    }
    residual
}

/// Gauss–Jordan inversion over GF(256). Consumes `a` as scratch space.
fn invert(a: &mut [u8], n: usize) -> Result<Vec<u8>, FecError> {
    let mut inv = vec![0u8; n * n];
    for i in 0..n {
        inv[i * n + i] = 1;
    }
    for col in 0..n {
        let pivot = (col..n).find(|&r| a[r * n + col] != 0).ok_or(FecError::Singular)?;
        if pivot != col {
            for c in 0..n {
                a.swap(pivot * n + c, col * n + c);
                inv.swap(pivot * n + c, col * n + c);
            }
        }
        let scale = gf_inv(a[col * n + col]);
        for c in 0..n {
            a[col * n + c] = gf_mul(a[col * n + c], scale);
            inv[col * n + c] = gf_mul(inv[col * n + c], scale);
        }
        for r in 0..n {
            if r == col {
                continue;
            }
            let factor = a[r * n + col];
            if factor == 0 {
                continue;
            }
            for c in 0..n {
                let av = a[col * n + c];
                let iv = inv[col * n + c];
                a[r * n + c] ^= gf_mul(factor, av);
                inv[r * n + c] ^= gf_mul(factor, iv);
            }
        }
    }
    Ok(inv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ds: usize, ps: usize, bs: usize) -> Vec<Vec<u8>> {
        let mut shards: Vec<Vec<u8>> = (0..ds)
            .map(|i| (0..bs).map(|b| ((i * 31 + b * 7 + 1) & 0xff) as u8).collect())
            .collect();
        shards.extend((0..ps).map(|_| vec![0u8; bs]));
        encode(&mut shards, ds, ps).unwrap();
        shards
    }

    /// Parity produced by nanors itself (rs.c + oblas_lite.c compiled with gcc)
    /// for the same input pattern as [`sample`].
    const NANORS_VECTORS: &[&str] = &[
        "6 3 37 b698178a753ef50131cc613f0326d7d1809a151fe004c39da479ce9096b3d81c9e0f801de2 847c422ce025137d0a20e49d4699606602ad93834fa24ad7a5bf9ee7e9363a2cca023c529e 6579a44ce9def0ed06b5990ad4467705faa17cc86dd500f38251e77450c2ceaca825f810b5",
        "1 2 16 8e04890b80129b199220ad27a42ebf35 f4f305f90b1c19e517cb3d3a33342126",
        "10 2 32 b7074f7f219f60a1c04f6ca2347ef157f803a142f51af40d14ab9a45a7aa7dd4 5cade95a875bb5b815195aa09213d5f6338c6a8453eb52ddc1ed4f3428c7db18",
        "20 6 24 8e544c8c4141fab44019bc3f909b58dbb3dc47a3791c4d03 176b0d2a9cb0e463cdfd20534d80c7fe9eb70049a156900b 5336bcc714b7ebb2cea7ad400d30d1163e41a42a816ab2e8 0c54f1028c8845867f664226951b226bf92400e99e142a05 c24e6af9cad142aac27260a0dcf90987b4afeb6bdb456e8e 3126f1193fe6016a0003324029b9f2c24f7d38bc95b39be9",
    ];

    #[test]
    fn matches_nanors() {
        for line in NANORS_VECTORS {
            let mut parts = line.split(' ');
            let ds: usize = parts.next().unwrap().parse().unwrap();
            let ps: usize = parts.next().unwrap().parse().unwrap();
            let bs: usize = parts.next().unwrap().parse().unwrap();
            let shards = sample(ds, ps, bs);
            for (j, expected) in parts.enumerate() {
                let got: String = shards[ds + j].iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(got, expected, "ds={ds} ps={ps} parity {j}");
            }
        }
    }

    #[test]
    fn inverse_table() {
        for a in 1..=255u8 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "a = {a}");
        }
        // Spot values from nanors' GF2_8_INV table.
        assert_eq!(gf_inv(2), 142);
        assert_eq!(gf_inv(3), 244);
        assert_eq!(gf_inv(4), 71);
    }

    #[test]
    fn recovers_any_erasure_pattern() {
        let (ds, ps, bs) = (6, 3, 37);
        let original = sample(ds, ps, bs);
        // Every way of losing up to `ps` shards.
        for mask in 0u32..(1 << (ds + ps)) {
            if mask.count_ones() as usize > ps {
                continue;
            }
            let mut shards: Vec<Option<Vec<u8>>> = original
                .iter()
                .enumerate()
                .map(|(k, s)| (mask & (1 << k) == 0).then(|| s.clone()))
                .collect();
            reconstruct(&mut shards, ds, ps, bs).unwrap();
            for i in 0..ds {
                assert_eq!(shards[i].as_ref().unwrap(), &original[i], "mask {mask:#b}, shard {i}");
            }
        }
    }

    #[test]
    fn recovers_zero_padded_tails_in_a_large_block() {
        let (ds, ps, bs) = (192, 63, 1408);
        let mut original = sample(ds, ps, bs);
        for shard in &mut original[..ds] {
            shard[bs - 17..].fill(0);
        }
        encode(&mut original, ds, ps).unwrap();

        // The first parity shard is also absent. Recovery must work with
        // later parity rows, both for one erasure and for several.
        for missing in [&[ds - 1][..], &[0, 39, ds - 1][..]] {
            let mut shards: Vec<_> = original.iter().map(|shard| Some(shard[..bs - 17].to_vec())).collect();
            shards[ds] = None;
            for &i in missing {
                shards[i] = None;
            }
            reconstruct(&mut shards, ds, ps, bs).unwrap();
            for &i in missing {
                assert_eq!(shards[i].as_deref(), Some(original[i].as_slice()));
            }
            assert!(shards[ds].is_none());
        }
    }

    #[test]
    fn too_many_losses() {
        let original = sample(4, 2, 8);
        let mut shards: Vec<Option<Vec<u8>>> = original.into_iter().map(Some).collect();
        shards[0] = None;
        shards[1] = None;
        shards[4] = None;
        assert_eq!(reconstruct(&mut shards, 4, 2, 8), Err(FecError::NotEnoughShards));
    }
}
