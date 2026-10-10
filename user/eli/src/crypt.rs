//! Password hashing: SHA-512, and the "SHA-512 crypt" scheme (`$6$...`)
//! that /etc/shadow uses on Linux.
//!
//! A stored password is `$6$<salt>$<hash>` (or `$6$rounds=N$<salt>$<hash>`).
//! The scheme runs SHA-512 thousands of times over the password and a random
//! salt, so each guess costs an attacker real work, and the same password
//! hashes differently for every user. Using the standard scheme means
//! eliOS's /etc/shadow works with Linux tools, and the other way around.

use alloc::string::String;
use alloc::vec::Vec;

/// SHA-512 (FIPS 180-4).
pub struct Sha512 {
    state: [u64; 8],
    buffer: [u8; 128],
    buffered: usize,
    length: u128,
}

const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667f3bcc908,
                0xbb67ae8584caa73b,
                0x3c6ef372fe94f82b,
                0xa54ff53a5f1d36f1,
                0x510e527fade682d1,
                0x9b05688c2b3e6c1f,
                0x1f83d9abfb41bd6b,
                0x5be0cd19137e2179,
            ],
            buffer: [0; 128],
            buffered: 0,
            length: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u128;
        while !data.is_empty() {
            let take = (128 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 128 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
    }

    pub fn finish(mut self) -> [u8; 64] {
        let bits = self.length * 8;
        self.update(&[0x80]);
        while self.buffered != 112 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut digest = [0; 64];
        for (chunk, word) in digest.chunks_mut(8).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u8; 128]) {
        let mut w = [0u64; 80];
        for (index, chunk) in block.chunks(8).enumerate() {
            w[index] = u64::from_be_bytes(chunk.try_into().unwrap());
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let choice = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(choice).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (word, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(value);
        }
    }
}

pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut hash = Sha512::new();
    hash.update(data);
    hash.finish()
}

/// The 64 characters crypt hashes and salts are written with.
const ALPHABET: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const DEFAULT_ROUNDS: u32 = 5000;
const MAX_SALT: usize = 16;

/// `bytes` repeated to fill `len` bytes.
fn repeated(bytes: &[u8; 64], len: usize) -> Vec<u8> {
    bytes.iter().copied().cycle().take(len).collect()
}

/// SHA-512 crypt of `password` with `salt` (at most 16 characters are
/// used), as specified by Ulrich Drepper for glibc. `rounds` is the number
/// given in a `rounds=` field, if there was one. Returns the full `$6$...`
/// string.
pub fn sha512_crypt(password: &[u8], salt: &str, explicit_rounds: Option<u32>) -> String {
    let salt = &salt.as_bytes()[..salt.len().min(MAX_SALT)];
    let rounds = explicit_rounds.map_or(DEFAULT_ROUNDS, |rounds| rounds.clamp(1000, 999_999_999));

    let mut alternate = Sha512::new();
    alternate.update(password);
    alternate.update(salt);
    alternate.update(password);
    let alternate = alternate.finish();

    let mut main = Sha512::new();
    main.update(password);
    main.update(salt);
    main.update(&repeated(&alternate, password.len()));
    let mut count = password.len();
    while count > 0 {
        if count & 1 != 0 {
            main.update(&alternate);
        } else {
            main.update(password);
        }
        count >>= 1;
    }
    let mut digest = main.finish();

    let mut password_hash = Sha512::new();
    for _ in 0..password.len() {
        password_hash.update(password);
    }
    let password_sequence = repeated(&password_hash.finish(), password.len());
    let mut salt_hash = Sha512::new();
    for _ in 0..16 + usize::from(digest[0]) {
        salt_hash.update(salt);
    }
    let salt_sequence = repeated(&salt_hash.finish(), salt.len());

    for round in 0..rounds {
        let mut step = Sha512::new();
        if round & 1 != 0 {
            step.update(&password_sequence);
        } else {
            step.update(&digest);
        }
        if round % 3 != 0 {
            step.update(&salt_sequence);
        }
        if round % 7 != 0 {
            step.update(&password_sequence);
        }
        if round & 1 != 0 {
            step.update(&digest);
        } else {
            step.update(&password_sequence);
        }
        digest = step.finish();
    }

    let mut output = String::from("$6$");
    if explicit_rounds.is_some() {
        output.push_str(&alloc::format!("rounds={rounds}$"));
    }
    output.push_str(core::str::from_utf8(salt).unwrap_or_default());
    output.push('$');
    // The digest's bytes are encoded three at a time, in a scrambled order
    // the scheme fixes.
    let mut encode = |high: u8, middle: u8, low: u8, characters: usize| {
        let mut value = (u32::from(high) << 16) | (u32::from(middle) << 8) | u32::from(low);
        for _ in 0..characters {
            output.push(ALPHABET[(value & 63) as usize] as char);
            value >>= 6;
        }
    };
    let mut index = 0;
    for _ in 0..21 {
        let (a, b, c) = (index, (index + 21) % 63, (index + 42) % 63);
        encode(digest[a], digest[b], digest[c], 4);
        index = (index + 22) % 63;
    }
    encode(0, 0, digest[63], 2);
    output
}

/// Hashes a new password with a random salt.
pub fn hash_password(password: &str) -> String {
    let mut random = [0u8; MAX_SALT];
    crate::system::random_bytes(&mut random);
    let salt: String = random.iter().map(|&byte| ALPHABET[usize::from(byte & 63)] as char).collect();
    sha512_crypt(password.as_bytes(), &salt, None)
}

/// Whether `password` matches a stored hash. An empty hash means no
/// password is needed; anything that isn't a `$6$` hash (like `!` or `*`
/// for a locked account) matches nothing.
pub fn verify_password(password: &str, stored: &str) -> bool {
    if stored.is_empty() {
        return true;
    }
    let Some(rest) = stored.strip_prefix("$6$") else { return false };
    let (rounds, rest) = match rest.strip_prefix("rounds=").and_then(|r| r.split_once('$')) {
        Some((rounds, rest)) => match rounds.parse::<u32>() {
            Ok(rounds) => (Some(rounds), rest),
            Err(_) => return false,
        },
        None => (None, rest),
    };
    let Some((salt, _)) = rest.split_once('$') else { return false };
    let computed = sha512_crypt(password.as_bytes(), salt, rounds);
    // Compare every byte, so the time taken doesn't reveal how much matched.
    computed.len() == stored.len() && computed.bytes().zip(stored.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}
