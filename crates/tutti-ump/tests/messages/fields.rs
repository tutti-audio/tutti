//! The bit-width fields (`U4`, `U7`, `U14`, `NoteNumber`, `DeltaTicks`) to the
//! shape of a range-checked value: `new`
//! and `TryFrom` refuse above the width and agree, `of` panics there, `masked`
//! keeps the low bits, `get` and `From` give the raw back, and serde (feature
//! `serde`) refuses what `new` refuses. `tutti-ump` depends on no workspace
//! crate, so this is its own `check_range_contract`.

use tutti_ump::{
    DeltaTicks, MonoChannels, MpeChannels, NotSharpsFlats, NoteNumber, OutOfRange, SharpsFlats, U4,
    U7, U14,
};

macro_rules! contract {
    ($ty:ident, $repr:ty, $width:expr) => {{
        let max: $repr = (1 << $width) - 1;
        let over: $repr = max + 1;
        for raw in [0, 1, max] {
            let v = $ty::new(raw).unwrap();
            assert_eq!(v.get(), raw);
            assert_eq!(<$repr>::from(v), raw);
            assert_eq!($ty::try_from(raw), Ok(v));
            assert_eq!($ty::of(raw), v);
            assert_eq!($ty::masked(raw), v);
        }
        assert_eq!($ty::MAX.get(), max);
        for raw in [over, <$repr>::MAX] {
            assert_eq!($ty::new(raw), None, "{} {raw}", stringify!($ty));
            assert_eq!(
                $ty::try_from(raw),
                Err(OutOfRange {
                    what: stringify!($ty),
                    value: raw as u32,
                    max: max as u32,
                })
            );
            assert_eq!($ty::masked(raw).get(), raw & max);
            assert!(
                std::panic::catch_unwind(|| $ty::of(raw)).is_err(),
                "{}::of({raw}) panics",
                stringify!($ty)
            );
        }
    }};
}

#[test]
fn each_field_keeps_the_contract() {
    contract!(U4, u8, 4);
    contract!(U7, u8, 7);
    contract!(U14, u16, 14);
    contract!(NoteNumber, u8, 7);
    contract!(DeltaTicks, u32, 20);
}

#[test]
fn of_is_usable_in_a_const() {
    const MIDDLE_C: NoteNumber = NoteNumber::of(60);
    const CENTRE: U14 = U14::of(8192);
    assert_eq!((MIDDLE_C.get(), CENTRE.get()), (60, 8192));
}

#[cfg(feature = "serde")]
#[test]
fn serde_refuses_what_new_refuses() {
    assert_eq!(serde_json::to_string(&U7::of(127)).unwrap(), "127");
    assert_eq!(serde_json::from_str::<U7>("127").unwrap(), U7::of(127));
    assert!(serde_json::from_str::<U7>("128").is_err());
    assert_eq!(serde_json::from_str::<U14>("16383").unwrap(), U14::MAX);
    assert!(serde_json::from_str::<U14>("16384").is_err());
    assert!(serde_json::from_str::<NoteNumber>("200").is_err());
    assert!(serde_json::from_str::<DeltaTicks>("1048576").is_err());
    assert!(serde_json::from_str::<U4>("16").is_err());
}

/// The bounded fields' conversions over the whole raw range: `TryFrom` agrees
/// with `new`, `From` gives `get` back, and the error carries the refused raw.
#[test]
fn the_bounded_conversions_agree_with_new_over_every_raw() {
    macro_rules! unsigned {
        ($ty:ident) => {
            for raw in 0..=u8::MAX {
                match $ty::new(raw) {
                    Some(v) => {
                        assert_eq!($ty::try_from(raw), Ok(v));
                        assert_eq!(u8::from(v), v.get());
                        assert_eq!(v.get(), raw);
                    }
                    None => {
                        let Err(e) = $ty::try_from(raw) else {
                            panic!("{} {raw}", stringify!($ty))
                        };
                        assert_eq!((e.what, e.value), (stringify!($ty), u32::from(raw)));
                    }
                }
            }
        };
    }
    unsigned!(MpeChannels);
    unsigned!(MonoChannels);
    for raw in i8::MIN..=i8::MAX {
        match SharpsFlats::new(raw) {
            Some(v) => {
                assert_eq!(SharpsFlats::try_from(raw), Ok(v));
                assert_eq!(i8::from(v), v.get());
                assert_eq!(v.get(), raw);
            }
            None => assert_eq!(SharpsFlats::try_from(raw), Err(NotSharpsFlats(raw))),
        }
    }
}
