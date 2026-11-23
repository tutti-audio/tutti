//! `check-building`'s tests.

use super::super::fake::{Mem, run_gate};
use super::allow::{CTOR, FFI, FIELDS, WIDTH, ty};
use super::*;

const F: &str = "crates/a/src/lib.rs";

fn rules(src: &str) -> Vec<(String, &'static str)> {
    scan(F, src)
        .into_iter()
        .map(|s| (s.key.rsplit("::").next().unwrap().to_owned(), s.rule.name()))
        .collect()
}

fn one(name: &str, rule: &'static str) -> Vec<(String, &'static str)> {
    vec![(name.to_owned(), rule)]
}

#[test]
fn a_chained_option_named_with_is_a_violation_and_a_bare_name_is_not() {
    let bad = "pub struct T; impl T { pub fn with_gain(mut self, g: f32) -> Self { self } }";
    assert_eq!(rules(bad), one("with_gain", "with-prefix"));
    let ok = "pub struct T; impl T { pub fn gain(mut self, g: f32) -> Self { self }\n\
              pub fn with_view(&self) -> u8 { 0 } }";
    assert!(rules(ok).is_empty());
}

#[test]
fn a_chained_option_returning_result_self_is_a_violation_and_a_terminal_build_is_not() {
    let bad = "pub struct T; impl T { pub fn depth(self, d: u8) -> Result<Self, E> { Ok(self) } }";
    assert_eq!(rules(bad), one("depth", "chain-result"));
    let bare = "pub struct T; impl T { pub fn depth(self) -> Result<T> { Ok(self) } }";
    assert_eq!(rules(bare), one("depth", "chain-result"));
    let ok = "pub struct B; impl B { pub fn build(self) -> Result<Product, E> { todo!() }\n\
              pub fn depth(self, d: u8) -> Self { self } }";
    assert!(rules(ok).is_empty());
}

#[test]
fn a_chained_option_taking_an_option_is_a_violation_and_other_shapes_are_not() {
    for bad in [
        "impl T { pub fn pool(mut self, p: Option<Arc<P>>) -> Self { self } }",
        "impl T { pub fn pool(self, p: std::option::Option<u8>) -> T { self } }",
        "impl T { pub fn pool(self, p: core::option::Option<u8>) -> Self { self } }",
        "impl T { pub fn pool(self, p: impl Into<Option<u8>>) -> Self { self } }",
        "impl T { pub fn pool(self, p: ::core::option::Option<u8>) -> Self { self } }",
        "impl T { pub fn pool(self, mut p: Option<u8>) -> Self { self } }",
        "impl T { pub fn pool<O: Into<Option<u8>>>(self, p: O) -> Self { self } }",
        "impl T { pub fn pool<O>(self, p: O) -> Self where O: Into<Option<u8>> { self } }",
    ] {
        assert_eq!(rules(bad), one("pool", "option-arg"), "{bad}");
    }
    for ok in [
        "impl T { pub fn pool(&self, p: Option<u8>) -> Self { todo!() } }",
        "impl T { pub fn pool(self, p: Option<u8>) -> Option<Self> { Some(self) } }",
        "impl T { pub fn build(self, p: Option<u8>) -> Product { todo!() } }",
        "pub fn pool(p: Option<u8>) -> Self { todo!() }",
        "impl T { pub(crate) fn pool(self, p: Option<u8>) -> Self { self } }",
        "impl T { pub fn pool(self, p: Vec<Option<u8>>) -> Self { self } }",
        "impl T { pub fn pool(self, p: u8) -> Self { self } }",
        "impl T { pub fn pool(self, p: &Option<u8>) -> Self { self } }",
        "impl T { pub fn pool<O: Into<u8>>(self, p: O) -> Self { self } }",
    ] {
        assert_eq!(rules(ok), vec![], "{ok}");
    }
}

const HOSTING: &str = "#[derive(Debug, Default)] pub enum Hosting { #[default] Auto, InProcess }\n";

fn tree_rules(files: &[(&str, &str)]) -> Vec<String> {
    let mut all = vec![(LIST, "")];
    all.extend_from_slice(files);
    let r = check(&Mem::with(&all), &[], 0);
    r.findings
        .into_iter()
        .filter(|l| l.contains("default-enum-arg"))
        .collect()
}

