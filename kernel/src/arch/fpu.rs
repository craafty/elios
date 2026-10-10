//! Floating point and SSE state.
//!
//! The kernel is compiled without floating point, so these registers only
//! ever hold user programs' values. They're saved and restored on every
//! thread switch. Only the legacy (FXSAVE) state is managed, so the CPU's
//! XSAVE features stay disabled; that tells programs not to use AVX.

use core::arch::asm;

use crate::sync::Once;

/// The FXSAVE area: x87, MMX and SSE registers.
#[repr(C, align(16))]
#[derive(Clone)]
pub struct FpuState([u8; 512]);

static INITIAL_STATE: Once<FpuState> = Once::new();

/// Enables SSE for user programs and records the clean register state new
/// programs start with.
pub fn init() {
    init_cpu();
    let mut state = FpuState([0; 512]);
    save(&mut state);
    // The default MXCSR: all SIMD exceptions masked, round to nearest.
    state.0[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
    let _ = INITIAL_STATE.set(state);
}

/// Enables the FPU and SSE on this CPU.
pub fn init_cpu() {
    // SAFETY: CR0.EM off and CR0.MP on enable the FPU; CR4.OSFXSR and
    // OSXMMEXCPT enable SSE and its exceptions.
    unsafe {
        asm!(
            "mov {tmp}, cr0",
            "and {tmp}, ~(1 << 2)",
            "or {tmp}, 1 << 1",
            "mov cr0, {tmp}",
            "mov {tmp}, cr4",
            "or {tmp}, (1 << 9) | (1 << 10)",
            "mov cr4, {tmp}",
            "fninit",
            tmp = out(reg) _,
            options(nostack),
        );
    }
}

impl FpuState {
    /// The register state a fresh program starts with.
    pub fn initial() -> Self {
        INITIAL_STATE.get().expect("FPU not initialized").clone()
    }

    /// Takes a state saved in user memory (by a signal handler, say).
    /// MXCSR's reserved bits are cleared: fxrstor faults on them.
    pub fn load(&mut self, bytes: &[u8]) {
        self.0.copy_from_slice(&bytes[..512]);
        let mxcsr = u32::from_le_bytes(self.0[24..28].try_into().unwrap()) & 0xFFFF;
        self.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    }
}

/// Saves the FPU registers to arbitrary (user) memory.
///
/// # Safety
/// `address` must be 16-byte aligned and valid for 512 bytes of writes.
pub unsafe fn save_to(address: *mut u8) {
    unsafe { asm!("fxsave64 [{}]", in(reg) address, options(nostack, preserves_flags)) };
}

pub fn save(state: &mut FpuState) {
    // SAFETY: the area is 512 bytes and 16-byte aligned, as fxsave requires.
    unsafe { asm!("fxsave64 [{}]", in(reg) state.0.as_mut_ptr(), options(nostack, preserves_flags)) };
}

pub fn restore(state: &FpuState) {
    // SAFETY: the area holds a state produced by fxsave (or the initial one).
    unsafe { asm!("fxrstor64 [{}]", in(reg) state.0.as_ptr(), options(nostack, preserves_flags)) };
}
