use tutti_winmm_ffi::{
    input_caps, input_count, input_interface, output_caps, output_count, output_interface,
};

#[test]
fn every_listed_port_describes_itself_and_no_input_is_a_clean_empty_list() {
    for index in 0..input_count() {
        let caps = input_caps(index).expect("a listed input has caps");
        assert!(!caps.name.is_empty());
    }
    for index in 0..output_count() {
        let caps = output_caps(index).expect("a listed output has caps");
        assert!(!caps.name.is_empty());
    }
    let past = input_count().max(output_count());
    assert!(
        input_caps(past).is_err(),
        "a position past the list is refused"
    );
}

#[test]
fn an_interface_path_is_a_device_path_or_absent() {
    for index in 0..output_count() {
        if let Some(path) = output_interface(index) {
            assert!(path.starts_with(r"\\?\"), "{path}");
        }
    }
    for index in 0..input_count() {
        if let Some(path) = input_interface(index) {
            assert!(path.starts_with(r"\\?\"), "{path}");
        }
    }
}