#[test]
fn a_derived_default_enum_taken_by_a_ref_method_is_a_violation() {
    let src = format!("{HOSTING}impl S {{ pub fn load(&self, id: u8, h: Hosting) -> u8 {{ 0 }} }}");
    assert_eq!(rules(&src), one("load", "default-enum-arg"));
    for peeled in [
        "h: &Hosting",
        "h: &mut Hosting",
        "h: impl Into<Hosting>",
        "h: crate::Hosting",
    ] {
        let src = format!("{HOSTING}impl S {{ pub fn load(&self, id: u8, {peeled}) {{}} }}");
        assert_eq!(rules(&src), one("load", "default-enum-arg"), "{peeled}");
    }
    let src = format!("{HOSTING}impl S {{ pub fn new(id: u8, h: Hosting) -> Self {{ todo!() }} }}");
    assert_eq!(rules(&src), one("new", "default-enum-arg"));
    let src = format!(
        "{HOSTING}impl S {{ pub fn load<H: Into<Hosting>>(&self, id: u8, h: H) {{}}\n\
         pub fn open<H>(&self, id: u8, h: H) where H: Into<Hosting> {{}} }}"
    );
    assert_eq!(
        rules(&src),
        vec![
            ("load".to_owned(), "default-enum-arg"),
            ("open".to_owned(), "default-enum-arg")
        ]
    );
}

#[test]
fn a_hand_default_enum_is_known_in_its_own_crate_only() {
    let en =
        "pub enum Mode { A, B }\nimpl<T> Default for Mode { fn default() -> Self { Mode::A } }\n";
    let user = "impl S { pub fn go(&self, id: u8, m: Mode) {} }";
    let same = tree_rules(&[("crates/a/src/lib.rs", user), ("crates/a/src/mode.rs", en)]);
    assert_eq!(same.len(), 1, "{same:?}");
    let other = tree_rules(&[("crates/a/src/lib.rs", user), ("crates/b/src/mode.rs", en)]);
    assert!(other.is_empty(), "{other:?}");
    let prefixed = "pub enum Mode { A }\nimpl core::default::Default for Mode { fn default() -> Self { Mode::A } }\n";
    let same = tree_rules(&[
        ("crates/a/src/lib.rs", user),
        ("crates/a/src/mode.rs", prefixed),
    ]);
    assert_eq!(same.len(), 1, "{same:?}");
}

#[test]
fn a_chained_option_taking_a_default_enum_and_the_enums_own_methods_are_not_violations() {
    for ok in [
        "impl C { pub fn hosting(self, id: u8, h: Hosting) -> Self { self } }",
        "impl C { pub fn hosting(mut self, id: u8, h: Hosting) -> C { self } }",
        "impl Hosting { pub fn same(&self, id: u8, h: Hosting) -> bool { true } }",
        "impl C { pub fn hosting(&self, n: u8) {} }",
    ] {
        assert_eq!(rules(&format!("{HOSTING}{ok}")), vec![], "{ok}");
    }
    // A by-value receiver that does not build its owner takes the option positionally.
    let src = format!(
        "{HOSTING}impl C {{ pub fn spawn(self, id: u8, h: Hosting) -> Product {{ todo!() }} }}"
    );
    assert_eq!(rules(&src), one("spawn", "default-enum-arg"));
}

#[test]
fn a_setter_taking_only_the_default_enum_passes_and_with_a_second_parameter_fails() {
    let only = format!(
        "{HOSTING}impl S {{ pub fn set_mode(&mut self, h: Hosting) {{}}\n\
                        pub fn pick(h: Hosting) -> u8 {{ 0 }} }}"
    );
    assert_eq!(rules(&only), one("set_mode", "setter"));
    let two = format!("{HOSTING}impl S {{ pub fn load(&mut self, id: u8, h: Hosting) {{}} }}");
    assert_eq!(rules(&two), one("load", "default-enum-arg"));
}

#[test]
fn a_keyed_setter_sets_its_last_parameter_and_an_enum_before_it_is_an_option() {
    let keyed =
        format!("{HOSTING}impl S {{ pub fn set_mode(&mut self, id: u8, h: Hosting) {{}} }}");
    assert_eq!(rules(&keyed), one("set_mode", "setter"));
    let first =
        format!("{HOSTING}impl S {{ pub fn set_mode(&mut self, h: Hosting, id: u8) {{}} }}");
    let found = rules(&first);
    assert!(
        found.iter().any(|(_, r)| *r == "default-enum-arg"),
        "an enum before the set value is an option: {found:?}"
    );
}

