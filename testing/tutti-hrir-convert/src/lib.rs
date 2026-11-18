//! Converts a SOFA (AES69) SimpleFreeFieldHRIR file to a tutti [`HrirSphere`].
//!
//! [`convert`] reads the file's impulse responses, directions and delays and
//! builds the sphere with [`HrirSphere::new`], which triangulates the
//! directions and applies the checks the renderer's loader applies. Whatever
//! the file holds that the sphere cannot represent is refused with a
//! [`SofaError`] naming it, never approximated:
//!
//! - the convention must be SimpleFreeFieldHRIR: one source direction per
//!   measurement, two receivers (receiver 0 the left ear, receiver 1 the
//!   right), one sampling rate;
//! - a `Data.Delay` of whole samples is prepended to its ear's response as
//!   zeros, and every response is then zero-padded at its end to the longest;
//!   a fractional delay is refused;
//! - responses longer than 50 ms are refused, never truncated;
//! - a source position's distance is not used: the sphere is a set of
//!   directions.

use std::collections::HashMap;
use std::path::Path;

use hdf5_pure::{AttrValue, File};
use tutti_buffers::Mono;
use tutti_dsp::{HrirMeasurement, HrirSphere, HrtfBinauralError};
use tutti_units::{Azimuth, Direction, Elevation, SampleRate};

/// Why a SOFA file could not become an HRIR sphere.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SofaError {
    /// The container could not be opened, or a variable not read.
    #[error("reading {what}: {source}")]
    Read {
        /// The file, variable or attribute being read.
        what: String,
        /// The HDF5 reader's error.
        source: hdf5_pure::Error,
    },
    /// The file has no SOFAConventions attribute, a string.
    #[error("not a SOFA file: no `SOFAConventions` attribute")]
    NotSofa,
    /// The convention is not SimpleFreeFieldHRIR.
    #[error("SOFA convention `{found}` is not `SimpleFreeFieldHRIR`")]
    Convention {
        /// The convention the file names.
        found: String,
    },
    /// A variable the convention requires is missing.
    #[error("the file has no `{name}`")]
    Missing {
        /// The variable's name.
        name: &'static str,
    },
    /// A variable has dimensions the convention does not allow.
    #[error("`{name}` has dimensions {found:?}, expected {expected}")]
    Shape {
        /// The variable's name.
        name: &'static str,
        /// The dimensions found.
        found: Vec<u64>,
        /// What the convention requires.
        expected: &'static str,
    },
    /// `Data.IR` does not have two receivers.
    #[error("`Data.IR` has {found} receivers, expected 2 (left, right)")]
    Receivers {
        /// The receiver count found.
        found: u64,
    },
    /// `Data.SamplingRate` is not one integral rate in Hz.
    #[error("`Data.SamplingRate` is not one whole number of Hz: {found:?}")]
    SamplingRate {
        /// The values found.
        found: Vec<f64>,
    },
    /// `Data.SamplingRate`'s Units attribute is not `hertz`.
    #[error("`Data.SamplingRate` has units `{found}`, expected `hertz`")]
    RateUnits {
        /// The units the attribute names.
        found: String,
    },
    /// A `Data.Delay` is not a whole, non-negative number of samples.
    #[error(
        "delay {value} of measurement {measurement}, receiver {receiver}, is not a whole, non-negative number of samples"
    )]
    Delay {
        /// The measurement's index.
        measurement: usize,
        /// The receiver's index.
        receiver: usize,
        /// The delay found, in samples.
        value: f64,
    },
    /// SourcePosition carries no usable `Type` attribute.
    #[error("`SourcePosition` has coordinate type {found:?}, expected `spherical` or `cartesian`")]
    PositionType {
        /// The type the attribute names, if it is present.
        found: Option<String>,
    },
    /// SourcePosition's Units attribute is not the convention's.
    #[error("`SourcePosition` has units `{found}`, expected `{expected}`")]
    PositionUnits {
        /// The units the attribute names.
        found: String,
        /// The units the coordinate type uses.
        expected: &'static str,
    },
    /// There is one source direction, which is no sphere.
    #[error(
        "`SourcePosition` holds one direction; a sphere needs measurements around the listener"
    )]
    SingleDirection,
    /// A source position is not finite, or its elevation is outside -90..=90
    /// degrees, or it is the origin.
    #[error("source position {position:?} of measurement {measurement} is not a direction")]
    Position {
        /// The measurement's index.
        measurement: usize,
        /// The coordinates found.
        position: [f64; 3],
    },
    /// The sphere refused the measurements.
    #[error(transparent)]
    Sphere(#[from] HrtfBinauralError),
}

