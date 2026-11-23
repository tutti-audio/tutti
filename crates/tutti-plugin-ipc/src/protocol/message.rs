//! The control connection's messages, declared once.
//!
//! `messages!` turns one table of rows into every type a control message has on
//! the wire: a struct per request, a struct per reply, the [`ControlRequest`] and
//! [`ControlReply`] enums over them, and each request's [`Message`] impl. The
//! table is `envelope.rs`'s, and its row order is the wire's.

use super::envelope::{ControlReply, ControlRequest};

/// A request the control connection carries, and what answers it.
///
/// Implemented by the macro for every row; nothing implements it by hand.
pub trait Message: Into<ControlRequest> {
    /// What the server answers with: the row's reply struct, or `()` for a request
    /// nothing answers.
    type Reply;
    /// Whether the server answers the request. A request that is answered has a
    /// `Reply` that is built from, and only from, the [`ControlReply`] variant of
    /// the same name.
    const REPLIES: bool;
}

/// A request the server answers: its [`Reply`](Message::Reply) is built from, and
/// only from, the [`ControlReply`] of its row, so a call
/// that asks it gets that reply or a refusal and never another kind.
pub trait Answered: Message<Reply: TryFrom<ControlReply, Error = ControlReply>> {}

/// A request nothing answers: only a tell can carry it.
pub trait Unanswered: Message<Reply = ()> {}

/// Declares the control messages: one row per request, in wire order.
///
/// A row is the request's docs, its name, its fields in braces (none for a unit
/// request), then an optional `->` and the reply it is answered with, written the
/// same way, then `;`:
///
/// ```text
/// /// Read one parameter's current normalized value.
/// GetParameter { param_id: ParamId }
/// ->
/// /// One parameter's normalized value, or `None` if the plugin declined.
/// ParameterValue {
///     #[serde(with = "super::wire::normalized_opt")]
///     value: Option<Normalized>,
/// };
/// ```
///
/// A row with no `->` is a request nothing answers.
///
/// It declares, for every row, a `pub struct` for the request and one for the
/// reply (docs and field attributes carried through), a variant of the same
/// name and fields in [`ControlRequest`] and in [`ControlReply`] (which ends in
/// `Error`, the failure any request may be answered with), `From<Request> for
/// ControlRequest`, `From<Reply> for ControlReply`, `TryFrom<ControlReply>` for
/// the reply (the one place a reply's variant is matched), and [`Message`] for
/// the request, with [`Answered`] or [`Unanswered`] by whether it has a reply.
///
/// A request's wire tag is its row's place among the rows, and a reply's is its
/// place among the rows that have one: append a row, never reorder them, and bump
/// [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).
macro_rules! messages {
    (@request $(#[$meta:meta])* $name:ident { $($(#[$fmeta:meta])* $field:ident : $fty:ty),* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct $name {
            $($(#[$fmeta])* pub $field: $fty,)*
        }

        impl From<$name> for ControlRequest {
            fn from(m: $name) -> Self {
                Self::$name { $($field: m.$field),* }
            }
        }
    };
    (@request $(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
        pub struct $name;

        impl From<$name> for ControlRequest {
            fn from(_: $name) -> Self {
                Self::$name
            }
        }
    };
    (@reply $(#[$meta:meta])* $name:ident { $($(#[$fmeta:meta])* $field:ident : $fty:ty),* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct $name {
            $($(#[$fmeta])* pub $field: $fty,)*
        }

        impl From<$name> for ControlReply {
            fn from(m: $name) -> Self {
                Self::$name { $($field: m.$field),* }
            }
        }

        impl TryFrom<ControlReply> for $name {
            type Error = ControlReply;

            fn try_from(reply: ControlReply) -> Result<Self, ControlReply> {
                match reply {
                    ControlReply::$name { $($field),* } => Ok(Self { $($field),* }),
                    other => Err(other),
                }
            }
        }
    };
    (@reply $(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
        pub struct $name;

        impl From<$name> for ControlReply {
            fn from(_: $name) -> Self {
                Self::$name
            }
        }

        impl TryFrom<ControlReply> for $name {
            type Error = ControlReply;

            fn try_from(reply: ControlReply) -> Result<Self, ControlReply> {
                match reply {
                    ControlReply::$name => Ok(Self),
                    other => Err(other),
                }
            }
        }
    };
    (@impl $name:ident -> $reply:ident) => {
        impl Message for $name {
            type Reply = $reply;
            const REPLIES: bool = true;
        }

        impl Answered for $name {}
    };
    (@impl $name:ident) => {
        impl Message for $name {
            type Reply = ();
            const REPLIES: bool = false;
        }

        impl Unanswered for $name {}
    };
    ($(
        $(#[$meta:meta])*
        $name:ident $({ $($(#[$fmeta:meta])* $field:ident : $fty:ty),* $(,)? })?
        $(->
            $(#[$rmeta:meta])*
            $reply:ident $({ $($(#[$rfmeta:meta])* $rfield:ident : $rfty:ty),* $(,)? })?
        )?
        ;
    )*) => {
        $(
            messages! { @request $(#[$meta])* $name $({ $($(#[$fmeta])* $field : $fty),* })? }
            $(messages! { @reply $(#[$rmeta])* $reply $({ $($(#[$rfmeta])* $rfield : $rfty),* })? })?
            messages! { @impl $name $(-> $reply)? }
        )*

        /// A request on the control connection: everything but a block.
        ///
        /// One variant per row of the message table, with the fields of the
        /// row's request struct. Variant order is wire-significant; see the
        /// module docs.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum ControlRequest {
            $(
                $(#[$meta])*
                $name $({ $($(#[$fmeta])* $field : $fty),* })?,
            )*
        }

        /// What the server answers a [`ControlRequest`] with.
        ///
        /// One variant per row that has a reply, with the fields of the row's
        /// reply struct, and [`Error`](Self::Error) last. Variant order is
        /// wire-significant; see the module docs.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum ControlReply {
            $($(
                $(#[$rmeta])*
                $reply $({ $($(#[$rfmeta])* $rfield : $rfty),* })?,
            )?)*
            /// The subprocess failed at something it was asked to do, in place of
            /// the answer a request awaits.
            Error {
                /// What failed.
                error: ServerError,
            },
        }

        impl ControlRequest {
            /// How many requests there are.
            pub const VARIANTS: usize = [$(stringify!($name)),*].len();

            /// Whether the server answers this request.
            pub fn expects_reply(&self) -> bool {
                match self {
                    $(Self::$name { .. } => <$name as Message>::REPLIES,)*
                }
            }
        }

        impl ControlReply {
            /// How many replies there are, the failure included.
            pub const VARIANTS: usize = 1 $($(+ { let _ = stringify!($reply); 1 })?)*;
        }
    };
}
