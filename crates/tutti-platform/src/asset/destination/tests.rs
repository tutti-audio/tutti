use std::io::Write;
use std::path::Path;

use super::Destination;

#[test]
fn a_str_is_a_path_as_it_is_for_an_asset() {
    let Destination::Path(staged) = Destination::from("out.wav") else {
        panic!("a &str is a path");
    };
    assert_eq!(staged.target(), Path::new("out.wav"));
    assert_eq!(
        staged.temp().parent(),
        Some(Path::new("")),
        "staged beside the target"
    );
}

#[test]
fn create_new_refuses_an_existing_file_and_leaves_a_writer_alone() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.bin");
    std::fs::write(&target, b"old").unwrap();
    for (destination, refused) in [
        (Destination::from(&target), false),
        (Destination::from(&target).create_new(), true),
    ] {
        let Destination::Path(staged) = destination else {
            panic!("a path is a path");
        };
        let mut file = staged.open().unwrap();
        file.write_all(b"new").unwrap();
        let committed = file.commit();
        assert_eq!(committed.is_err(), refused);
        if let Err(error) = committed {
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(&target).unwrap(), b"old");
            std::fs::remove_file(error.kept()).unwrap();
            std::fs::write(&target, b"old").unwrap();
        } else {
            assert_eq!(std::fs::read(&target).unwrap(), b"new");
            std::fs::write(&target, b"old").unwrap();
        }
    }
    let mut bytes = Vec::new();
    assert!(matches!(
        Destination::from(&mut bytes).create_new(),
        Destination::Writer(_)
    ));
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
