//! SipHash-1-3 with a 128-bit key: the keyed hash that places keys in slot
//! buckets. The key is a random seed stored in each file, so a party that
//! chooses keys cannot aim them at one bucket without knowing the seed.

/// Hashes `parts` as one concatenated message.
pub(crate) fn hash(seed: [u64; 2], parts: &[&[u8]]) -> u64 {
    let mut hasher = Sip::new(seed);
    for part in parts {
        hasher.write(part);
    }
    hasher.finish()
}

/// An incremental hash: [`Self::write`] any number of pieces, then
/// [`Self::finish`]. The result depends only on the concatenation.
pub(crate) struct Sip {
    state: State,
    tail: [u8; 8],
    filled: usize,
    total: u64,
}

impl Sip {
    pub(crate) const fn new(seed: [u64; 2]) -> Self {
        Self {
            state: State::new(seed),
            tail: [0; 8],
            filled: 0,
            total: 0,
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.tail[self.filled] = byte;
            self.filled += 1;
            if self.filled == 8 {
                self.state.compress(u64::from_le_bytes(self.tail));
                self.filled = 0;
            }
        }
        self.total = self.total.wrapping_add(bytes.len() as u64);
    }

    pub(crate) fn finish(mut self) -> u64 {
        let mut last = (self.total & 0xff) << 56;
        for (index, byte) in self.tail[..self.filled].iter().enumerate() {
            last |= u64::from(*byte) << (8 * index);
        }
        self.state.compress(last);
        self.state.finish()
    }
}

struct State {
    v: [u64; 4],
}

impl State {
    const fn new(seed: [u64; 2]) -> Self {
        Self {
            v: [
                seed[0] ^ 0x736f_6d65_7073_6575,
                seed[1] ^ 0x646f_7261_6e64_6f6d,
                seed[0] ^ 0x6c79_6765_6e65_7261,
                seed[1] ^ 0x7465_6462_7974_6573,
            ],
        }
    }

    fn round(&mut self) {
        let v = &mut self.v;
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13);
        v[1] ^= v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16);
        v[3] ^= v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21);
        v[3] ^= v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17);
        v[1] ^= v[2];
        v[2] = v[2].rotate_left(32);
    }

    fn compress(&mut self, word: u64) {
        self.v[3] ^= word;
        self.round();
        self.v[0] ^= word;
    }

    fn finish(mut self) -> u64 {
        self.v[2] ^= 0xff;
        self.round();
        self.round();
        self.round();
        self.v[0] ^ self.v[1] ^ self.v[2] ^ self.v[3]
    }
}

#[cfg(test)]
mod tests {
    use super::hash;

    /// The reference SipHash-1-3 vectors (key `00..0f`, message `00..n`):
    /// a file's buckets must be found the same on every platform.
    #[test]
    fn matches_the_reference_vectors_on_every_byte_order() {
        let seed = [0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908];
        for (len, expected) in [
            (0usize, 0xabac_0158_050f_c4dc_u64),
            (1, 0xc9f4_9bf3_7d57_ca93),
            (7, 0xd392_7d98_9bb1_1140),
            (8, 0x3690_9511_8d29_9a8e),
            (15, 0xd320_d86d_2a51_9956),
            (63, 0x9d19_9062_b7bb_b3a8),
        ] {
            let message: Vec<u8> = (0..len as u8).collect();
            assert_eq!(hash(seed, &[&message]), expected, "length {len}");
        }
    }

    #[test]
    fn splitting_a_message_does_not_change_its_hash() {
        let seed = [0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908];
        let message: Vec<u8> = (0u8..40).collect();
        let whole = hash(seed, &[&message]);
        for cut in 0..message.len() {
            assert_eq!(hash(seed, &[&message[..cut], &message[cut..]]), whole);
        }
    }

    #[test]
    fn the_seed_and_every_byte_change_the_hash() {
        let seed = [1, 2];
        let base = hash(seed, &[b"key"]);
        assert_ne!(hash([1, 3], &[b"key"]), base);
        assert_ne!(hash(seed, &[b"kez"]), base);
        assert_ne!(hash(seed, &[b"key", b"\0"]), base);
        assert_ne!(hash(seed, &[b""]), hash(seed, &[b"\0"]));
    }
}
