/// The flush bits of the thread's control register, whole register for
/// the comparison.
pub(super) const FLUSH: u64 = 1 << 24;

pub(super) fn read_reg() -> u64 {
    let val: u64;
    // SAFETY: reads FPCR into a register, no memory or flags.
    unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) val) };
    val
}
