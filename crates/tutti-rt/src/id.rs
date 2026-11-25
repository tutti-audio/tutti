//! [`id!`](crate::id!), the one way to write an id type.

/// Defines an id: a name for one thing, minted from a [`Mint`](crate::Mint)
/// and never zero.
///
/// It emits the type (`Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord,
/// Hash`, a private field), `get`, and a `Display` that writes the number. The
/// number is an identity, not a quantity: the type has no arithmetic, and the
/// first id of a fresh `Mint` is 1.
///
/// An id is **opaque** by default: the only way to hold one outside its crate
/// is to be handed it, so a holder cannot name a thing that was never minted.
/// Only the crate that defines it has `mint(&Mint)` for the next id of a
/// counter, `new(raw) -> Option` (`None` for 0) and a const `of` that panics
/// for a zero literal. An id written `struct Name: named;` is one the caller
/// chooses (the key of a map the caller fills): `mint`, `new` and `of` are
/// public.
///
/// The macro emits no serde and no `TryFrom`. A stored id derives
/// `Serialize` and writes its own `Deserialize` through `new`, so a document
/// cannot hold a zero. An id that crosses a C ABI, an IPC or a wire keeps its
/// raw type there and converts with `new` and `get`.
///
/// ```
/// use tutti_rt::Mint;
///
/// tutti_rt::id! {
///     /// One take.
///     pub struct TakeId;
/// }
/// tutti_rt::id! {
///     /// A meter a caller names.
///     pub struct MeterId: named;
/// }
///
/// let mint = Mint::new();
/// let (a, b) = (TakeId::mint(&mint), TakeId::mint(&mint));
/// assert_eq!((a.get(), b.get()), (1, 2));
/// assert_eq!(a.to_string(), "1");
///
/// assert_eq!(MeterId::new(0), None);
/// assert_eq!(MeterId::of(2).get(), 2);
/// assert_eq!(MeterId::mint(&mint).get(), 3);
/// ```
#[macro_export]
macro_rules! id {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident;
    ) => {
        $crate::id!(@define [pub(crate)] $(#[$meta])* $vis struct $name;);
    };
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident: named;
    ) => {
        $crate::id!(@define [$vis] $(#[$meta])* $vis struct $name;);
    };
    (
        @define [$($ctor:tt)*]
        $(#[$meta:meta])*
        $vis:vis struct $name:ident;
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(::core::num::NonZeroU64);

        impl $name {
            /// The next id of `mint`: one `Mint` never hands out an id twice.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            $($ctor)* fn mint(mint: &$crate::Mint) -> Self {
                Self(mint.mint())
            }

            /// The id numbered `raw`, or `None` for 0, which no `Mint` hands out.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            $($ctor)* const fn new(raw: u64) -> Option<Self> {
                match ::core::num::NonZeroU64::new(raw) {
                    Some(raw) => Some(Self(raw)),
                    None => None,
                }
            }

            /// The id numbered `raw`, for a literal or a constant. For a
            /// number that arrives from outside, use [`new`](Self::new).
            ///
            /// # Panics
            ///
            /// When `raw` is 0; in a `const` that is a compile error.
            #[inline]
            #[must_use]
            #[track_caller]
            #[allow(dead_code)]
            $($ctor)* const fn of(raw: u64) -> Self {
                match Self::new(raw) {
                    Some(id) => id,
                    None => panic!("an id is not 0"),
                }
            }

            /// The number.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            $vis const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::Mint;

    crate::id! {
        /// A test id.
        pub(super) struct WidgetId;
    }

    crate::id! {
        /// A test id a caller names.
        pub(super) struct NamedId: named;
    }

    #[test]
    fn a_named_id_is_built_from_a_number_and_a_zero_is_refused() {
        assert_eq!(NamedId::of(3).get(), 3);
        assert_eq!(NamedId::new(0), None);
        assert_eq!(NamedId::mint(&Mint::new()), NamedId::of(1));
    }

    #[test]
    fn a_fresh_mint_starts_at_one_and_counts_up() {
        let mint = Mint::new();
        let ids: Vec<u64> = (0..3).map(|_| WidgetId::mint(&mint).get()).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn zero_is_not_an_id() {
        assert_eq!(WidgetId::new(0), None);
        assert_eq!(WidgetId::new(7).map(WidgetId::get), Some(7));
    }

    #[test]
    #[should_panic(expected = "an id is not 0")]
    fn of_refuses_zero() {
        let _ = WidgetId::of(0);
    }

    #[test]
    fn display_is_the_bare_number_and_debug_names_the_type() {
        let id = WidgetId::of(12);
        assert_eq!(id.to_string(), "12");
        assert_eq!(format!("{id:>4}"), "  12");
        assert_eq!(format!("{id:?}"), "WidgetId(12)");
    }

    #[test]
    fn ids_order_and_compare_by_number() {
        let mint = Mint::new();
        let (a, b) = (WidgetId::mint(&mint), WidgetId::mint(&mint));
        assert!(a < b);
        assert_ne!(a, b);
        assert_eq!(a, WidgetId::of(1));
    }

    #[test]
    fn an_id_is_one_word_and_an_option_of_it_is_too() {
        assert_eq!(size_of::<WidgetId>(), 8);
        assert_eq!(size_of::<Option<WidgetId>>(), 8);
    }
}
