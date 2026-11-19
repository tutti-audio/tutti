//! The contract against CLAP.

tutti_plugin_contract::contract!(
    tutti_plugin_contract::Clap,
    gaps = 1,
    hostings = [InProc, OutOfProcess]
);
