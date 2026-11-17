use tutti_winmm_ffi::{input_ports, output_ports};

#[test]
fn every_listed_port_describes_itself_and_names_itself() {
    for port in input_ports() {
        assert!(!port.caps().name.is_empty());
        assert!(!port.name().is_empty());
    }
    for port in output_ports() {
        assert!(!port.caps().name.is_empty());
        assert!(!port.name().is_empty());
    }
}

#[test]
fn an_interface_path_is_a_device_path_or_absent() {
    for path in output_ports()
        .iter()
        .map(|p| p.interface())
        .chain(input_ports().iter().map(|p| p.interface()))
        .flatten()
    {
        assert!(path.starts_with(r"\\?\"), "{path}");
    }
}
