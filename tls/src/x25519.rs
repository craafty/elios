//! X25519 (RFC 7748): Diffie-Hellman on Curve25519, the key exchange most
//! TLS servers prefer. Field elements mod 2^255 - 19 are five 51-bit limbs.

#[derive(Clone, Copy)]
struct Field([u64; 5]);

const MASK: u64 = (1 << 51) - 1;

impl Field {
    const ZERO: Self = Self([0; 5]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    fn from_bytes(bytes: &[u8; 32]) -> Self {
        let mut limbs = [0; 5];
        for (index, limb) in limbs.iter_mut().enumerate() {
            let bit = index * 51;
            let mut word = [0u8; 16];
            let start = bit / 8;
            let end = (start + 16).min(32);
            word[..end - start].copy_from_slice(&bytes[start..end]);
            *limb = (u128::from_le_bytes(word) >> (bit % 8)) as u64 & MASK;
        }
        Self(limbs)
    }

    /// Brings every limb back under 2^51 (plus a little in limb 0).
    fn carry(mut self) -> Self {
        let l = &mut self.0;
        for index in 0..4 {
            l[index + 1] += l[index] >> 51;
            l[index] &= MASK;
        }
        l[0] += 19 * (l[4] >> 51);
        l[4] &= MASK;
        self
    }

    fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry().carry().0;
        // Subtract p if the value is at least p: add 19 and see whether it
        // carries out of bit 255.
        let mut q = (h[0] + 19) >> 51;
        for limb in &h[1..] {
            q = (limb + q) >> 51;
        }
        h[0] += 19 * q;
        for index in 0..4 {
            h[index + 1] += h[index] >> 51;
            h[index] &= MASK;
        }
        h[4] &= MASK;
        let mut out = [0u8; 32];
        let mut accumulator = 0u128;
        let mut bits = 0;
        let mut position = 0;
        for limb in h {
            accumulator |= u128::from(limb) << bits;
            bits += 51;
            while bits >= 8 && position < 32 {
                out[position] = accumulator as u8;
                accumulator >>= 8;
                bits -= 8;
                position += 1;
            }
        }
        if position < 32 {
            out[position] = accumulator as u8;
        }
        out
    }

    fn add(self, other: Self) -> Self {
        let mut out = self.0;
        for (a, b) in out.iter_mut().zip(other.0) {
            *a += b;
        }
        Self(out).carry()
    }

    fn sub(self, other: Self) -> Self {
        // Add 4p first so nothing goes negative.
        const FOUR_P: [u64; 5] =
            [0x1F_FFFF_FFFF_FFB4, 0x1F_FFFF_FFFF_FFFC, 0x1F_FFFF_FFFF_FFFC, 0x1F_FFFF_FFFF_FFFC, 0x1F_FFFF_FFFF_FFFC];
        let mut out = self.0;
        for ((a, b), p) in out.iter_mut().zip(other.0).zip(FOUR_P) {
            *a = *a + p - b;
        }
        Self(out).carry()
    }

    fn mul(self, other: Self) -> Self {
        let a = self.0.map(u128::from);
        let b = other.0.map(u128::from);
        let b19 = b.map(|limb| limb * 19);
        let t = [
            a[0] * b[0] + a[1] * b19[4] + a[2] * b19[3] + a[3] * b19[2] + a[4] * b19[1],
            a[0] * b[1] + a[1] * b[0] + a[2] * b19[4] + a[3] * b19[3] + a[4] * b19[2],
            a[0] * b[2] + a[1] * b[1] + a[2] * b[0] + a[3] * b19[4] + a[4] * b19[3],
            a[0] * b[3] + a[1] * b[2] + a[2] * b[1] + a[3] * b[0] + a[4] * b19[4],
            a[0] * b[4] + a[1] * b[3] + a[2] * b[2] + a[3] * b[1] + a[4] * b[0],
        ];
        let mut out = [0u64; 5];
        let mut carry = 0u128;
        for (index, value) in t.into_iter().enumerate() {
            let value = value + carry;
            out[index] = value as u64 & MASK;
            carry = value >> 51;
        }
        out[0] += carry as u64 * 19;
        Self(out).carry()
    }

    fn mul_small(self, factor: u64) -> Self {
        self.mul(Self([factor, 0, 0, 0, 0]))
    }

    /// The inverse, as self^(p - 2).
    fn invert(self) -> Self {
        // p - 2 = 2^255 - 21: bits 254..5 set, then 01011.
        let mut result = Self::ONE;
        for bit in (0..255).rev() {
            result = result.mul(result);
            let set = bit >= 5 || (0b01011 >> bit) & 1 != 0;
            if set {
                result = result.mul(self);
            }
        }
        result
    }

    /// Swaps `a` and `b` if `swap` is 1, without branching on it.
    fn swap(swap: u64, a: &mut Self, b: &mut Self) {
        let mask = swap.wrapping_neg();
        for (x, y) in a.0.iter_mut().zip(b.0.iter_mut()) {
            let t = mask & (*x ^ *y);
            *x ^= t;
            *y ^= t;
        }
    }
}

/// The X25519 function: `scalar` times the point with x-coordinate `u`.
pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    let x1 = Field::from_bytes(u);
    let (mut x2, mut z2, mut x3, mut z3) = (Field::ONE, Field::ZERO, x1, Field::ONE);
    let mut swap = 0;
    for t in (0..255).rev() {
        let bit = u64::from(k[t / 8] >> (t % 8) & 1);
        swap ^= bit;
        Field::swap(swap, &mut x2, &mut x3);
        Field::swap(swap, &mut z2, &mut z3);
        swap = bit;
        let a = x2.add(z2);
        let aa = a.mul(a);
        let b = x2.sub(z2);
        let bb = b.mul(b);
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        let sum = da.add(cb);
        x3 = sum.mul(sum);
        let difference = da.sub(cb);
        z3 = x1.mul(difference.mul(difference));
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121_665)));
    }
    Field::swap(swap, &mut x2, &mut x3);
    Field::swap(swap, &mut z2, &mut z3);
    x2.mul(z2.invert()).to_bytes()
}

/// The public key for a private one: the scalar times the base point 9.
pub fn public_key(private: &[u8; 32]) -> [u8; 32] {
    let mut base = [0; 32];
    base[0] = 9;
    x25519(private, &base)
}
