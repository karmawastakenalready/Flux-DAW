//! Flush-to-zero for DSP threads. Filters, reverbs and neural amp models decaying into silence produce
//! subnormal numbers, which x86 processors handle up to a hundred times slower than normal ones; flushing
//! them to zero changes results only below 1e-38.

/// Flush-to-zero and denormals-are-zero on this thread until dropped, then the previous mode again.
pub struct FlushDenormals {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    saved: u64,
}

impl FlushDenormals {
    #[allow(clippy::new_without_default)]
    pub fn new() -> FlushDenormals {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let saved = arch::get();
            arch::set(saved | arch::FLUSH);
            FlushDenormals { saved }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        FlushDenormals {}
    }
}

impl Drop for FlushDenormals {
    fn drop(&mut self) {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        arch::set(self.saved);
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    /// MXCSR flush-to-zero (bit 15) and denormals-are-zero (bit 6).
    pub const FLUSH: u64 = (1 << 15) | (1 << 6);

    pub fn get() -> u64 {
        let mut csr: u32 = 0;
        // SAFETY: stores MXCSR into a local.
        unsafe { std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut csr, options(nostack, preserves_flags)) };
        csr as u64
    }

    pub fn set(v: u64) {
        let csr = v as u32;
        // SAFETY: loads a value read from MXCSR with only the FTZ/DAZ bits changed.
        unsafe { std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const csr, options(nostack, preserves_flags, readonly)) };
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    /// FPCR flush-to-zero (bit 24).
    pub const FLUSH: u64 = 1 << 24;

    pub fn get() -> u64 {
        let v: u64;
        // SAFETY: reads FPCR.
        unsafe { std::arch::asm!("mrs {}, fpcr", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    pub fn set(v: u64) {
        // SAFETY: writes FPCR with only the FZ bit changed.
        unsafe { std::arch::asm!("msr fpcr, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnormals_flush_while_held_and_not_after() {
        use std::hint::black_box;
        // fresh opaque operands each time, so the products are computed where they stand
        let half_of_smallest = || black_box(black_box(f32::MIN_POSITIVE) * black_box(0.5f32));
        assert!(half_of_smallest() > 0.0, "subnormals exist by default");
        {
            let _f = FlushDenormals::new();
            if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
                assert_eq!(half_of_smallest(), 0.0);
            }
        }
        assert!(half_of_smallest() > 0.0, "the mode is restored");
    }
}