fn read<T>(what: &str, result: Result<T, hdf5_pure::Error>) -> Result<T, SofaError> {
    result.map_err(|source| SofaError::Read {
        what: what.to_owned(),
        source,
    })
}

/// A string attribute's text, without the padding or whitespace around it.
fn text(attrs: &HashMap<String, AttrValue>, name: &str) -> Option<String> {
    let raw = match attrs.get(name)? {
        AttrValue::String(s) | AttrValue::AsciiString(s) => s.as_str(),
        AttrValue::StringArray(v) | AttrValue::AsciiStringArray(v) if v.len() == 1 => &v[0],
        _ => return None,
    };
    Some(
        raw.trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_owned(),
    )
}

/// The variable `name`: its dimensions and its values.
fn variable(file: &File, name: &'static str) -> Result<(Vec<u64>, Vec<f64>), SofaError> {
    let dataset = file
        .dataset(name)
        .map_err(|_| SofaError::Missing { name })?;
    Ok((
        read(name, dataset.shape())?,
        read(name, dataset.read_f64())?,
    ))
}

/// The sphere of the SOFA file at `path`.
///
/// # Errors
///
/// A [`SofaError`] naming what the file holds that the sphere cannot
/// represent, or the sphere's own refusal of the measurements.
pub fn convert(path: &Path) -> Result<HrirSphere, SofaError> {
    let file = read(&path.display().to_string(), File::open(path))?;
    let root = read("the root attributes", file.root().attrs())?;
    match text(&root, "SOFAConventions") {
        None => return Err(SofaError::NotSofa),
        Some(found) if found != "SimpleFreeFieldHRIR" => {
            return Err(SofaError::Convention { found });
        }
        Some(_) => {}
    }

    let (shape, ir) = variable(&file, "Data.IR")?;
    let &[measurements, receivers, taps] = shape.as_slice() else {
        return Err(SofaError::Shape {
            name: "Data.IR",
            found: shape,
            expected: "measurements x receivers x samples",
        });
    };
    if receivers != 2 {
        return Err(SofaError::Receivers { found: receivers });
    }
    if taps == 0 {
        return Err(SofaError::Shape {
            name: "Data.IR",
            found: shape,
            expected: "at least one sample per response",
        });
    }
    let (measurements, taps) = (measurements as usize, taps as usize);

    if let Ok(dataset) = file.dataset("Data.SamplingRate") {
        let attrs = read("Data.SamplingRate's attributes", dataset.attrs())?;
        if let Some(found) = text(&attrs, "Units").filter(|u| u != "hertz") {
            return Err(SofaError::RateUnits { found });
        }
    }
    let (_, rate) = variable(&file, "Data.SamplingRate")?;
    let rate = match rate[..] {
        [hz] if hz.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&hz) => hz as u32,
        _ => return Err(SofaError::SamplingRate { found: rate }),
    };

    let delays = delays(&file, measurements)?;
    let longest = taps + delays.iter().flatten().copied().max().unwrap_or(0);
    let directions = directions(&file, measurements)?;
    if longest > rate as usize / 20 {
        return Err(HrtfBinauralError::ResponseTooLong {
            len: longest,
            max: rate as usize / 20,
        }
        .into());
    }

    // Per measurement, the left then the right response: the delay as zeros,
    // the taps, then zeros to the longest.
    let mut responses = vec![0.0f32; measurements * 2 * longest];
    for (ear, (out, taps)) in responses
        .chunks_exact_mut(longest)
        .zip(ir.chunks_exact(taps))
        .enumerate()
    {
        let delay = delays[ear / 2][ear % 2];
        for (o, &t) in out[delay..].iter_mut().zip(taps) {
            *o = t as f32;
        }
    }
    let measured: Vec<HrirMeasurement<'_>> = directions
        .into_iter()
        .zip(responses.chunks_exact(2 * longest))
        .map(|(direction, pair)| {
            let (left, right) = pair.split_at(longest);
            HrirMeasurement::new(direction, Mono::new(left), Mono::new(right))
        })
        .collect();
    Ok(HrirSphere::new(SampleRate::of(f64::from(rate)), &measured)?)
}

