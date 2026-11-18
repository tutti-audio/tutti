# tutti-types

Shared building blocks for audio code: channel layouts and the speakers they feed, ids, and the lock-free cell that carries a parameter to the audio thread, over the unit types of `tutti-units`.

It depends on no other part of tutti but `tutti-units`, so you can use it on its own, for example to describe the channels of an audio device. Most other tutti crates are built on it.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
