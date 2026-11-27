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
        .map(|text| strip_static_wrappers(&text))
        .and_then(|text| std::fs::write(&out, text));
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{out}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Removes the static members of a runtime class that the generator writes wrongly.
///
/// A static the winmd marks as not throwing (`MidiApi::EnsureServiceAvailable`,
/// `MidiSession::Create`) is written as a function that returns the bare value around a
/// helper that returns a `Result`, which does not compile. The `I*Statics` interface each
/// calls through has the right vtable, and the safe face calls the members through it.
/// A static that returns a `Result` is written right and stays.
///
/// The output is rustfmt's: a member starts at a line `pub fn` and ends at the first
/// later line that is a `}` at its indent.
fn strip_static_wrappers(text: &str) -> String {
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
            let body = &lines[i..=end];
            let signature_end = body
                .iter()
                .position(|l| l.trim_end().ends_with('{'))
                .unwrap_or(0);
            let calls_statics = body.iter().any(|l| {
                l.trim_start().strip_prefix("Self::I").is_some_and(|rest| {
                    rest.contains("Statics") && rest.ends_with("(|this| unsafe {")
                })
            });
            let returns_result = body[..=signature_end].iter().any(|l| l.contains("Result<"));
            if calls_statics && !returns_result {
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
    use super::strip_static_wrappers;

    const CLASS: &str = "\
impl MidiApi {
    pub fn EnsureServiceAvailable() -> bool {
        Self::IMidiApiStatics(|this| unsafe {
            0
        })
    }
    pub fn Throws(
        a: u32,
    ) -> windows_core::Result<u32> {
        Self::IMidiApiStatics(|this| unsafe {
            0
        })
    }
    pub fn Instance(&self) -> bool {
        let this = self;
        unsafe { 0 }
    }
}
";

    /// **A static that returns a bare value is dropped; a static that returns a `Result`, an
    /// instance member and everything around them stay**, so the output is the input less
    /// one member.
    #[test]
    fn only_the_bare_value_statics_are_removed() {
        let out = strip_static_wrappers(CLASS);
        assert!(!out.contains("EnsureServiceAvailable"));
        let expected = CLASS.replacen(
            "    pub fn EnsureServiceAvailable() -> bool {\n        Self::IMidiApiStatics(|this| unsafe {\n            0\n        })\n    }\n",
            "",
            1,
        );
        assert_eq!(out, expected);
    }
}
