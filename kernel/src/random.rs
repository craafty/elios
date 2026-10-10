//! Random numbers, from the CPU's generator when it has one, otherwise a
//! xorshift generator seeded from the timestamp counter.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::arch;
use crate::sync::SpinLock;

static STATE: SpinLock<u64> = SpinLock::new(0);
/// 0 = not checked yet, 1 = no RDRAND, 2 = RDRAND available.
static HARDWARE: AtomicU8 = AtomicU8::new(0);

fn next() -> u64 {
    if HARDWARE.load(Ordering::Relaxed) != 1 {
        match arch::hardware_random() {
            Some(value) => {
                HARDWARE.store(2, Ordering::Relaxed);
                return value;
            }
            None => HARDWARE.store(1, Ordering::Relaxed),
        }
    }
    let mut state = STATE.lock();
    if *state == 0 {
        *state = arch::rdtsc() | 1;
    }
    // Mix in the clock so outputs differ between boots and over time.
    *state ^= arch::rdtsc().rotate_left(17);
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

pub fn fill(buffer: &mut [u8]) {
    for chunk in buffer.chunks_mut(8) {
        let bytes = next().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}
