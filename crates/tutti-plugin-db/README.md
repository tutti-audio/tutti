# tutti-plugin-db

Finds the audio plugins installed on a machine and remembers what it found.

It walks plugin folders, records each plugin file's details, and notes files that crashed while being scanned so they are skipped next time. It depends only on tutti's plugin type definitions, so it can be used on its own to build a plugin list. With the `serde` feature the list can be saved: you pick the file format and where it lives.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
