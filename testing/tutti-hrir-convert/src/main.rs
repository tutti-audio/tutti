//! `tutti-hrir-convert <in.sofa> <out.hrir>`: writes the HRIR sphere
//! `HrtfBinauralNode` loads from a SOFA SimpleFreeFieldHRIR file.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a program: it reports to its user on stderr"
)]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(output), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: tutti-hrir-convert <in.sofa> <out.hrir>");
        return ExitCode::from(2);
    };
    let written = tutti_hrir_convert::convert(&input)
        .map_err(|e| format!("{}: {e}", input.display()))
        .and_then(|sphere| {
            std::fs::write(&output, sphere.to_bytes())
                .map_err(|e| format!("{}: {e}", output.display()))
        });
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
