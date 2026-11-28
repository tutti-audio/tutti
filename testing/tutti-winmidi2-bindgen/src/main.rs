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
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceWatcher",
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceInformationAddedEventArgs",
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceInformationRemovedEventArgs",
    "Windows.Devices.Midi2.Enumeration.MidiEndpointDeviceInformationUpdatedEventArgs",
    "Windows.Devices.Midi2.MidiGroup",
    "Windows.Devices.Midi2.MidiMessageProcessingPluginAddResult",
    "Windows.Devices.Midi2.Transports.Virtual.MidiVirtualDeviceManager",
    "Windows.Devices.Midi2.Transports.Virtual.MidiVirtualDeviceCreationConfig",
    "Windows.Devices.Midi2.Transports.Virtual.MidiVirtualDevice",
    "Windows.Devices.Midi2.Enumeration.MidiDeclaredEndpointInfo",
    "Windows.Devices.Midi2.Enumeration.MidiDeclaredDeviceIdentity",
    "Windows.Devices.Midi2.Enumeration.MidiFunctionBlock",
    "Windows.Devices.Midi2.Enumeration.MidiFunctionBlockDirection",
    "Windows.Devices.Midi2.Enumeration.MidiFunctionBlockUIHint",
    "Windows.Devices.Midi2.Enumeration.MidiFunctionBlockRepresentsMidi10Connection",
];

/// The vtable members the safe face calls (`os/`), by name. A member the generator leaves out
/// because a type it needs is not in [`FILTER`] has no `pub` field, so a filter that lost a
/// type fails here and not in the safe face's build.
const NEEDED: &[&str] = &[
    "EnsureServiceAvailable",
    "TimestampFrequency",
    "TimestampConstantSendImmediately",
    "Create",
    "CreateEndpointConnection",
    "DisconnectEndpointConnection",
    "Open",
    "ConnectionId",
    "SendSingleMessageWordArray",
    "MessageReceived",
    "RemoveMessageReceived",
    "Timestamp",
    "FillWordArray",
    "EndpointDeviceId",
    "Name",
    "FindAll3",
    "DiagnosticsLoopbackAEndpointDeviceId",
    "DiagnosticsLoopbackBEndpointDeviceId",
    "Close",
    "Create2",
    "Start",
    "Stop",
    "Added",
    "Removed",
    "Updated",
    "Stopped",
    "AddedDevice",
    "RemovedDevice",
    "UpdatedDevice",
    "CreateVirtualDevice",
    "DeviceEndpointDeviceId",
    "AddMessageProcessingPlugin",
    "SetCreateOnlyUmpEndpoints",
    "FunctionBlocks",
    "SetNumber",
    "SetName",
    "SetIsActive",
    "SetDirection",
    "SetUIHint",
    "SetRepresentsMidi10Connection",
    "SetFirstGroup",
    "SetGroupCount",
    "SetMidiCIMessageVersionFormat",
    "SetMaxSystemExclusive8Streams",
    "GetDeclaredEndpointInfo",
    "GetDeclaredFunctionBlocks",
    "SupportsMidi10Protocol",
    "SupportsMidi20Protocol",
    "SupportsReceivingJitterReductionTimestamps",
    "SupportsSendingJitterReductionTimestamps",
    "HasStaticFunctionBlocks",
    "DeclaredFunctionBlockCount",
    "SpecificationVersionMajor",
    "SpecificationVersionMinor",
    "Number",
    "IsActive",
    "Direction",
    "UIHint",
    "RepresentsMidi10Connection",
    "FirstGroup",
    "Index",
    "GroupCount",
    "MidiCIMessageVersionFormat",
    "MaxSystemExclusive8Streams",
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(winmd), Some(out), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: tutti-winmidi2-bindgen <Windows.Devices.Midi2.winmd> <out.rs>");
        return ExitCode::from(2);
    };
    // A stale file is not what a failed run leaves behind.
    if let Err(e) = std::fs::remove_file(&out)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("{out}: {e}");
        return ExitCode::FAILURE;
    }
    let mut bindgen = vec!["--in".to_owned(), "default".to_owned(), winmd];
    bindgen.extend(["--out".to_owned(), out.clone(), "--filter".to_owned()]);
    bindgen.extend(FILTER.iter().map(|s| (*s).to_owned()));
    // The generator panics on an error; what it returns is the methods it skipped for a type
    // the filter lacks, which `check` holds to the members the safe face needs.
    eprintln!("{}", windows_bindgen::bindgen(bindgen));
    let written = std::fs::read_to_string(&out)
        .map_err(|e| e.to_string())
        .map(|text| strip_no_throw_members(&text))
        .and_then(|text| check(&text).map(|()| text))
        .and_then(|text| std::fs::write(&out, text).map_err(|e| e.to_string()));
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // No output is better than output the safe face cannot use.
            let _ = std::fs::remove_file(&out);
            eprintln!("{out}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Holds the stripped output to what the safe face relies on: no member that discards its
/// `HRESULT` is left, and every member of [`NEEDED`] has its vtable field.
fn check(text: &str) -> Result<(), String> {
    if text.contains("hresult__") {
        return Err("a member that discards its HRESULT is left in the output".to_owned());
    }
    let missing: Vec<&str> = NEEDED
        .iter()
        .copied()
        .filter(|name| !text.contains(&format!("pub {name}:")))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "no vtable member for {missing:?}: a type they need is not in the filter"
        ))
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
    use super::{check, strip_no_throw_members};

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

    /// **The output is refused when a member that discards its `HRESULT` is left, and when a
    /// needed vtable member is not there**; it passes with neither.
    #[test]
    fn the_output_is_held_to_what_the_safe_face_needs() {
        let all: String = super::NEEDED
            .iter()
            .map(|name| format!("pub {name}: unsafe extern \"system\" fn(),\n"))
            .collect();
        assert_eq!(check(&all), Ok(()));
        let left = format!("{all}let hresult__ = f();\n");
        assert!(check(&left).unwrap_err().contains("discards its HRESULT"));
        let lost = all.replace("pub Open:", "Open: usize,\n//");
        assert!(check(&lost).unwrap_err().contains("\"Open\""));
    }
}
