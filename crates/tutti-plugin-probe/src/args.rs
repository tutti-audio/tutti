//! The command line: `<path> <result-file>`, then the flags in any order, each at
//! most once.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tutti_plugin_types::{PluginFormat, PluginId};

/// The usage text.
pub(crate) const USAGE: &str = "usage: plugin-probe <plugin-path> <result-file> [--as <format>] \
                         [--measure [--state <file> [--plugin <id>]] [--budget <seconds>]]";

/// A command line that was read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Args {
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
pub(crate) struct Measure {
    /// `--state`: the state to put one plugin in.
    pub state: Option<StateFile>,
    /// `--budget`: how long the whole run may take.
    pub budget: Option<Duration>,
}

/// `--state <file>` and `--plugin <id>`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StateFile {
    /// The file the plugin's own `save_state` wrote.
    pub file: PathBuf,
    /// `--plugin`: the id of the plugin the state is for; `None` is the file's
    /// only plugin.
    pub plugin: Option<PluginId>,
}

/// Why a command line was not read.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum Usage {
    /// The path or the result file is not named, so nothing can say what was wrong.
    #[error("the plugin path and the result file are required")]
    NoResultFile,
    /// The flags are wrong; the result file is known.
    #[error("{cause}")]
    Wrong {
        /// Where to say so.
        result: PathBuf,
        /// What is wrong.
        cause: Flag,
    },
}

