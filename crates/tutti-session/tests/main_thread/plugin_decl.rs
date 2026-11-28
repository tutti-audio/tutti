//! A plugin declared as a node of a patch, hosted in process on the main thread.
//! The cases are `support/plugin_decl.rs`'s, which `tests/plugin` runs against a
//! real `plugin-server`.

use tutti_plugin_db::Hosting;

macro_rules! in_process {
    ($($case:ident,)*) => {
        pub(crate) const CASES: &[(&str, fn())] = &[$((
            stringify!($case),
            || crate::decl::$case(Hosting::InProcess),
        ),)*];
    };
}

crate::decl::cases!(in_process);
