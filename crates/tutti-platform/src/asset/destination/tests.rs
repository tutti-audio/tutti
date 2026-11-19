use std::io::Write;
use std::path::Path;

use super::Destination;

#[test]
fn a_str_is_a_path_as_it_is_for_an_asset() {
    let Destination::Path(path) = Destination::from("out.wav") else {
        panic!("a &str is a path");
    };
    assert_eq!(path.as_ref(), Path::new("out.wav"));
}

#[test]
fn a_dyn_writer_and_a_sized_one_are_both_writers_that_receive_the_bytes() {
    let mut sized = Vec::new();
    let mut unsized_backing = Vec::new();
    let dyn_writer: &mut dyn Write = &mut unsized_backing;
    for (destination, bytes) in [
        (Destination::from(&mut sized), b"ab".as_slice()),
        (Destination::from(dyn_writer), b"cd".as_slice()),
    ] {
        let Destination::Writer(writer) = destination else {
            panic!("a writer is a writer");
        };
        writer.write_all(bytes).unwrap();
    }
    assert_eq!(sized, b"ab");
    assert_eq!(unsized_backing, b"cd");
}
