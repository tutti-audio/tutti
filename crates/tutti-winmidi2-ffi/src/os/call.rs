//! Calls through an interface's vtable.
//!
//! The members the winmd marks as not throwing are written by `windows-bindgen` to discard
//! their `HRESULT` behind a `debug_assert!` (statics also do not compile), so the bindings
//! leave them out. Every one the safe face needs is called here, through the vtable of the
//! interface it belongs to, so a failure is an `Error` and never a panic or a zeroed value.

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

/// Calls the member `$method` of the interface `$interface` (any interface or class whose
/// vtable has it: a class's is its default interface's) with `$arg`s, and returns what it
/// writes through its last parameter as a `$out`, or the `HRESULT` it failed with as an
/// `Error::Call` named `$name`. Without `-> $out` the member writes nothing.
///
/// `$out` is the type that is `Default` and has the layout of what the member writes: a
/// `bool`, an integer, a `GUID`, or an `Option` of an interface or an `HSTRING` (a null
/// pointer when empty). Each `$arg` is a value of the type the vtable's parameter has, in its
/// order, a slice being its length and pointer.
macro_rules! call {
    ($name:literal, $interface:expr, $method:ident($($arg:expr),*) -> $out:ty) => {{
        let interface = &$interface;
        let mut out = <$out>::default();
        // SAFETY: `interface` is live and has the vtable's type, so `as_raw` is the `this` its
        // methods take. `out` is written once through the last parameter, a pointer to
        // storage of its layout: the callee writes a scalar, or a null or an owned reference
        // that `out` then owns (`Option<interface>` and `HSTRING` are one pointer, null empty).
        // A failed call leaves `out` as it was. Each `$arg` is passed as the parameter's type.
        let code = unsafe {
            (windows_core::Interface::vtable(interface).$method)(
                windows_core::Interface::as_raw(interface),
                $($arg,)*
                (&raw mut out).cast(),
            )
        };
        code.ok()
            .map(|()| out)
            .map_err($crate::os::error::call($name))
    }};
    ($name:literal, $interface:expr, $method:ident($($arg:expr),*)) => {{
        let interface = &$interface;
        // SAFETY: as above, with no result written.
        let code = unsafe {
            (windows_core::Interface::vtable(interface).$method)(
                windows_core::Interface::as_raw(interface),
                $($arg),*
            )
        };
        code.ok().map_err($crate::os::error::call($name))
    }};
}

pub(super) use call;
