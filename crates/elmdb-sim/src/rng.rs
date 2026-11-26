//! Deterministic xoshiro256++ PRNG seeded by a 64-bit seed (SplitMix64 expansion).

#[derive(Debug, Clone)]
pub struct SimRng {
    state: [u64; 4],
}

impl SimRng {
    pub fn seeded(mut seed: u64) -> Self {
        let mut state = [0u64; 4];
        for slot in &mut state {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            *slot = z ^ (z >> 31);
        }
        Self { state }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = (self.state[0].wrapping_add(self.state[3])).rotate_left(23).wrapping_add(self.state[0]);
        let t = self.state[1] << 17;
        self.state[2] ^= self.state[0];
        self.state[3] ^= self.state[1];
        self.state[1] ^= self.state[2];
        self.state[0] ^= self.state[3];
        self.state[2] ^= t;
        self.state[3] = self.state[3].rotate_left(45);
        result
    }

    pub fn gen_range(&mut self, upper_exclusive: usize) -> usize {
        assert!(upper_exclusive > 0, "upper_exclusive must be > 0");
        (self.next_u64() as usize) % upper_exclusive
    }

    pub fn gen_bool(&mut self, numerator: u32, denominator: u32) -> bool {
        assert!(denominator > 0 && numerator <= denominator);
        ((self.next_u64() % u64::from(denominator)) as u32) < numerator
    }

    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}