#[test]
fn a_name_resolves_to_the_crate_it_is_imported_from() {
    let en = "#[derive(Default)] pub enum Direction { #[default] Up, Down }";
    let st = "pub struct Direction(f64);";
    let user =
        |imp: &str, ty: &str| format!("{imp}\nimpl S {{ pub fn go(&self, id: u8, d: {ty}) {{}} }}");
    let run = |imp: &str, ty: &str| {
        tree_rules(&[
            ("crates/tutti-a/src/lib.rs", &user(imp, ty)),
            ("crates/tutti-e/src/lib.rs", en),
            ("crates/tutti-s/src/lib.rs", st),
        ])
        .len()
    };
    assert_eq!(run("use tutti_s::Direction;", "Direction"), 0);
    assert_eq!(run("use tutti_e::Direction;", "Direction"), 1);
    assert_eq!(run("use tutti_e::{Other, Direction};", "Direction"), 1);
    assert_eq!(run("use tutti_s::{a::{Other}, Direction};", "Direction"), 0);
    assert_eq!(run("use tutti_e::module::Direction;", "Direction"), 1);
    assert_eq!(run("use tutti_e;", "tutti_e::Direction"), 1);
    assert_eq!(run("", "tutti_s::Direction"), 0);
    assert_eq!(run("", "tutti_e::Direction"), 1);
    // No import and no path: the file's own crate, which has no `Direction`.
    assert_eq!(run("", "Direction"), 0);
    assert_eq!(run("use std::fmt::Direction;", "Direction"), 0);
}

#[test]
fn an_as_rename_resolves_by_its_local_name() {
    let en = "#[derive(Default)] pub enum Direction { #[default] Up, Down }";
    let st = "pub struct Direction(f64);";
    let run = |imp: &str, ty: &str| {
        let user = format!("{imp}\nimpl S {{ pub fn go(&self, id: u8, d: {ty}) {{}} }}");
        tree_rules(&[
            ("crates/tutti-a/src/lib.rs", &user),
            ("crates/tutti-e/src/lib.rs", en),
            ("crates/tutti-s/src/lib.rs", st),
        ])
        .len()
    };
    assert_eq!(run("use tutti_e::Direction as Heading;", "Heading"), 1);
    assert_eq!(run("use tutti_s::Direction as Heading;", "Heading"), 0);
    // The original name is no longer in scope: it is the file's own crate's.
    assert_eq!(run("use tutti_e::Direction as Heading;", "Direction"), 0);
}

#[test]
fn an_enum_without_a_default_is_not_a_usual_value() {
    let src = "pub enum Kind { A, B }\nimpl S { pub fn go(&self, k: Kind) {} }";
    assert_eq!(rules(src), vec![]);
    let src = "#[derive(Debug)] pub enum Kind { A, B }\nimpl S { pub fn go(&self, k: Kind) {} }";
    assert_eq!(rules(src), vec![]);
}

#[test]
fn an_option_arg_row_with_no_violation_is_stale_and_fails() {
    let list = "a  option-arg  crates/a/src/lib.rs::T::pool\n";
    let r = run_gate(
        |t| check(t, &[], 0),
        &[
            (LIST, list),
            (F, "impl T { pub fn pool(self, p: u8) -> Self { self } }"),
        ],
    );
    assert!(!r.passed());
}

#[test]
fn a_setter_on_a_building_owner_is_a_violation_and_a_by_value_or_private_one_is_not() {
    for owner in [
        "GainNode",
        "FileSource",
        "RenderSpec",
        "MixConfig",
        "LoadOptions",
    ] {
        let src = format!(
            "impl {owner} {{ pub fn set_gain(&self, g: f32) {{}}\n\
                           pub fn set_mode(&mut self, m: u8) {{}} }}"
        );
        assert_eq!(
            rules(&src),
            [
                ("set_gain".to_owned(), "setter"),
                ("set_mode".to_owned(), "setter")
            ],
            "{owner}"
        );
    }
    let ok = "impl GainNode { pub fn set_gain(self, g: f32) -> Self { self } }\n\
              impl GainNode { pub fn gain(&self) -> f32 { 0.0 } }\n\
              impl GainNode { pub(crate) fn set_gain(&self, g: f32) {} }";
    assert!(rules(ok).is_empty());
}

#[test]
fn a_setter_on_an_owner_that_is_not_a_node_source_or_spec_is_a_violation() {
    for owner in ["GainHandle", "Transport", "Meter", "Node"] {
        let src = format!("impl {owner} {{ pub fn set_gain(&self, g: f32) {{}} }}");
        assert_eq!(rules(&src), one("set_gain", "setter"), "{owner}");
    }
}

