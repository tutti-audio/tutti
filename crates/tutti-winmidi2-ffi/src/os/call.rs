//! Calls through the `I*Statics` interfaces.
//!
//! `windows-bindgen` writes a static member the winmd marks as not throwing wrongly (its
//! helper returns a `Result` around a bare value), and the bindings leave those members
//! out. The interface each belongs to is generated right, so a member is called through
//! its vtable here.

use super::Error;

/// The factory of the runtime class `C`, as its `I` statics interface.
pub(super) fn statics<C: windows_core::RuntimeName, I: windows_core::Interface>() -> Result<I, Error>
{
    windows_core::factory::<C, I>().map_err(|e| Error::ApiMissing { code: e.code().0 })
}

/// The ABI value of `name`, for the `HSTRING` parameter of a vtable call: a borrow of its
/// handle, valid while `name` lives.
pub(super) fn abi(name: &windows_core::HSTRING) -> *mut core::ffi::c_void {
    // SAFETY: an `HSTRING` is one pointer, the handle its ABI passes. The copy is not an owner
    // (nothing releases it) and the callee only reads it during the call that borrows `name`.
    unsafe { core::mem::transmute_copy(name) }
}

/// Calls the static member `$method` of the interface `$statics` with `$arg`s, and
/// returns what it writes through its last parameter as a `$out`, or the `HRESULT` it
/// failed with as an `Error::Call` named `$name`.
///
/// `$out` is the type that is `Default` and has the layout of what the member writes: a
/// `bool`, an integer, or an `Option` of an interface or an `HSTRING` (a null pointer when
/// empty).
macro_rules! call {
    ($name:literal, $statics:expr, $method:ident($($arg:expr),*) -> $out:ty) => {{
        let statics = &$statics;
        let mut out = <$out>::default();
        // SAFETY: `statics` is a live interface of the vtable's type, so `as_raw` is the `this`
        // its methods take. `out` is written once through the last parameter, a pointer to
        // storage of its layout: the callee writes a scalar, or a null or an owned reference
        // that `out` then owns (`Option<interface>` and `HSTRING` are one pointer, null empty).
        // A failed call leaves `out` as it was.
        let code = unsafe {
            (windows_core::Interface::vtable(statics).$method)(
                windows_core::Interface::as_raw(statics),
                $($arg,)*
                (&raw mut out).cast(),
            )
        };
        code.ok()
            .map(|()| out)
            .map_err(super::error::call($name))
    }};
}

pub(super) use call;
