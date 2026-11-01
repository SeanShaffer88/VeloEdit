//! Hand-written streaming MD5, SHA-1, and SHA-256 (roadmap item 37).
//!
//! The crate's dependency rule (windows-sys is the only crate) rules
//! out a hashing library, so the three digests the file-hash feature
//! needs are implemented here directly from their public
//! specifications: MD5 from RFC 1321, SHA-1 and SHA-256 from
//! FIPS 180-4. Each hasher is a small streaming state machine —
//! [`StreamingHash::update`] accepts any number of bytes in any
//! chunking, and `finalize` applies the algorithm's padding and
//! returns the digest. Chunking never affects the result; a test
//! feeds a million bytes in odd-sized chunks and compares against
//! the published known-answer digest.
//!
//! [`hash_byte_source`] runs all three over a byte source read
//! through fixed-size windows, so hashing a file of any size holds
//! only one window of it in memory.

/// Window size used by [`hash_byte_source`]: large enough to keep
/// the per-call overhead negligible, small enough to stay a
/// rounding error next to the program's memory budget.
pub const HASH_WINDOW: usize = 64 * 1024;

/// Lower-case hex rendering of a digest (the form the known-answer
/// vectors and hash-compare fields use).
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The common streaming shape of the three hashers: feed bytes with
/// [`StreamingHash::update`], then take the hex digest with
/// [`StreamingHash::finalize_hex`]. Each hasher also offers a
/// `finalize` returning the raw digest bytes.
pub trait StreamingHash {
    /// Feed the next chunk of input. Any chunk sizes, in any order
    /// of calls, produce the same digest as one whole-input call.
    fn update(&mut self, data: &[u8]);
    /// Apply the padding and return the digest as lower-case hex.
    fn finalize_hex(self) -> String;
}

/// Shared block-buffer plumbing for the three hashers: accumulate
/// input into one 64-byte block and hand full blocks to the
/// algorithm's compression function.
#[derive(Clone)]
struct BlockBuffer {
    block: [u8; 64],
    used: usize,
    total: u64,
}

impl BlockBuffer {
    fn new() -> Self {
        Self {
            block: [0u8; 64],
            used: 0,
            total: 0,
        }
    }

    /// Feed `data`, calling `compress` on each completed block.
    fn feed(&mut self, mut data: &[u8], mut compress: impl FnMut(&[u8; 64])) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.used > 0 {
            let take = (64 - self.used).min(data.len());
            self.block[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
            if self.used == 64 {
                compress(&self.block);
                self.used = 0;
            }
        }
        while data.len() >= 64 {
            let (chunk, rest) = data.split_at(64);
            compress(chunk.try_into().expect("64-byte chunk"));
            data = rest;
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.used = data.len();
        }
    }
}

// ---------------------------------------------------------------------------
// MD5 (RFC 1321)
// ---------------------------------------------------------------------------

/// Per-step left-rotate amounts, rounds 1–4.
const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `K[i] = floor(2^32 * abs(sin(i + 1)))`, the RFC 1321 sine table.
const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// Streaming MD5 hasher (RFC 1321). See [`StreamingHash`].
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    buffer: BlockBuffer,
}

impl Md5 {
    pub fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            buffer: BlockBuffer::new(),
        }
    }

    fn compress(state: &mut [u32; 4], block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let next = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(MD5_K[i])
                    .wrapping_add(m[g])
                    .rotate_left(MD5_S[i]),
            );
            a = d;
            d = c;
            c = b;
            b = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    /// Apply the padding and return the 16-byte digest.
    pub fn finalize(mut self) -> [u8; 16] {
        let bit_len = self.buffer.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffer.used != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_le_bytes());
        debug_assert_eq!(self.buffer.used, 0);
        let mut out = [0u8; 16];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingHash for Md5 {
    fn update(&mut self, data: &[u8]) {
        let state = &mut self.state;
        self.buffer.feed(data, |block| Self::compress(state, block));
    }

    fn finalize_hex(self) -> String {
        to_hex(&self.finalize())
    }
}

// ---------------------------------------------------------------------------
// SHA-1 (FIPS 180-4)
// ---------------------------------------------------------------------------

/// Streaming SHA-1 hasher (FIPS 180-4). See [`StreamingHash`].
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    buffer: BlockBuffer,
}