#[test]
fn a_public_constructor_on_a_node_is_a_violation_and_one_on_its_config_is_not() {
    let bad = "impl GainNode { pub fn new(c: GainConfig) -> Self { todo!() }\n\
               pub fn open(p: &str) -> Result<Self, E> { todo!() }\n\
               pub fn maybe() -> Option<GainNode> { None } }";
    let got = rules(bad);
    assert!(got.contains(&("new".to_owned(), "node-ctor")));
    assert!(got.contains(&("open".to_owned(), "node-ctor")));
    assert!(got.contains(&("maybe".to_owned(), "node-ctor")));
    let ok = "impl GainNode { pub fn gain(&self) -> Param<Db> { todo!() }\n\
              pub(crate) fn new() -> Self { todo!() } }\n\
              impl Other { pub fn new() -> Self { todo!() } }";
    assert!(rules(ok).is_empty());
}

#[test]
fn a_config_constructor_must_take_a_width_first() {
    let ok = "impl AConfig { pub fn new(l: ChannelLayout, r: u8) -> Self { todo!() } }\n\
              impl BConfig { pub fn new(t: tutti_buffers::BusTopology) -> Self { todo!() } }\n\
              impl CConfig { pub fn new(l: impl Into<ChannelLayout>) -> Self { todo!() } }\n\
              impl DConfig { pub fn new<L: Into<BusTopology>>(l: L) -> Self { todo!() } }\n\
              impl EConfig { pub fn new<L>(l: L) -> Self where L: Into<ChannelLayout> { todo!() } }\n\
              impl FConfig { pub fn gain(&self) -> Self { todo!() } }\n\
              impl From<AConfig> for ANode { fn from(c: AConfig) -> Self { todo!() } }\n\
              impl From<BConfig> for BNode { fn from(c: BConfig) -> Self { todo!() } }\n\
              impl From<CConfig> for CNode { fn from(c: CConfig) -> Self { todo!() } }\n\
              impl From<DConfig> for DNode { fn from(c: DConfig) -> Self { todo!() } }\n\
              impl From<EConfig> for ENode { fn from(c: EConfig) -> Self { todo!() } }\n\
              impl From<FConfig> for FNode { fn from(c: FConfig) -> Self { todo!() } }";
    assert!(rules(ok).is_empty());
    for bad in [
        "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }",
        "impl XConfig { pub fn new() -> Self { todo!() } }",
        "impl XConfig { pub fn new(r: u32, l: ChannelLayout) -> Self { todo!() } }",
        "impl XConfig { pub fn new<L: Into<u8>>(l: L) -> Self { todo!() } }",
    ] {
        let node = format!(
            "{bad}\nimpl From<XConfig> for XNode {{ fn from(c: XConfig) -> Self {{ todo!() }} }}"
        );
        assert_eq!(rules(&node), one("new", "config-width"), "{bad}");
    }
}

#[test]
fn a_config_that_no_node_is_converted_from_is_not_read_for_a_width() {
    let bare = "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }";
    assert!(rules(bare).is_empty());
    let other = "impl From<XConfig> for XSettings { fn from(c: XConfig) -> Self { todo!() } }\n\
                 impl From<YConfig> for XNode { fn from(c: YConfig) -> Self { todo!() } }";
    assert!(rules(&format!("{bare}\n{other}")).is_empty());
    let node = "impl From<XConfig> for XNode { fn from(c: XConfig) -> Self { todo!() } }";
    assert_eq!(
        rules(&format!("{bare}\n{node}")),
        one("new", "config-width")
    );
}

#[test]
fn a_node_config_is_a_from_or_try_from_into_a_node_or_an_alias_of_one() {
    let bare = "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }\n";
    for conv in [
        "impl<F: Float> From<XConfig> for a::XNode<F> { fn from(c: XConfig) -> Self { todo!() } }",
        "impl<F: Float> TryFrom<b::XConfig<F>> for XNode<F> { type Error = E; fn try_from(c: XConfig) -> Result<Self, E> { todo!() } }",
        "impl From<XConfig> for XNode { fn from(c: XConfig) -> Self { todo!() } }",
        "pub type XNode<F> = KernelNode<XKernel<F>>;\n\
         impl<F> TryFrom<XConfig> for XNode<F> { type Error = E; fn try_from(c: XConfig) -> Result<Self, E> { todo!() } }",
    ] {
        assert_eq!(
            rules(&format!("{bare}{conv}")),
            one("new", "config-width"),
            "{conv}"
        );
    }
    let across = Mem::with(&[
        (LIST, ""),
        (F, bare),
        (
            "crates/a/src/node.rs",
            "pub type XNode<F> = KernelNode<XKernel<F>>;\n\
             impl<F> TryFrom<XConfig> for XNode<F> { type Error = E; fn try_from(c: XConfig) -> Result<Self, E> { todo!() } }",
        ),
    ]);
    let r = check(&across, &[], 0);
    assert_eq!(
        r.findings[1],
        "  a  config-width  crates/a/src/lib.rs::XConfig::new"
    );
}

