//! The command line: `<path> <result-file>`, then the flags in any order, each at
//! most once.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
use tutti_plugin_types::PluginFormat;

/// The usage text.
pub const USAGE: &str = "usage: plugin-probe <plugin-path> <result-file> [--as <format>] \
                         [--measure [--state <file> [--plugin <id>]] [--budget <seconds>]]";

/// A command line that was read.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// The plugin file.
    pub path: PathBuf,
    /// Where the outcome is written.
    pub result: PathBuf,
    /// `--as`: the format the path is read as, in place of its extension's.
    pub format: Option<PluginFormat>,
    /// `--measure`, with what it was given.
    pub measure: Option<Measure>,
}

/// What the command line asks of the measuring step.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Measure {
    /// `--state`: the state to put one plugin in.
    pub state: Option<StateFile>,
    /// `--budget`: how long the whole run may take.
    pub budget: Option<Duration>,
}

/// `--state <file>` and `--plugin <id>`.
#[derive(Debug, PartialEq, Eq)]
pub struct StateFile {
    /// The file the plugin's own `save_state` wrote.
    pub file: PathBuf,
    /// `--plugin`: the descriptor id of the plugin the state is for; `None` is the
    /// file's only plugin.
    pub plugin: Option<String>,
}

/// Why a command line was not read.
#[derive(Debug, PartialEq, Eq)]
pub enum Usage {
    /// The path or the result file is not named, so nothing can say what was wrong.
    NoResultFile,
    /// The flags are wrong; the result file is known.
    Wrong {
        /// Where to say so.
        result: PathBuf,
    },
}

/// Reads the arguments after the program name.
///
/// # Errors
///
/// [`Usage`]: a missing path or result file, a flag that is unknown, repeated,
/// valueless or whose value does not read, `--state`, `--plugin` or `--budget`
/// without `--measure`, or `--plugin` without `--state`.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, Usage> {
    let mut args = args.into_iter();
    let (Some(path), Some(result)) = (args.next(), args.next()) else {
        return Err(Usage::NoResultFile);
    };
    let (path, result) = (PathBuf::from(path), PathBuf::from(result));
    match flags(args) {
        Some((format, measure)) => Ok(Args {
            path,
            result,
            format,
            measure,
        }),
        None => Err(Usage::Wrong { result }),
    }
}

