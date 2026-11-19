use super::*;
use std::io::Read;

#[test]
fn two_opens_of_one_bytes_asset_are_independent_readers() {
    let asset = Asset::from(vec![10u8, 11, 12, 13, 14, 15]);
    let mut first = asset.open().unwrap();
    let mut second = asset.open().unwrap();
    let mut head = [0u8; 3];
    first.read_exact(&mut head).unwrap();
    assert_eq!(head, [10, 11, 12]);
    let mut again = [0u8; 2];
    second.read_exact(&mut again).unwrap();
    assert_eq!(again, [10, 11], "the second reader starts at 0");
    let mut rest = Vec::new();
    first.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, [13, 14, 15]);
}

#[test]
fn equal_bytes_are_two_identities_and_a_clone_keeps_its_own() {
    let a = Asset::from(vec![1u8, 2, 3]);
    let b = Asset::from(vec![1u8, 2, 3]);
    assert_ne!(a.identity(), b.identity());
    assert!(a.identity().is_some());
    assert_eq!(a.clone().identity(), a.identity());
}

#[test]
fn a_hint_overrides_the_source_and_a_path_hints_its_extension() {
    let bytes = Asset::from(vec![0u8]);
    assert_eq!(bytes.extension(), None);
    assert_eq!(bytes.hint("flac").extension(), Some("flac"));
    assert_eq!(Asset::from("dir/kick.wav").extension(), Some("wav"));
}

#[test]
fn a_path_identity_changes_when_the_file_length_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clip.bin");
    std::fs::write(&path, [0u8; 4]).unwrap();
    let asset = Asset::from(path.as_path());
    let before = asset.identity();
    assert!(before.is_some());
    assert_eq!(before, Asset::from(path.as_path()).identity());
    std::fs::write(&path, [0u8; 9]).unwrap();
    // The same asset: its key is taken at each call, from the file as it is now.
    assert_ne!(before, asset.identity());
    let mut read = Vec::new();
    asset.open().unwrap().read_to_end(&mut read).unwrap();
    assert_eq!(read.len(), 9);
}

#[test]
fn a_missing_path_has_no_identity_and_fails_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let asset = Asset::from(dir.path().join("absent.wav"));
    assert_eq!(asset.identity(), None);
    assert!(asset.open().is_err());
    assert!(asset.name().ends_with("absent.wav"));
}

#[test]
fn two_assets_of_one_path_are_equal_and_of_two_paths_are_not() {
    assert_eq!(
        Asset::from("a/kick.wav"),
        Asset::from(PathBuf::from("a/kick.wav"))
    );
    assert_ne!(Asset::from("a/kick.wav"), Asset::from("a/snare.wav"));
}

#[test]
fn equal_bytes_in_two_vecs_are_two_assets_and_a_clone_is_its_original() {
    let a = Asset::from(vec![1u8, 2, 3]);
    assert_ne!(a, Asset::from(vec![1u8, 2, 3]));
    assert_eq!(a, a.clone());
    assert_eq!(a, a.clone().hint("wav"), "the hint is not the source");
}

#[test]
fn an_arc_of_an_open_is_equal_to_itself_and_not_to_another_arc() {
    struct Silent;
    impl Open for Silent {
        fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
            Ok(Box::new(Cursor::new(Vec::new())))
        }
        fn name(&self) -> String {
            "silent".into()
        }
    }
    let first: Arc<dyn Open> = Arc::new(Silent);
    let second: Arc<dyn Open> = Arc::new(Silent);
    assert_eq!(Asset::from(Arc::clone(&first)), Asset::from(first));
    assert_ne!(
        Asset::from(Arc::clone(&second)),
        Asset::from(Arc::new(Silent) as Arc<dyn Open>)
    );
}