#[test]
fn a_node_constructor_taking_a_config_makes_it_a_node_config() {
    let bare = "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }\n";
    for ctor in [
        "impl XNode { pub fn new(config: XConfig) -> Self { todo!() } }",
        "impl<K> XNode<K> { pub fn new(config: &x::XConfig) -> Self { todo!() } }",
        "impl XNode { pub fn from_config(rate: u32, config: impl Into<XConfig>) -> Self { todo!() } }",
    ] {
        assert_eq!(rules(&format!("{bare}{ctor}")).len(), 2, "{ctor}");
        assert!(rules(&format!("{bare}{ctor}")).contains(&("new".to_owned(), "config-width")));
    }
    let not = "impl XNode { pub fn new(config: YConfig) -> Self { todo!() } }\n\
               impl XNode { pub fn tuned(&self, config: XConfig) -> Self { todo!() } }\n\
               impl XOther { pub fn new(config: XConfig) -> Self { todo!() } }";
    assert!(!rules(&format!("{bare}{not}")).contains(&("new".to_owned(), "config-width")));
}

#[test]
fn a_result_constructor_on_a_building_owner_is_a_violation_and_a_plain_one_is_not() {
    let bad = "impl PhaserConfig { pub fn new(r: f32) -> Result<Self, E> { todo!() } }";
    assert!(rules(bad).contains(&("new".to_owned(), "ctor-result")));
    let ok = "impl XSource { pub fn probe(p: &str) -> Result<Report, E> { todo!() } }\n\
              impl Wave { pub fn plain(p: &str) -> Self { todo!() } }\n\
              impl Wave { pub fn load(&self) -> Result<Self, E> { todo!() } }";
    assert!(rules(ok).is_empty());
}

#[test]
fn a_result_constructor_on_an_owner_that_is_not_a_building_one_is_a_violation() {
    for owner in ["Wave", "Session", "Fold"] {
        let src =
            format!("impl {owner} {{ pub fn open(p: &str) -> Result<{owner}, E> {{ todo!() }} }}");
        assert_eq!(rules(&src), one("open", "ctor-result"), "{owner}");
    }
    let src = "impl Wave { pub fn read(p: &str) -> Result<Self> { todo!() } }";
    assert_eq!(rules(src), one("read", "ctor-result"));
}

#[test]
fn a_macro_writing_a_range_type_is_a_violation_outside_its_homes_only() {
    let src = "macro_rules! ranged { ($n:ident) => { impl $n { pub fn new(raw: u8) -> Option<Self> { todo!() } } }; }\n\
               macro_rules! plain { ($n:ident) => { impl $n { pub fn new(a: u8, b: u8) -> Option<Self> { todo!() } } }; }";
    assert_eq!(rules(src), one("ranged", "range-macro"));
    assert!(scan("crates/tutti-units/src/lib.rs", src).is_empty());
    assert!(scan("crates/tutti-rt/src/id.rs", src).is_empty());
}

#[test]
fn a_public_field_config_is_a_violation_and_other_shapes_are_not() {
    for bad in [
        "pub struct SynthConfig { pub gain: f32, depth: u8 }",
        "pub struct VoiceSettings<F> { #[doc = \"x\"] pub gain: F }",
        "pub struct RunOptions(pub u8);",
        "pub struct RunOptions(u8, pub u8);",
        "pub struct Config { pub gain: f32 }",
    ] {
        let name = bad
            .split_whitespace()
            .nth(2)
            .unwrap()
            .split(['<', '('])
            .next()
            .unwrap();
        assert_eq!(rules(bad), one(name, "pub-field-config"), "{bad}");
    }
    for ok in [
        "pub struct SynthConfig { pub(crate) gain: f32, pub(super) d: u8 }",
        "pub struct SynthConfig(pub(crate) u8);",
        "pub struct SynthConfig { gain: f32 } impl SynthConfig { pub fn new() -> Self { todo!() } pub fn gain(self, g: f32) -> Self { self } }",
        "pub struct Synth { pub gain: f32 }",
        "pub struct SynthConfigs { pub gain: f32 }",
        "struct SynthConfig { pub gain: f32 }",
        "pub(crate) struct SynthConfig { pub gain: f32 }",
        "pub struct SynthConfig;",
    ] {
        assert_eq!(rules(ok), vec![], "{ok}");
    }
}

