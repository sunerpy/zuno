//! SHA-1, because that is the hash the oracle's directory names are built from.
//!
//! `packages/core/src/util/hash.ts` defines
//! `Hash.fast(input) = createHash("sha1").update(input).digest("hex")`, and two
//! on-disk names depend on it byte for byte:
//!
//! - the snapshot store, `snapshot/<projectID>/<Hash.fast(worktree)>`
//!   (`packages/opencode/src/snapshot/index.ts:71`),
//! - the non-default model catalog cache, `models-<Hash.fast(source)>.json`
//!   (`packages/core/src/models-dev.ts:162-163`),
//!
//! and one identifier does: the project id derived from a Git remote,
//! `Hash.fast("git-remote:" + normalized)`
//! (`packages/core/src/project.ts:78`).
//!
//! # Why it is implemented here instead of pulled from a crate
//!
//! The workspace pins `sha2` — which is SHA-2 — and no SHA-1 crate. Adding one
//! means editing the root `Cargo.toml`, which this todo may not touch while
//! sibling agents hold it. SHA-1 is a fixed, fully specified 60-line algorithm
//! (FIPS 180-4 §6.1), so implementing it is cheaper and less risky than a
//! concurrent manifest edit. See the project's engineering notes.
//!
//! SHA-1 is used here **only to reproduce directory names**. It is never a
//! security boundary, and nothing in this crate should be read as endorsing it
//! for one.

/// SHA-1 of `input`, lower-case hex — the exact output of the oracle's
/// `Hash.fast`.
///
/// ```
/// use zuno_paths::sha1::hex;
/// assert_eq!(hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
/// ```
#[must_use]
pub fn hex(input: &[u8]) -> String {
    let digest = digest(input);
    let mut out = String::with_capacity(40);
    for byte in digest {
        // Two lower-case hex nibbles per byte, matching Node's `digest("hex")`.
        out.push(nibble(byte >> 4));
        out.push(nibble(byte & 0x0f));
    }
    out
}

fn nibble(value: u8) -> char {
    char::from(match value {
        0..=9 => b'0' + value,
        _ => b'a' + (value - 10),
    })
}

/// The raw 20-byte SHA-1 digest.
#[must_use]
pub fn digest(input: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];

    // Message padding per FIPS 180-4 §5.1.1: a single 0x80 byte, then zeros
    // until the length is 56 mod 64, then the bit length as a big-endian u64.
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let (blocks, remainder) = padded.as_chunks::<64>();
    debug_assert!(remainder.is_empty());
    for block in blocks {
        compress(&mut state, block);
    }

    let mut out = [0u8; 20];
    let (chunks, remainder) = out.as_chunks_mut::<4>();
    debug_assert!(remainder.is_empty());
    for (chunk, word) in chunks.iter_mut().zip(state) {
        *chunk = word.to_be_bytes();
    }
    out
}

/// One 512-bit compression round (FIPS 180-4 §6.1.2).
fn compress(state: &mut [u32; 5], block: &[u8]) {
    debug_assert_eq!(block.len(), 64);

    let mut schedule = [0u32; 80];
    let (words, remainder) = block.as_chunks::<4>();
    debug_assert!(remainder.is_empty());
    for (index, word) in words.iter().enumerate() {
        schedule[index] = u32::from_be_bytes(*word);
    }
    for index in 16..80 {
        let mixed =
            schedule[index - 3] ^ schedule[index - 8] ^ schedule[index - 14] ^ schedule[index - 16];
        schedule[index] = mixed.rotate_left(1);
    }

    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (index, word) in schedule.iter().enumerate() {
        let (mixer, constant) = match index {
            0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999_u32),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(mixer)
            .wrapping_add(e)
            .wrapping_add(constant)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 published vectors plus the empty input, which is the one Node
    /// and every other implementation agree on and which a padding bug breaks
    /// first.
    #[test]
    fn matches_published_vectors() {
        assert_eq!(hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&b"a".repeat(1_000_000)),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    /// The block-boundary lengths, where an off-by-one in the padding loop hides.
    #[test]
    fn handles_block_boundaries() {
        // 55, 56, 63, 64 and 65 bytes: either side of "length is 56 mod 64" and
        // either side of a whole block. Expected values produced by coreutils
        // `sha1sum`, not by reading this implementation back to itself.
        assert_eq!(
            hex(&b"x".repeat(55)),
            "cef734ba81a024479e09eb5a75b6ddae62e6abf1"
        );
        assert_eq!(
            hex(&b"x".repeat(56)),
            "901305367c259952f4e7af8323f480d59f81335b"
        );
        assert_eq!(
            hex(&b"x".repeat(63)),
            "0ddc4e0cccd9a12850deb5abb0853a4425559fec"
        );
        assert_eq!(
            hex(&b"x".repeat(64)),
            "bb2fa3ee7afb9f54c6dfb5d021f14b1ffe40c163"
        );
        assert_eq!(
            hex(&b"x".repeat(65)),
            "78c741ddc482e4cdf8c474a0876347a0905b6233"
        );
    }

    #[test]
    fn digest_and_hex_agree() {
        let raw = digest(b"abc");
        assert_eq!(raw.len(), 20);
        assert_eq!(hex(b"abc").len(), 40);
        let rendered: String = raw.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(rendered, hex(b"abc"));
    }

    /// Every character of the output must be lower-case hex; Node's
    /// `digest("hex")` never emits upper case, and a directory name that
    /// differed in case would silently split a snapshot store on a
    /// case-sensitive filesystem.
    #[test]
    fn output_is_lowercase_hex() {
        let value = hex(b"/config/workspace/ProdDir/AI/opencode-rust");
        assert_eq!(value.len(), 40);
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{value}"
        );
    }
}