impl Sha1 {
    pub fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
            buffer: BlockBuffer::new(),
        }
    }

    fn compress(state: &mut [u32; 5], block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a827999u32),
                1 => (b ^ c ^ d, 0x6ed9eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    /// Apply the padding and return the 20-byte digest.
    pub fn finalize(mut self) -> [u8; 20] {
        let bit_len = self.buffer.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffer.used != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        debug_assert_eq!(self.buffer.used, 0);
        let mut out = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingHash for Sha1 {
    fn update(&mut self, data: &[u8]) {
        let state = &mut self.state;
        self.buffer.feed(data, |block| Self::compress(state, block));
    }

    fn finalize_hex(self) -> String {
        to_hex(&self.finalize())
    }
}

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------

/// Round constants: the first 32 bits of the fractional parts of the
/// cube roots of the first 64 primes.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Streaming SHA-256 hasher (FIPS 180-4). See [`StreamingHash`].
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buffer: BlockBuffer,
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: BlockBuffer::new(),
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            state[0], state[1], state[2], state[3], state[4], state[5], state[6], state[7],
        );
        for i in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let tmp1 = h
                .wrapping_add(sum1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let tmp2 = sum0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(tmp1);
            d = c;
            c = b;
            b = a;
            a = tmp1.wrapping_add(tmp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    /// Apply the padding and return the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.buffer.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffer.used != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        debug_assert_eq!(self.buffer.used, 0);
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingHash for Sha256 {
    fn update(&mut self, data: &[u8]) {
        let state = &mut self.state;
        self.buffer.feed(data, |block| Self::compress(state, block));
    }

    fn finalize_hex(self) -> String {
        to_hex(&self.finalize())
    }
}

// ---------------------------------------------------------------------------
// Whole-source helper
// ---------------------------------------------------------------------------

/// The three digests of one byte source, hex-encoded (the file-hash
/// feature's report shape).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHashes {
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

/// Hash `total` bytes from a byte source, read chunk by chunk
/// through [`HASH_WINDOW`]-sized windows so the source is never held
/// in memory whole. `read_at` follows the same contract as
/// [`crate::line_index::LineIndex::build`]: it fills `buf` starting
/// at the given offset and returns how many bytes it filled (0 at
/// the end of the source). A short or empty read stops the scan
/// early and hashes what was actually read.
pub fn hash_byte_source(
    total: u64,
    mut read_at: impl FnMut(u64, &mut [u8]) -> usize,
) -> FileHashes {
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut window = vec![0u8; HASH_WINDOW];
    let mut offset: u64 = 0;
    while offset < total {
        let want = ((total - offset) as usize).min(window.len());
        let read = read_at(offset, &mut window[..want]);
        if read == 0 {
            break;
        }
        let chunk = &window[..read];
        md5.update(chunk);
        sha1.update(chunk);
        sha256.update(chunk);
        offset += read as u64;
    }
    FileHashes {
        md5: md5.finalize_hex(),
        sha1: sha1.finalize_hex(),
        sha256: sha256.finalize_hex(),
    }
}

/// Hash an in-memory byte string with all three algorithms.
pub fn hash_bytes(data: &[u8]) -> FileHashes {
    hash_byte_source(data.len() as u64, |offset, buf| {
        let start = offset as usize;
        if start >= data.len() {
            return 0;
        }
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        n
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 56-byte FIPS example message (the shortest input whose
    /// padding spills into a second block).
    const MSG56: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    /// The 112-byte FIPS example message.
    const MSG112: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";

    #[test]
    fn md5_known_answers() {
        assert_eq!(
            Md5::new().finalize_hex(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        let mut h = Md5::new();
        h.update(b"abc");
        assert_eq!(h.finalize_hex(), "900150983cd24fb0d6963f7d28e17f72");
        let mut h = Md5::new();
        h.update(MSG56);
        assert_eq!(h.finalize_hex(), "8215ef0796a20bcaaae116d3876c664a");
        let mut h = Md5::new();
        h.update(MSG112);
        assert_eq!(h.finalize_hex(), "03dd8807a93175fb062dfb55dc7d359c");
    }

    #[test]
    fn sha1_known_answers() {
        assert_eq!(
            Sha1::new().finalize_hex(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        let mut h = Sha1::new();
        h.update(b"abc");
        assert_eq!(h.finalize_hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let mut h = Sha1::new();
        h.update(MSG56);
        assert_eq!(h.finalize_hex(), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        let mut h = Sha1::new();
        h.update(MSG112);
        assert_eq!(h.finalize_hex(), "a49b2446a02c645bf419f995b67091253a04a259");
    }

    #[test]
    fn sha256_known_answers() {
        assert_eq!(
            Sha256::new().finalize_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            h.finalize_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h = Sha256::new();
        h.update(MSG56);
        assert_eq!(
            h.finalize_hex(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let mut h = Sha256::new();
        h.update(MSG112);
        assert_eq!(
            h.finalize_hex(),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    /// Feeding the million-'a' FIPS vector in awkward chunk sizes
    /// (crossing block boundaries at odd places) must give the same
    /// digests as the published whole-input vectors — this is the
    /// chunked-update equivalence every streaming caller relies on.
    #[test]
    fn million_a_in_odd_chunks_matches_known_answers() {
        let data = vec![b'a'; 1_000_000];
        let mut md5 = Md5::new();
        let mut sha1 = Sha1::new();
        let mut sha256 = Sha256::new();
        let mut pos = 0usize;
        let mut step = 1usize;
        while pos < data.len() {
            let n = step.min(data.len() - pos);
            md5.update(&data[pos..pos + n]);
            sha1.update(&data[pos..pos + n]);
            sha256.update(&data[pos..pos + n]);
            pos += n;
            step = step * 2 + 1; // 1, 3, 7, 15, … then wraps big
            if step > 65_537 {
                step = 999;
            }
        }
        assert_eq!(md5.finalize_hex(), "7707d6ae4e027c70eea2a935c2296f21");
        assert_eq!(
            sha1.finalize_hex(),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
        assert_eq!(
            sha256.finalize_hex(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// One-byte-at-a-time updates agree with a single update.
    #[test]
    fn byte_at_a_time_updates_match_single_update() {
        let mut slow = Sha256::new();
        for &b in MSG112 {
            slow.update(&[b]);
        }
        let mut fast = Sha256::new();
        fast.update(MSG112);
        assert_eq!(slow.finalize_hex(), fast.finalize_hex());
    }

    /// The whole-source helper hashes through windows and agrees
    /// with the single-shot known answers; a source that ends early
    /// hashes the prefix it actually delivered.
    #[test]
    fn hash_byte_source_windows_match_known_answers() {
        let hashes = hash_bytes(MSG56);
        assert_eq!(hashes.md5, "8215ef0796a20bcaaae116d3876c664a");
        assert_eq!(hashes.sha1, "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(
            hashes.sha256,
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // A read_at that serves at most 5 bytes per call still works.
        let data = b"abc";
        let hashes = hash_byte_source(3, |offset, buf| {
            let start = offset as usize;
            let n = buf.len().min(5).min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            n
        });
        assert_eq!(hashes.sha256, hash_bytes(b"abc").sha256);
    }
}