/// What is wrong with a flag.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum Flag {
    /// An argument that is no flag the probe has.
    #[error("unknown argument {0:?}")]
    Unknown(String),
    /// A flag given twice.
    #[error("{0} is given twice")]
    Repeated(&'static str),
    /// A flag that takes a value and has none.
    #[error("{0} needs a value")]
    MissingValue(&'static str),
    /// A value that is not text, or that the flag does not read.
    #[error("{0} cannot read {1:?}")]
    BadValue(&'static str, String),
    /// `--state` or `--budget` without `--measure`.
    #[error("{0} is a measuring flag and needs --measure")]
    NeedsMeasure(&'static str),
    /// `--plugin` without `--state`.
    #[error("--plugin names the plugin of a --state")]
    NeedsState,
}

/// Reads the arguments after the program name.
///
/// # Errors
///
/// [`Usage`]: a missing path or result file, or a [`Flag`] that is unknown,
/// repeated, valueless or whose value does not read, `--state`, `--plugin` or
/// `--budget` without `--measure`, or `--plugin` without `--state`.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, Usage> {
    let mut args = args.into_iter();
    let (Some(path), Some(result)) = (args.next(), args.next()) else {
        return Err(Usage::NoResultFile);
    };
    let (path, result) = (PathBuf::from(path), PathBuf::from(result));
    match flags(args) {
        Ok((format, measure)) => Ok(Args {
            path,
            result,
            format,
            measure,
        }),
        Err(cause) => Err(Usage::Wrong { result, cause }),
    }
}

/// The value after `flag`.
fn value(args: &mut impl Iterator<Item = OsString>, flag: &'static str) -> Result<OsString, Flag> {
    args.next().ok_or(Flag::MissingValue(flag))
}

/// The text of `value`, read by `read`.
fn read<T>(
    flag: &'static str,
    value: &OsString,
    read: impl FnOnce(&str) -> Option<T>,
) -> Result<T, Flag> {
    value
        .to_str()
        .and_then(read)
        .ok_or_else(|| Flag::BadValue(flag, value.to_string_lossy().into_owned()))
}

/// The flags, or what is wrong with them.
fn flags(
    mut args: impl Iterator<Item = OsString>,
) -> Result<(Option<PluginFormat>, Option<Measure>), Flag> {
    let (mut format, mut measure, mut state, mut plugin, mut budget) =
        (None, false, None, None, None);
    while let Some(flag) = args.next() {
        let Some(text) = flag.to_str() else {
            return Err(Flag::Unknown(flag.to_string_lossy().into_owned()));
        };
        match text {
            "--measure" => {
                if std::mem::replace(&mut measure, true) {
                    return Err(Flag::Repeated("--measure"));
                }
            }
            "--as" => {
                if format.is_some() {
                    return Err(Flag::Repeated("--as"));
                }
                let v = value(&mut args, "--as")?;
                format = Some(read("--as", &v, PluginFormat::from_extension_id)?);
            }
            "--state" => {
                if state.is_some() {
                    return Err(Flag::Repeated("--state"));
                }
                state = Some(PathBuf::from(value(&mut args, "--state")?));
            }
            "--plugin" => {
                if plugin.is_some() {
                    return Err(Flag::Repeated("--plugin"));
                }
                let v = value(&mut args, "--plugin")?;
                plugin = Some(read("--plugin", &v, |s| s.parse().ok())?);
            }
            "--budget" => {
                if budget.is_some() {
                    return Err(Flag::Repeated("--budget"));
                }
                let v = value(&mut args, "--budget")?;
                budget = Some(Duration::from_secs(read("--budget", &v, |s| {
                    s.parse().ok()
                })?));
            }
            other => return Err(Flag::Unknown(other.to_owned())),
        }
    }
    if !measure {
        if state.is_some() {
            return Err(Flag::NeedsMeasure("--state"));
        }
        if budget.is_some() {
            return Err(Flag::NeedsMeasure("--budget"));
        }
    }
    let state = match (state, plugin) {
        (Some(file), plugin) => Some(StateFile { file, plugin }),
        (None, None) => None,
        (None, Some(_)) => return Err(Flag::NeedsState),
    };
    Ok((format, measure.then_some(Measure { state, budget })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &[&str]) -> Result<Args, Usage> {
        parse(line.iter().map(OsString::from))
    }

    fn wrong(cause: Flag) -> Result<Args, Usage> {
        Err(Usage::Wrong {
            result: "r".into(),
            cause,
        })
    }

    fn flags_after_names(flags: &[&str]) -> Result<Args, Usage> {
        let line: Vec<&str> = ["p", "r"]
            .into_iter()
            .chain(flags.iter().copied())
            .collect();
        parse_line(&line)
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
            "clap.id-1",
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
                    plugin: Some(PluginId::Clap("id-1".into())),
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

    /// **`--state` and `--budget` are the measuring's**: without `--measure` they are
    /// refused, and `--plugin` names the plugin of a `--state`.
    #[test]
    fn a_measuring_flag_without_measure_is_wrong() {
        assert_eq!(
            flags_after_names(&["--state", "s"]),
            wrong(Flag::NeedsMeasure("--state"))
        );
        assert_eq!(
            flags_after_names(&["--budget", "1"]),
            wrong(Flag::NeedsMeasure("--budget"))
        );
        assert_eq!(
            flags_after_names(&["--measure", "--plugin", "clap.id"]),
            wrong(Flag::NeedsState)
        );
        assert_eq!(
            flags_after_names(&["--plugin", "clap.id"]),
            wrong(Flag::NeedsState)
        );
    }

    /// **A flag given twice is refused**, not resolved to one of its values.
    #[test]
    fn a_repeated_flag_is_wrong() {
        for (flags, flag) in [
            (["--as", "clap", "--as", "vst3"].as_slice(), "--as"),
            (["--measure", "--measure"].as_slice(), "--measure"),
            (
                ["--measure", "--budget", "1", "--budget", "2"].as_slice(),
                "--budget",
            ),
            (
                ["--measure", "--state", "a", "--state", "b"].as_slice(),
                "--state",
            ),
            (
                [
                    "--measure",
                    "--state",
                    "a",
                    "--plugin",
                    "clap.x",
                    "--plugin",
                    "clap.y",
                ]
                .as_slice(),
                "--plugin",
            ),
        ] {
            assert_eq!(
                flags_after_names(flags),
                wrong(Flag::Repeated(flag)),
                "{flags:?}"
            );
        }
    }

    /// **A value that does not read is refused by name**: a format the probe does not
    /// know, a budget that is no whole number of seconds, a flag with no value and a
    /// flag that does not exist.
    #[test]
    fn a_value_that_does_not_read_is_wrong() {
        assert_eq!(
            flags_after_names(&["--as", "foo"]),
            wrong(Flag::BadValue("--as", "foo".into()))
        );
        assert_eq!(
            flags_after_names(&["--as"]),
            wrong(Flag::MissingValue("--as"))
        );
        assert_eq!(
            flags_after_names(&["--measure", "--state", "s", "--plugin", "synth"]),
            wrong(Flag::BadValue("--plugin", "synth".into())),
            "a plugin id starts with its format"
        );
        assert_eq!(
            flags_after_names(&["--measure", "--budget", "soon"]),
            wrong(Flag::BadValue("--budget", "soon".into()))
        );
        assert_eq!(
            flags_after_names(&["--measure", "--budget", "-1"]),
            wrong(Flag::BadValue("--budget", "-1".into()))
        );
        assert_eq!(
            flags_after_names(&["--measure", "--state"]),
            wrong(Flag::MissingValue("--state"))
        );
        assert_eq!(
            flags_after_names(&["--bogus"]),
            wrong(Flag::Unknown("--bogus".into()))
        );
        assert_eq!(
            flags_after_names(&["extra"]),
            wrong(Flag::Unknown("extra".into()))
        );
    }

    /// **Without a result file there is nowhere to say what was wrong**, which is
    /// a different error from a wrong flag.
    #[test]
    fn a_line_with_no_result_file_names_none() {
        assert_eq!(parse_line(&[]), Err(Usage::NoResultFile));
        assert_eq!(parse_line(&["only-a-path"]), Err(Usage::NoResultFile));
    }
}