#[test]
fn a_public_field_config_in_a_test_or_a_macro_body_is_not_read() {
    let src = "#[cfg(test)] mod m { pub struct SynthConfig { pub gain: f32 } }\n\
               macro_rules! m { () => { pub struct SynthConfig { pub gain: f32 } }; }\n\
               // pub struct XConfig { pub a: u8 }\n";
    assert!(rules(src).is_empty());
    let r = check(
        &Mem::with(&[
            (LIST, ""),
            ("crates/a/tests/t.rs", "pub struct XConfig { pub a: u8 }"),
            ("crates/a/src/tests.rs", "pub struct XConfig { pub a: u8 }"),
            ("testing/b/src/lib.rs", "pub struct XConfig { pub a: u8 }"),
            ("xtask/src/lib.rs", "pub struct XConfig { pub a: u8 }"),
            ("vendor/c/src/lib.rs", "pub struct XConfig { pub a: u8 }"),
        ]),
        &[],
        0,
    );
    assert!(r.passed());
}

#[test]
fn a_public_field_config_needs_a_row_and_an_allow_row_or_a_baseline_row_covers_it() {
    let src = "pub struct SynthConfig { pub gain: f32 }";
    let r = run_gate(|t| check(t, &[], 0), &[(LIST, ""), (F, src)]);
    assert!(!r.passed());
    assert_eq!(
        r.findings[1],
        "  a  pub-field-config  crates/a/src/lib.rs::SynthConfig"
    );
    let row = "a  pub-field-config  crates/a/src/lib.rs::SynthConfig\n";
    assert!(run_gate(|t| check(t, &[], 0), &[(LIST, row), (F, src)]).passed());
    let allow = [ty(F, "SynthConfig", FIELDS, "the protocol's own fields")];
    assert!(run_gate(|t| check(t, &allow, 0), &[(LIST, ""), (F, src)]).passed());
    assert!(!run_gate(|t| check(t, &allow, 0), &[(LIST, row), (F, src)]).passed());
    let other = [ty(F, "OtherConfig", FIELDS, "x")];
    assert!(!run_gate(|t| check(t, &other, 0), &[(LIST, ""), (F, src)]).passed());
}

#[test]
fn a_public_field_config_row_with_no_violation_is_stale_and_fails() {
    let r = run_gate(
        |t| check(t, &[], 0),
        &[
            (
                LIST,
                "a  pub-field-config  crates/a/src/lib.rs::SynthConfig\n",
            ),
            (F, "pub struct SynthConfig { gain: f32 }"),
        ],
    );
    assert!(!r.passed());
    assert!(r.findings[0].contains("names no violation"));
}

#[test]
fn items_in_tests_comments_strings_and_non_public_fns_are_not_read() {
    let src = "// pub fn with_a(self) -> Self\nconst S: &str = \"impl XNode { pub fn new() -> Self }\";\n\
               impl T { pub(crate) fn with_b(self) -> Self { self } }\n\
               #[cfg(test)] mod m { impl T { pub fn with_c(self) -> Self { self } } }\n\
               trait Q { fn with_d(self) -> Self; }\n";
    assert!(rules(src).is_empty());
}

#[test]
fn a_violation_without_a_row_fails_and_names_the_row_to_write() {
    let src = "impl T { pub fn with_x(self) -> Self { self } }";
    let r = run_gate(|t| check(t, &[], 0), &[(LIST, ""), (F, src)]);
    assert!(!r.passed());
    assert_eq!(
        r.findings[1],
        "  a  with-prefix  crates/a/src/lib.rs::T::with_x"
    );
}

#[test]
fn a_baselined_violation_passes_with_its_count() {
    let src = "impl T { pub fn with_x(self) -> Self { self } }";
    let r = run_gate(
        |t| check(t, &[], 0),
        &[
            (LIST, "a  with-prefix  crates/a/src/lib.rs::T::with_x\n"),
            (F, src),
        ],
    );
    assert!(r.passed());
    assert_eq!(
        r.out[1],
        "check-building: 1 violations, 1 rows in xtask/building-baseline.txt (a 1)"
    );
}