/// The flags, or `None` when they are wrong.
fn flags(
    mut args: impl Iterator<Item = OsString>,
) -> Option<(Option<PluginFormat>, Option<Measure>)> {
    let (mut format, mut measure, mut state, mut plugin, mut budget) =
        (None, false, None, None, None);
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--measure" if !measure => measure = true,
            "--as" if format.is_none() => {
                format = Some(PluginFormat::from_extension_id(args.next()?.to_str()?)?);
            }
            "--state" if state.is_none() => state = Some(PathBuf::from(args.next()?)),
            "--plugin" if plugin.is_none() => plugin = Some(args.next()?.into_string().ok()?),
            "--budget" if budget.is_none() => {
                budget = Some(Duration::from_secs(args.next()?.to_str()?.parse().ok()?));
            }
            _ => return None,
        }
    }
    let state = match (state, plugin) {
        (Some(file), plugin) => Some(StateFile { file, plugin }),
        (None, None) => None,
        (None, Some(_)) => return None,
    };
    if !measure && (state.is_some() || budget.is_some()) {
        return None;
    }
    Some((format, measure.then_some(Measure { state, budget })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &[&str]) -> Result<Args, Usage> {
        parse(line.iter().map(OsString::from))
    }

    fn wrong(result: &str) -> Result<Args, Usage> {
        Err(Usage::Wrong {
            result: result.into(),
        })
    }

    /// **The two names alone are a plain description**: no format, no measuring.
    #[test]
    fn a_path_and_a_result_file_describe_with_the_extensions_format() {
        assert_eq!(
            parse_line(&["Some.clap", "out"]),
            Ok(Args {
                path: "Some.clap".into(),
                result: "out".into(),
                format: None,
                measure: None,
            })
        );
    }

    /// **Every flag is read, with its value.**
    #[test]
    fn every_flag_is_read_with_its_value() {
        let args = parse_line(&[
            "p",
            "r",
            "--as",
            "vst3",
            "--measure",
            "--state",
            "s.bin",
            "--plugin",
            "id-1",
            "--budget",
            "30",
        ])
        .unwrap();
        assert_eq!(args.format, Some(PluginFormat::Vst3));
        assert_eq!(
            args.measure,
            Some(Measure {
                state: Some(StateFile {
                    file: "s.bin".into(),
                    plugin: Some("id-1".into()),
                }),
                budget: Some(Duration::from_secs(30)),
            })
        );
    }

    /// **The order of the flags is not meaningful**: the host's order and its
    /// reverse read as the same line.
    #[test]
    fn the_flags_read_the_same_in_any_order() {
        let forward = parse_line(&["p", "r", "--as", "clap", "--measure", "--budget", "5"]);
        let reverse = parse_line(&["p", "r", "--budget", "5", "--measure", "--as", "clap"]);
        assert!(forward.is_ok());
        assert_eq!(forward, reverse);
    }

    /// **`--measure` alone measures with the plugin as it loads and no budget.**
    #[test]
    fn measure_alone_asks_for_no_state_and_no_budget() {
        let args = parse_line(&["p", "r", "--measure"]).unwrap();
        assert_eq!(args.measure, Some(Measure::default()));
    }

    /// **`--state`, `--plugin` and `--budget` are the measuring's**: without
    /// `--measure` they are refused, and `--plugin` names a plugin for a state.
    #[test]
    fn a_measuring_flag_without_measure_is_wrong() {
        for flags in [
            ["--state", "s"].as_slice(),
            ["--budget", "1"].as_slice(),
            ["--plugin", "id"].as_slice(),
        ] {
            let line: Vec<&str> = ["p", "r"]
                .into_iter()
                .chain(flags.iter().copied())
                .collect();
            assert_eq!(parse_line(&line), wrong("r"), "{flags:?}");
        }
        assert_eq!(
            parse_line(&["p", "r", "--measure", "--plugin", "id"]),
            wrong("r")
        );
    }

    /// **A flag given twice is refused**, not resolved to one of its values.
    #[test]
    fn a_repeated_flag_is_wrong() {
        for flags in [
            ["--as", "clap", "--as", "vst3"].as_slice(),
            ["--measure", "--measure"].as_slice(),
            ["--measure", "--budget", "1", "--budget", "2"].as_slice(),
            ["--measure", "--state", "a", "--state", "b"].as_slice(),
        ] {
            let line: Vec<&str> = ["p", "r"]
                .into_iter()
                .chain(flags.iter().copied())
                .collect();
            assert_eq!(parse_line(&line), wrong("r"), "{flags:?}");
        }
    }

    /// **A value that does not read is refused**: a format the probe does not
    /// know, a budget that is no whole number of seconds, a flag with no value
    /// and a flag that does not exist.
    #[test]
    fn a_value_that_does_not_read_is_wrong() {
        for flags in [
            ["--as", "foo"].as_slice(),
            ["--as"].as_slice(),
            ["--measure", "--budget", "soon"].as_slice(),
            ["--measure", "--budget", "-1"].as_slice(),
            ["--measure", "--state"].as_slice(),
            ["--bogus"].as_slice(),
            ["extra"].as_slice(),
        ] {
            let line: Vec<&str> = ["p", "r"]
                .into_iter()
                .chain(flags.iter().copied())
                .collect();
            assert_eq!(parse_line(&line), wrong("r"), "{flags:?}");
        }
    }

    /// **Without a result file there is nowhere to say what was wrong**, which is
    /// a different error from a wrong flag.
    #[test]
    fn a_line_with_no_result_file_names_none() {
        assert_eq!(parse_line(&[]), Err(Usage::NoResultFile));
        assert_eq!(parse_line(&["only-a-path"]), Err(Usage::NoResultFile));
    }
}
