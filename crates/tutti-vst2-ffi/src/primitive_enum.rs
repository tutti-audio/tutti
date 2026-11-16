//! `primitive_enum!`: a fieldless enum with its conversions to and from its
//! `repr` integer, declared from one variant list.

/// A value no variant of the enum has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no variant has the value {0}")]
pub struct UnknownPrimitive(pub i64);

/// Declares `$name` with `#[repr($repr)]`, `TryFrom<$repr>` (an unknown value is
/// `UnknownPrimitive`) and `From<$name> for $repr`, from the one variant list.
macro_rules! primitive_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident : $repr:ident {
            $( $(#[$vmeta:meta])* $variant:ident $(= $disc:expr)? ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr($repr)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant $(= $disc)? ),*
        }

        impl TryFrom<$repr> for $name {
            type Error = $crate::UnknownPrimitive;

            fn try_from(value: $repr) -> Result<Self, Self::Error> {
                $( if value == $name::$variant as $repr { return Ok($name::$variant); } )*
                Err($crate::UnknownPrimitive(value as i64))
            }
        }

        impl From<$name> for $repr {
            fn from(value: $name) -> $repr {
                value as $repr
            }
        }

        #[cfg(test)]
        impl $name {
            /// Every variant decodes from its own value, and the value past the
            /// largest one is refused.
            pub(crate) fn assert_round_trips() {
                let all = [ $( $name::$variant ),* ];
                let mut largest = $repr::MIN;
                for variant in all {
                    let value: $repr = variant.into();
                    let back = $name::try_from(value).expect("a variant's own value decodes");
                    assert_eq!($repr::from(back), value);
                    largest = largest.max(value);
                }
                assert_eq!(
                    $name::try_from(largest + 1).map(|_| ()),
                    Err($crate::UnknownPrimitive(largest as i64 + 1))
                );
            }
        }
    };
}
