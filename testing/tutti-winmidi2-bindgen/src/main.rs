//! `tutti-winmidi2-bindgen <Windows.Devices.Midi2.winmd> <out.rs>`: writes the bindings
//! `tutti-winmidi2-ffi` checks in as `src/bindings.rs`. `cargo xtask winmidi2-bindings`
//! downloads the winmd, checks its hash and runs this.

use std::process::ExitCode;

/// What the safe face uses. A type's methods whose parameter or result types are not listed
/// are left out by the generator, which names each one on stderr.
const FILTER: &[&str] = &[
    "Windows.Devices.Midi2.MidiApi",
    "Windows.Devices.Midi2.MidiClock",
    "Windows.Devices.Midi2.MidiSession",
    "Windows.Devices.Midi2.MidiEndpointConnection",
    "Windows.Devices.Midi2.MidiMessageReceivedEventArgs",
    "Windows.Devices.Midi2.MidiSendMessageResults",
    "Windows.Devices.Midi2.Diagnostics.MidiDiagnostics",
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceInformation",
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceInformationFilters",
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceInformationSortOrder",
    "Windows.Foundation.TypedEventHandler",
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(winmd), Some(out), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: tutti-winmidi2-bindgen <Windows.Devices.Midi2.winmd> <out.rs>");
        return ExitCode::from(2);
    };
    let mut bindgen = vec!["--in".to_owned(), "default".to_owned(), winmd];
    bindgen.extend(["--out".to_owned(), out.clone(), "--filter".to_owned()]);
    bindgen.extend(FILTER.iter().map(|s| (*s).to_owned()));
    eprintln!("{}", windows_bindgen::bindgen(bindgen));
    let written = std::fs::read_to_string(&out)
        .map(|text| strip_no_throw_members(&text))
        .and_then(|text| std::fs::write(&out, text));
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{out}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Removes the members of the generated types that the generator writes for a member the
/// winmd marks as not throwing, statics and instance members alike.
///
/// Such a member discards its `HRESULT` behind a `debug_assert!` and returns a bare value: a
/// failing call panics a debug build (inside a `Drop`, an abort) and reads zeroed output in a
/// release build. A static is also written wrongly (its helper returns a `Result` around the
/// bare value) and does not compile. The safe face calls every such member through its
/// interface's vtable, which turns a failure into an error. A member that returns a `Result`
/// is written right and stays.
///
/// The output is rustfmt's: a member starts at a line `pub fn` and ends at the first
/// later line that is a `}` at its indent.
fn strip_no_throw_members(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        let indent = lines[i].len() - lines[i].trim_start().len();
        let member = lines[i].trim_start().starts_with("pub fn ");
        let end = if member {
            let close = format!("{}}}", " ".repeat(indent));
            (i..lines.len()).find(|&j| lines[j] == close)
        } else {
            None
        };
        if let Some(end) = end {
            let discards_hresult = lines[i..=end]
                .iter()
                .any(|l| l.trim() == "debug_assert!(hresult__.0 == 0);");
            if discards_hresult {
                i = end + 1;
                continue;
            }
        }
        kept.push_str(lines[i]);
        kept.push('\n');
        i += 1;
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::strip_no_throw_members;

    const CLASS: &str = "\
impl MidiApi {
    pub fn EnsureServiceAvailable() -> bool {
        Self::IMidiApiStatics(|this| unsafe {
            debug_assert!(hresult__.0 == 0);
            result__
        })
    }
    pub fn Throws(
        a: u32,
    ) -> windows_core::Result<u32> {
        Self::IMidiApiStatics(|this| unsafe {
            0
        })
    }
    pub fn Open(&self) -> bool {
        let this = self;
        unsafe {
            let hresult__ = f();
            debug_assert!(hresult__.0 == 0);
            result__
        }
    }
    pub fn Close(&self) -> windows_core::Result<()> {
        unsafe { f().ok() }
    }
}
";

    /// **A member that discards its `HRESULT`, static or instance, is dropped; a member that
    /// returns a `Result` and everything around them stay**, so the output is the input less
    /// the two members.
    #[test]
    fn only_the_members_that_discard_their_hresult_are_removed() {
        let out = strip_no_throw_members(CLASS);
        assert!(!out.contains("EnsureServiceAvailable") && !out.contains("fn Open"));
        let expected = "\
impl MidiApi {
    pub fn Throws(
        a: u32,
    ) -> windows_core::Result<u32> {
        Self::IMidiApiStatics(|this| unsafe {
            0
        })
    }
    pub fn Close(&self) -> windows_core::Result<()> {
        unsafe { f().ok() }
    }
}
";
        assert_eq!(out, expected);
    }
}
