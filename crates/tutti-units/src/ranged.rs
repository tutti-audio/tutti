//! [`ranged!`](crate::ranged!), the one way to write a range-checked type.

/// Defines a range-checked type: a raw value its `new` accepts and never
/// clamps, in the `MidiChannel` shape, and its refused-raw error.
///
/// It emits the type (`Clone, Copy, Debug, PartialEq`, a private field), its
/// error carrying the refused raw (derives `thiserror::Error`, so the calling
/// crate depends on `thiserror`), and `new(raw) -> Option`, a const `of` that
/// panics for a refused literal, `get`, `TryFrom<Raw>` agreeing with `new`, and
/// `From<Self> for Raw`. `$valid` is the range as a `const` expression over the
/// raw named by `$raw`; a NaN or an infinity must fail it.
///
/// It emits no serde. A type that is stored adds it among the attributes,
/// through `try_from` so a document cannot hold a refused raw:
/// `#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(try_from = "Raw", into = "Raw"))]`.
/// Test every type with `tutti_fixtures::check_range_contract`.
///
/// ```
/// tutti_units::ranged! {
///     /// How many voices, `1..=8`.
///     Voices(u8), VoicesOutOfRange, "within 1..=8", |raw| raw >= 1 && raw <= 8
/// }
///
/// assert_eq!(Voices::new(3).map(Voices::get), Some(3));
/// assert_eq!(Voices::new(0), None);
/// assert_eq!(Voices::try_from(9), Err(VoicesOutOfRange(9)));
/// ```
///
/// `bits $width` in place of the predicate is the range `0..=(1 << $width) - 1`
/// of a bit-width field. It adds `pub const MAX` and `pub const fn masked(raw)`,
/// which keeps the low `$width` bits of a raw the wire has just framed, and its
/// error derives `Eq`.
///
/// ```
/// tutti_units::ranged! {
///     /// A 5-bit field.
///     Five(u8), FiveOutOfRange, bits 5
/// }
///
/// assert_eq!(Five::MAX.get(), 31);
/// assert_eq!(Five::new(32), None);
/// assert_eq!(Five::masked(33).get(), 1);
/// assert_eq!(Five::try_from(32), Err(FiveOutOfRange(32)));
/// ```
///
/// A width as wide as the integer is refused at compile time:
///
/// ```compile_fail
/// tutti_units::ranged! {
///     /// A field as wide as its integer.
///     Foo(u8), FooOutOfRange, bits 8
/// }
/// ```
#[macro_export]
macro_rules! ranged {
    (
        $(#[$meta:meta])*
        $name:ident($inner:ty), $err:ident, $what:literal, |$raw:ident| $valid:expr
    ) => {
        $crate::ranged!(@items [] $(#[$meta])* $name($inner), $err, $what, |$raw| $valid);
    };
    (
        $(#[$meta:meta])*
        $name:ident($inner:ty), $err:ident, bits $width:literal
    ) => {
        $crate::ranged!(
            @items [Eq]
            $(#[$meta])*
            $name($inner), $err, concat!("within ", $width, " bits"),
            |raw| raw <= Self::MAX.0
        );

        const _: () = assert!(
            $width < <$inner>::BITS,
            "a bits field must be narrower than its integer"
        );

        impl $name {
            #[doc = concat!("The largest value, ", $width, " bits all set.")]
            pub const MAX: Self = Self(((1 as $inner) << $width) - 1);

            /// The low bits of `raw` the type holds. Only for bits the wire
            /// has just framed; a value from anywhere else is checked with
            /// [`new`](Self::new).
            #[inline]
            #[must_use]
            pub const fn masked(raw: $inner) -> Self {
                Self(raw & Self::MAX.0)
            }
        }
    };
    (
        @items [$($extra:ident),*]
        $(#[$meta:meta])*
        $name:ident($inner:ty), $err:ident, $what:expr, |$raw:ident| $valid:expr
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name($inner);

        #[doc = concat!("A raw value that is not ", $what, ".")]
        #[derive(Clone, Copy, Debug, PartialEq, $($extra,)* thiserror::Error)]
        #[error("{0:?} is not {what}", what = $what)]
        pub struct $err(pub $inner);

        impl $name {
            #[doc = concat!("The value, or `None` unless the raw is ", $what, ". Never clamps.")]
            #[inline]
            #[must_use]
            pub const fn new($raw: $inner) -> Option<Self> {
                if $valid { Some(Self($raw)) } else { None }
            }

            /// The value for a literal or a constant. For a value that
            /// arrives from outside, use [`new`](Self::new).
            ///
            /// # Panics
            ///
            /// When the raw is refused by [`new`](Self::new); in a `const` that
            /// is a compile error.
            #[inline]
            #[must_use]
            #[track_caller]
            pub const fn of(raw: $inner) -> Self {
                match Self::new(raw) {
                    Some(value) => value,
                    None => panic!(concat!("not ", $what)),
                }
            }

            /// The raw value.
            #[inline]
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl TryFrom<$inner> for $name {
            type Error = $err;

            #[inline]
            fn try_from(raw: $inner) -> core::result::Result<Self, $err> {
                Self::new(raw).ok_or($err(raw))
            }
        }

        impl From<$name> for $inner {
            #[inline]
            fn from(value: $name) -> $inner {
                value.0
            }
        }
    };
}
