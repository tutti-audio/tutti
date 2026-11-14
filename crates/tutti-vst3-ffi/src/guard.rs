//! [`Out`], the out-parameter a COM method writes through. The panic guard the
//! methods run their bodies in is
//! [`ffi_guard`](tutti_plugin_types::ffi_guard).

/// An out-parameter a plugin passed to a COM method: null (the plugin wants no
/// answer) or valid for one write for the length of the call.
///
/// It is the one place the host writes through a plugin's out-pointer, so the
/// null check and the SAFETY argument are made once. Build it before the
/// [`ffi_guard`](tutti_plugin_types::ffi_guard) and set it to the "nothing" answer first, so a panic in the body
/// leaves the plugin a defined value.
pub(crate) struct Out<T>(*mut T);

impl<T> Out<T> {
    /// # Safety
    ///
    /// `ptr` is null, or valid for a write of `T` for as long as this `Out`
    /// lives (the COM ABI gives that for the duration of the method call the
    /// `Out` is built in).
    pub(crate) unsafe fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    /// Whether the plugin passed no pointer.
    pub(crate) fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// Writes `value` if the plugin passed a pointer.
    pub(crate) fn set(&self, value: T) {
        if !self.0.is_null() {
            // SAFETY: non-null, and valid for a write by `new`'s contract.
            unsafe { self.0.write(value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Out;

    #[test]
    fn an_out_writes_through_a_pointer_and_skips_a_null_one() {
        let mut slot = 0;
        // SAFETY: `slot` outlives `out`.
        let out = unsafe { Out::new(&mut slot) };
        out.set(7);
        assert_eq!(slot, 7);
        // SAFETY: null is allowed.
        unsafe { Out::<i32>::new(std::ptr::null_mut()) }.set(9);
    }
}
