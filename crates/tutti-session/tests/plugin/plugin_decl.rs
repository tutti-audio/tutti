//! A plugin declared as a node of a patch, in a real `plugin-server`. The cases
//! are `support/plugin_decl.rs`'s, which `tests/main_thread` runs in process.

use tutti_plugin_db::Hosting;

macro_rules! served {
    ($($case:ident,)*) => {$(
        #[test]
        fn $case() {
            crate::decl::$case(Hosting::OutOfProcess);
        }
    )*};
}

crate::decl::cases!(served);
