/// The flush bits of the thread's control register, whole register for
/// the comparison.
pub(super) const FLUSH: u64 = 0x8040;

pub(super) fn read_reg() -> u64 {
    let mut val: u32 = 0;
    // SAFETY: `val` is a live `u32`, the four bytes `stmxcsr` stores.
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut val, options(nostack)) };
    u64::from(val)
}