/// Each measurement's delay per receiver, in whole samples.
fn delays(file: &File, measurements: usize) -> Result<Vec<[usize; 2]>, SofaError> {
    let (shape, values) = variable(file, "Data.Delay")?;
    // One delay per receiver for every measurement, or one per measurement.
    let rows = match shape[..] {
        [1, 2] => 1,
        [m, 2] if m as usize == measurements => measurements,
        _ => {
            return Err(SofaError::Shape {
                name: "Data.Delay",
                found: shape,
                expected: "1 x 2 or measurements x 2",
            });
        }
    };
    let mut table = Vec::with_capacity(measurements);
    for measurement in 0..measurements {
        let row = measurement % rows;
        let mut pair = [0; 2];
        for (receiver, whole) in pair.iter_mut().enumerate() {
            let value = values[row * 2 + receiver];
            // A delay past the largest rate's 50 ms is past any response the
            // sphere accepts: refused by the length check, not allocated.
            let valid = value.fract() == 0.0 && (0.0..=1e9).contains(&value);
            if !valid {
                return Err(SofaError::Delay {
                    measurement,
                    receiver,
                    value,
                });
            }
            *whole = value as usize;
        }
        table.push(pair);
    }
    Ok(table)
}

/// Each measurement's source direction, in tutti's frame.
fn directions(file: &File, measurements: usize) -> Result<Vec<Direction>, SofaError> {
    let dataset = file
        .dataset("SourcePosition")
        .map_err(|_| SofaError::Missing {
            name: "SourcePosition",
        })?;
    let attrs = read("SourcePosition's attributes", dataset.attrs())?;
    let kind = text(&attrs, "Type");
    let (spherical, expected) = match kind.as_deref() {
        Some("spherical") => (true, "degree, degree, metre"),
        Some("cartesian") => (false, "metre, metre, metre"),
        _ => return Err(SofaError::PositionType { found: kind }),
    };
    if let Some(found) = text(&attrs, "Units") {
        let parts: Vec<&str> = found.split(',').map(str::trim).collect();
        let unit = if spherical { "degree" } else { "metre" };
        let metre = |s: &&str| matches!(*s, "metre" | "meter");
        let fits = parts.len() == 3
            && if spherical {
                parts[..2].iter().all(|&p| p == unit) && metre(&parts[2])
            } else {
                parts.iter().all(metre)
            };
        if !fits {
            return Err(SofaError::PositionUnits { found, expected });
        }
    }
    let (shape, values) = variable(file, "SourcePosition")?;
    let rows = match shape[..] {
        [1, 3] => return Err(SofaError::SingleDirection),
        [m, 3] if m as usize == measurements => measurements,
        _ => {
            return Err(SofaError::Shape {
                name: "SourcePosition",
                found: shape,
                expected: "measurements x 3",
            });
        }
    };
    values
        .as_chunks::<3>()
        .0
        .iter()
        .take(rows)
        .enumerate()
        .map(|(measurement, p)| {
            let position = [p[0], p[1], p[2]];
            let (azimuth, elevation) = if spherical {
                (position[0], position[1])
            } else {
                let level = position[0].hypot(position[1]);
                (
                    position[1].atan2(position[0]).to_degrees(),
                    position[2].atan2(level).to_degrees(),
                )
            };
            let origin = !spherical && position == [0.0; 3];
            let valid = azimuth.is_finite() && (-90.0..=90.0).contains(&elevation) && !origin;
            if !valid {
                return Err(SofaError::Position {
                    measurement,
                    position,
                });
            }
            let azimuth = Azimuth(azimuth as f32).wrap();
            Ok(Direction::new(azimuth, Elevation(elevation as f32)))
        })
        .collect()
}
