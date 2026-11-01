//! Entropy core (roadmap R4): Shannon entropy per fixed-size
//! block over a byte source — the data behind an entropy strip
//! that shows where a file is dense (compressed/encrypted data,
//! near 8.0) and where it is sparse (text and zero runs, near 0).
//!
//! The block size is a parameter: the core default is
//! [`DEFAULT_ENTROPY_BLOCK`] = 4096 bytes. 4096 was chosen over the
//! 256 bytes first considered because a 256-byte sample's entropy
//! is dominated by sampling noise (a block of English text and a
//! block of random data differ by less than their own variance),
//! while 4096 keeps the value vector small — a 100 GB file is
//! 26 million blocks, and the strip UI aggregates blocks to pixels
//! anyway. Callers rendering a zoomed strip pass a smaller block.

/// Default block size for [`entropy_map`], in bytes.
pub const DEFAULT_ENTROPY_BLOCK: u64 = 4096;

/// Shannon entropy of one block, in bits per byte: 0.0 for empty
/// or single-symbol input, 8.0 for a perfectly uniform byte
/// distribution.
pub fn block_entropy(block: &[u8]) -> f64 {
    if block.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in block {
        counts[b as usize] += 1;
    }
    let len = block.len() as f64;
    let mut entropy = 0.0f64;
    for &count in &counts {
        if count > 0 {
            let p = count as f64 / len;
            entropy -= p * p.log2();
        }
    }
    // Clamp float dust at the extremes.
    entropy.clamp(0.0, 8.0)
}

/// Entropy of every block of `block_size` bytes over `total`
/// bytes from a byte source, streamed in block-sized reads (the
/// source is never held whole). The last block may be short and is
/// scored on the bytes it has. A `block_size` of 0 means the
/// default. A source that ends early ends the map (the values
/// produced stand, one per full or partial block read).
pub fn entropy_map(
    total: u64,
    mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
    block_size: u64,
) -> Vec<f64> {
    let block_size = if block_size == 0 {
        DEFAULT_ENTROPY_BLOCK
    } else {
        block_size
    };
    let mut values = Vec::new();
    let mut buffer = vec![0u8; block_size as usize];
    let mut offset: u64 = 0;
    while offset < total {
        let want = ((total - offset) as usize).min(buffer.len());
        // Fill the block fully where the source allows: read_at may
        // legally return short reads mid-source.
        let mut filled = 0usize;
        while filled < want {
            let read = read_at(offset + filled as u64, &mut buffer[filled..want]);
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        values.push(block_entropy(&buffer[..filled]));
        offset += filled as u64;
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
        move |offset: u64, out: &mut [u8]| {
            let s = offset as usize;
            if s >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - s);
            out[..n].copy_from_slice(&data[s..s + n]);
            n
        }
    }

    #[test]
    fn block_entropy_extremes_and_known_values() {
        assert_eq!(block_entropy(&[]), 0.0);
        assert_eq!(block_entropy(&[7u8; 1000]), 0.0);
        // Two equally likely symbols: exactly 1 bit.
        assert!((block_entropy(&[0u8, 1].repeat(500)) - 1.0).abs() < 1e-12);
        // Four equally likely symbols: exactly 2 bits.
        assert!((block_entropy(&[0u8, 1, 2, 3].repeat(250)) - 2.0).abs() < 1e-12);
        // All 256 symbols equally: exactly 8 bits.
        let uniform: Vec<u8> = (0..=255).collect();
        assert!((block_entropy(&uniform) - 8.0).abs() < 1e-12);
        // Skewed: 3/4 zeros, 1/4 ones → -(0.75·log2 0.75 + 0.25·log2 0.25).
        let skewed = [0u8, 0, 0, 1].repeat(100);
        assert!((block_entropy(&skewed) - 0.811_278_124_459_132_8).abs() < 1e-9);
    }

    #[test]
    fn map_covers_blocks_and_the_partial_tail() {
        let mut data = vec![0u8; 100]; // block 0: entropy 0
        data.extend((0..=255).collect::<Vec<u8>>()); // block 1 (256 bytes at size 100: mixed)
        let values = entropy_map(data.len() as u64, reader(&data), 100);
        assert_eq!(values.len(), 4); // 356 bytes / 100 = 3 full + 56 tail
        assert_eq!(values[0], 0.0);
        assert!(values.iter().all(|&v| (0.0..=8.0).contains(&v)));
        // Zero block size selects the default: the whole input is
        // one short block here.
        let values = entropy_map(data.len() as u64, reader(&data), 0);
        assert_eq!(values.len(), 1);
        // Empty input, empty map.
        assert!(entropy_map(0, reader(&[]), 100).is_empty());
    }
}
