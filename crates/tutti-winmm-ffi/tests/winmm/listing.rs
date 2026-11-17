use tutti_winmm_ffi::{Input, Output, ports};

#[test]
fn every_listed_port_describes_itself_and_names_itself() {
    for port in ports::<Input>() {
        assert!(!port.caps().name.is_empty());
        assert!(!port.name().is_empty());
    }
    for port in ports::<Output>() {
        assert!(!port.caps().name.is_empty());
        assert!(!port.name().is_empty());
    }
}

#[test]
fn an_interface_path_is_a_device_path_or_absent() {
    let inputs = ports::<Input>();
    let outputs = ports::<Output>();
    for path in outputs
        .iter()
        .map(|p| p.interface())
        .chain(inputs.iter().map(|p| p.interface()))
        .flatten()
    {
        assert!(path.starts_with(r"\\?\"), "{path}");
    }
}