#[test]
fn a_row_with_no_violation_is_stale_and_fails() {
    let r = run_gate(
        |t| check(t, &[], 0),
        &[
            (LIST, "a  with-prefix  crates/a/src/lib.rs::T::with_x\n"),
            (F, "pub fn f() {}"),
        ],
    );
    assert!(!r.passed());
    assert_eq!(
        r.findings[1],
        "  a  with-prefix  crates/a/src/lib.rs::T::with_x"
    );
}

#[test]
fn a_clean_tree_passes() {
    let r = run_gate(
        |t| check(t, &[], 0),
        &[
            (LIST, ""),
            (F, "impl T { pub fn x(self) -> Self { self } }"),
        ],
    );
    assert!(r.passed());
    assert_eq!(r.out[0], "check-building: OK");
}

#[test]
fn an_allowed_item_passes_without_a_row_and_a_row_for_it_is_stale() {
    let src = "impl Shape { pub fn with_cost(self, c: u8) -> Self { self } }";
    let allow = [Allow {
        path: "crates/a/",
        owner: "Shape",
        rules: &[],
        reason: "r",
    }];
    assert!(run_gate(|t| check(t, &allow, 0), &[(LIST, ""), (F, src)]).passed());
    let r = run_gate(
        |t| check(t, &allow, 0),
        &[
            (
                LIST,
                "a  with-prefix  crates/a/src/lib.rs::Shape::with_cost\n",
            ),
            (F, src),
        ],
    );
    assert!(!r.passed());
    let other = "impl Other { pub fn with_cost(self, c: u8) -> Self { self } }";
    assert!(!run_gate(|t| check(t, &allow, 0), &[(LIST, ""), (F, other)]).passed());
}

#[test]
fn a_config_width_allow_row_covers_its_type_and_not_another_config_of_the_crate() {
    let src = "impl AlignConfig { pub fn new(r: u8) -> Self { todo!() } }\n\
               impl OtherConfig { pub fn new(r: u8) -> Self { todo!() } }\n\
               impl From<AlignConfig> for AlignNode { fn from(c: AlignConfig) -> Self { todo!() } }\n\
               impl From<OtherConfig> for OtherNode { fn from(c: OtherConfig) -> Self { todo!() } }";
    let allow = [ty("crates/a/", "AlignConfig", WIDTH, "r")];
    let r = run_gate(|t| check(t, &allow, 0), &[(LIST, ""), (F, src)]);
    assert!(!r.passed());
    assert_eq!(
        r.findings[1],
        "  a  config-width  crates/a/src/lib.rs::OtherConfig::new"
    );
    assert!(
        run_gate(
            |t| check(t, &allow, 0),
            &[
                (
                    LIST,
                    "a  config-width  crates/a/src/lib.rs::OtherConfig::new\n"
                ),
                (F, src)
            ]
        )
        .passed()
    );
}

#[test]
fn a_path_qualified_result_or_option_is_peeled_like_a_bare_one() {
    for ret in [
        "io::Result<Self>",
        "crate::Result<Self>",
        "std::io::Result<Foo>",
    ] {
        let src = format!("impl Foo {{ pub fn open() -> {ret} {{ todo!() }} }}");
        assert_eq!(rules(&src), one("open", "ctor-result"), "{ret}");
    }
    let node = "impl GainNode { pub fn new() -> std::option::Option<Self> { todo!() } }";
    assert_eq!(rules(node), one("new", "node-ctor"));
    let chain = "impl Foo { pub fn depth(self) -> io::Result<Self> { todo!() } }";
    assert_eq!(rules(chain), one("depth", "chain-result"));
}

#[test]
fn a_function_allow_row_allows_one_function_and_leaves_its_sibling_counted() {
    let src = "impl Wave { pub fn read(p: &str) -> Result<Self, E> { todo!() }\n\
               pub fn from_bytes(b: Vec<u8>) -> Result<Self, E> { todo!() } }";
    let allow = [ty("crates/a/", "Wave::read", CTOR, "r")];
    let r = run_gate(|t| check(t, &allow, 0), &[(LIST, ""), (F, src)]);
    assert_eq!(
        r.findings[1],
        "  a  ctor-result  crates/a/src/lib.rs::Wave::from_bytes"
    );
    assert!(
        run_gate(
            |t| check(t, &allow, 0),
            &[
                (
                    LIST,
                    "a  ctor-result  crates/a/src/lib.rs::Wave::from_bytes\n"
                ),
                (F, src)
            ]
        )
        .passed()
    );
    let gone = [ty("crates/a/", "Wave::gone", CTOR, "r")];
    let r = run_gate(
        |t| check(t, &gone, 0),
        &[
            (
                LIST,
                "a  ctor-result  crates/a/src/lib.rs::Wave::from_bytes\na  ctor-result  crates/a/src/lib.rs::Wave::read\n",
            ),
            (F, src),
        ],
    );
    assert!(r.findings[0].contains("ALLOW covers no violation"));
}

