# tutti-units

Numbers with units for audio code: hertz, decibels, seconds, beats, tempo and sample counts, each its own type, so a frequency cannot be passed where a duration is meant.

Each type converts to the others by a named method, and offers only the arithmetic that means something for it. It has no dependency on the rest of tutti and does not allocate, so you can use it on its own, for example in a plain DSP library. Most other tutti crates are built on it.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
