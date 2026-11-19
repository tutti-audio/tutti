//! The ids and names of the reference shell plugin (`tutti-vst2-shell-plugin`), spelt
//! once for the plugin and for the tests that hold a host to it: a test cannot link the
//! plugin's own crate beside another plugin, because both export `VSTPluginMain`.

/// The shell's own `uniqueID` ("SHEL"): the plugin the host gets when it names none.
pub const SHELL_ID: i32 = i32::from_be_bytes(*b"SHEL");
/// The first sub-plugin ("SHAL"): an effect.
pub const ALPHA_ID: i32 = i32::from_be_bytes(*b"SHAL");
/// The second sub-plugin ("SHBE"): an instrument.
pub const BETA_ID: i32 = i32::from_be_bytes(*b"SHBE");

/// The shell's name, as `effGetEffectName` answers it.
pub const SHELL_NAME: &str = "Tutti VST2 Shell";
/// The first sub-plugin's name.
pub const ALPHA_NAME: &str = "Shell Alpha";
/// The second sub-plugin's name.
pub const BETA_NAME: &str = "Shell Beta";