#[test]
fn an_allow_row_that_covers_no_violation_fails() {
    let src = "impl T { pub fn x(self) -> Self { self } }";
    let idle = [ty("crates/a/", "Gone", CTOR, "r")];
    let r = run_gate(|t| check(t, &idle, 0), &[(LIST, ""), (F, src)]);
    assert!(!r.passed());
    assert!(r.findings[0].contains("ALLOW covers no violation"));
    assert_eq!(r.findings[1], "  crates/a/ Gone: r");
    let wrong_rule = "impl Gone { pub fn with_x(self) -> Self { self } }";
    let r = run_gate(
        |t| check(t, &idle, 0),
        &[
            (LIST, "a  with-prefix  crates/a/src/lib.rs::Gone::with_x\n"),
            (F, wrong_rule),
        ],
    );
    assert!(!r.passed(), "{:?}", r.findings);
}

#[test]
fn an_allow_row_covers_only_its_rules_and_the_ffi_crates_by_name() {
    let src = "impl XConfig { pub fn new(r: u8) -> Result<Self, E> { todo!() } }";
    let one_rule = [Allow {
        path: "crates/a/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: "r",
    }];
    let r = run_gate(|t| check(t, &one_rule, 0), &[(LIST, ""), (F, src)]);
    assert_eq!(
        r.findings[1],
        "  a  ctor-result  crates/a/src/lib.rs::XConfig::new"
    );
    let ffi = [Allow {
        path: FFI,
        owner: "",
        rules: &[],
        reason: "r",
    }];
    let tree = Mem::with(&[
        (LIST, ""),
        (
            "crates/a-ffi/src/lib.rs",
            "impl T { pub fn with_x(self) -> Self { self } }",
        ),
        (
            "crates/b/src/lib.rs",
            "impl T { pub fn with_x(self) -> Self { self } }",
        ),
    ]);
    let r = check(&tree, &ffi, 0);
    assert_eq!(
        r.findings[1],
        "  b  with-prefix  crates/b/src/lib.rs::T::with_x"
    );
}

#[test]
fn a_malformed_or_misfiled_row_fails() {
    for list in [
        "a with-prefix\n",
        "a nonsense crates/a/src/lib.rs::T::f\n",
        "b with-prefix crates/a/src/lib.rs::T::f\n",
        "a with-prefix crates/a/src/lib.rs::T::f\na with-prefix crates/a/src/lib.rs::T::f\n",
    ] {
        let r = run_gate(|t| check(t, &[], 0), &[(LIST, list), (F, "")]);
        assert!(!r.passed(), "{list}");
        assert!(r.findings[0].contains("malformed"));
    }
}

#[test]
fn a_walk_that_finds_too_few_violations_fails() {
    let tree = Mem::with(&[(LIST, ""), (F, "pub fn f() {}")]);
    assert!(check(&tree, &[], 1).findings[0].contains("the walk is broken"));
}

fn prune(list: &str, src: &str) -> (String, usize) {
    pruned(&Mem::with(&[(LIST, list), (F, src)]), &[], 0).expect("prunes")
}

#[test]
fn pruning_deletes_a_stale_row_and_its_emptied_heading_and_keeps_the_rest() {
    let list = "# header\n\n# a\na  with-prefix  crates/a/src/lib.rs::T::with_x\n\
                a  with-prefix  crates/a/src/lib.rs::T::with_y\n\n# b\n\
                b  setter  crates/b/src/lib.rs::U::set_z\n";
    let src = "impl T { pub fn with_x(self) -> Self { self } }";
    let (text, removed) = prune(list, src);
    assert_eq!(removed, 2);
    assert_eq!(
        text,
        "# header\n\n# a\na  with-prefix  crates/a/src/lib.rs::T::with_x\n"
    );
}

#[test]
fn pruning_never_adds_the_row_of_a_new_violation() {
    let src = "impl T { pub fn with_x(self) -> Self { self } }";
    let (text, removed) = prune("# header\n", src);
    assert_eq!((text.as_str(), removed), ("# header\n", 0));
}
