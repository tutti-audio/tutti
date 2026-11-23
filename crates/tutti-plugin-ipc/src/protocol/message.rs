//! The control connection's messages, declared once.
//!
//! `messages!` turns one table of rows into every type a control message has on
//! the wire: a struct per request, a struct per reply, the [`ControlRequest`] and
//! [`ControlReply`] enums over them, and each request's [`Message`] impl. The
//! table is `envelope.rs`'s, and its row order is the wire's.

use super::envelope::{ControlReply, ControlRequest, Working};
use tutti_plugin_types::{MainThreadMarker, ServerError};

/// What the server's loop does once a request is handled.
// One value per message, moved once to the transport loop; a `ControlReply`
// carries a block's inline MIDI list.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Reaction {
    /// Send this reply back to the host.
    Reply(ControlReply),
    /// Send the plugin's saved state back: a `StateReady` frame and the object
    /// holding these bytes.
    ///
    /// The state does not fit a frame, so it is answered as an object. The handler
    /// returns it rather than writing to the transport, so handling stays a pure
    /// function of the request, testable without a socket.
    State(Vec<u8>),
    /// No reply; keep looping.
    None,
    /// The host asked for shutdown; leave the loop.
    Shutdown,
}

impl From<ControlReply> for Reaction {
    fn from(reply: ControlReply) -> Self {
        Self::Reply(reply)
    }
}

/// A tell is handled and nothing is sent.
impl From<()> for Reaction {
    fn from((): ()) -> Self {
        Self::None
    }
}

/// A handled request is its reply, or the failure that answers in its place.
impl<R: Into<Reaction>> From<Result<R, ServerError>> for Reaction {
    fn from(handled: Result<R, ServerError>) -> Self {
        match handled {
            Ok(reply) => reply.into(),
            Err(error) => Self::Reply(ControlReply::Error { error }),
        }
    }
}

/// A plugin's saved state: what handling [`SaveState`](super::SaveState) returns
/// in place of its [`StateReady`](super::StateReady), whose object the transport
/// makes.
#[derive(Debug)]
pub struct SavedState(pub Vec<u8>);

impl From<SavedState> for Reaction {
    fn from(state: SavedState) -> Self {
        Self::State(state.0)
    }
}

/// What handling [`Shutdown`](super::Shutdown) returns: the loop ends.
#[derive(Debug)]
pub struct Stop;

impl From<Stop> for Reaction {
    fn from(Stop: Stop) -> Self {
        Self::Shutdown
    }
}

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
    /// What handling the request returns: the row's reply or the failure that
    /// answers in its place, `()` for a request nothing answers, or the row's
    /// own outcome where the table says (`=>`).
    type Handled: Into<Reaction>;
}

/// What a handler has of the loop that serves it: the main thread's proof, and
/// the sink for what a long handling has reached.
pub struct Handling<'a> {
    /// The main thread, which handling runs on.
    pub main: MainThreadMarker,
    interim: &'a mut dyn FnMut(Working),
}

impl std::fmt::Debug for Handling<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handling")
            .field("main", &self.main)
            .finish_non_exhaustive()
    }
}

impl<'a> Handling<'a> {
    /// Handling on `main`, with `interim` hearing what a handler reports.
    pub fn new(main: MainThreadMarker, interim: &'a mut dyn FnMut(Working)) -> Self {
        Self { main, interim }
    }

    /// Tells the host a long handling has reached `stage`
    /// ([`Working::Restoring`]), so what it waits for changes with it.
    pub fn interim(&mut self, stage: Working) {
        (self.interim)(stage);
    }
}

/// The server's handling of one request: it returns the row's
/// [`Handled`](Message::Handled), so a handler that answers with another row's
/// reply does not compile.
pub trait Handle<M: Message> {
    /// Handles `msg` in the loop `cx` describes.
    fn handle(&mut self, cx: &mut Handling<'_>, msg: M) -> M::Handled;
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
/// A row with no `->` is a request nothing answers. A row may end in `=>` and
/// the type its handler returns in place of the reply (`Result<Reply,
/// ServerError>`, or `()` for a request nothing answers): `SaveState` returns
/// the state's bytes, whose object the transport makes, and `Shutdown` returns
/// [`Stop`].
///
/// It declares, for every row, a `pub struct` for the request and one for the
/// reply (docs and field attributes carried through), a newtype variant of the
/// same name over the struct in [`ControlRequest`] and in [`ControlReply`] (which ends in
/// `Error`, the failure any request may be answered with), `From<Request> for
/// ControlRequest`, `From<Reply> for ControlReply`, `TryFrom<ControlReply>` for
/// the reply (the one place a reply's variant is matched), and [`Message`] for
/// the request, with [`Answered`] or [`Unanswered`] by whether it has a reply.
/// It also declares `serve`, the one match over [`ControlRequest`]: it calls the
/// [`Handle`] of the row and returns what that handled as a [`Reaction`], and it
/// compiles only for a handler of every row.
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
                Self::$name(m)
            }
        }
    };
    (@request $(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
        pub struct $name;

        impl From<$name> for ControlRequest {
            fn from(m: $name) -> Self {
                Self::$name(m)
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
                Self::$name(m)
            }
        }

        impl From<$name> for Reaction {
            fn from(m: $name) -> Self {
                Self::Reply(m.into())
            }
        }

        impl TryFrom<ControlReply> for $name {
            type Error = ControlReply;

            fn try_from(reply: ControlReply) -> Result<Self, ControlReply> {
                match reply {
                    ControlReply::$name(m) => Ok(m),
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
            fn from(m: $name) -> Self {
                Self::$name(m)
            }
        }

        impl From<$name> for Reaction {
            fn from(m: $name) -> Self {
                Self::Reply(m.into())
            }
        }

        impl TryFrom<ControlReply> for $name {
            type Error = ControlReply;

            fn try_from(reply: ControlReply) -> Result<Self, ControlReply> {
                match reply {
                    ControlReply::$name(m) => Ok(m),
                    other => Err(other),
                }
            }
        }
    };
    (@impl $name:ident -> $reply:ident => $handled:ty) => {
        impl Message for $name {
            type Reply = $reply;
            const REPLIES: bool = true;
            type Handled = $handled;
        }

        impl Answered for $name {}
    };
    (@impl $name:ident -> $reply:ident) => {
        messages! { @impl $name -> $reply => Result<$reply, ServerError> }
    };
    (@impl $name:ident => $handled:ty) => {
        impl Message for $name {
            type Reply = ();
            const REPLIES: bool = false;
            type Handled = $handled;
        }

        impl Unanswered for $name {}
    };
    (@impl $name:ident) => {
        messages! { @impl $name => () }
    };
    ($(
        $(#[$meta:meta])*
        $name:ident $({ $($(#[$fmeta:meta])* $field:ident : $fty:ty),* $(,)? })?
        $(->
            $(#[$rmeta:meta])*
            $reply:ident $({ $($(#[$rfmeta:meta])* $rfield:ident : $rfty:ty),* $(,)? })?
        )?
        $(=> $handled:ty)?
        ;
    )*) => {
        $(
            messages! { @request $(#[$meta])* $name $({ $($(#[$fmeta])* $field : $fty),* })? }
            $(messages! { @reply $(#[$rmeta])* $reply $({ $($(#[$rfmeta])* $rfield : $rfty),* })? })?
            messages! { @impl $name $(-> $reply)? $(=> $handled)? }
        )*

        /// A request on the control connection: everything but a block.
        ///
        /// One variant per row of the message table, a newtype over the row's
        /// request struct. Variant order is wire-significant; see the
        /// module docs.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum ControlRequest {
            $(
                $(#[$meta])*
                $name($name),
            )*
        }

        /// What the server answers a [`ControlRequest`] with.
        ///
        /// One variant per row that has a reply, a newtype over the row's reply
        /// struct, and [`Error`](Self::Error) last. Variant order is
        /// wire-significant; see the module docs.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum ControlReply {
            $($(
                $(#[$rmeta])*
                $reply($reply),
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
                    $(Self::$name(_) => <$name as Message>::REPLIES,)*
                }
            }
        }

        /// Handles `request` with the handler of its row and returns what the
        /// server's loop does next.
        pub fn serve<H>(handler: &mut H, cx: &mut Handling<'_>, request: ControlRequest) -> Reaction
        where
            H: $(Handle<$name> +)*
        {
            match request {
                $(
                    ControlRequest::$name(msg) => {
                        <H as Handle<$name>>::handle(handler, cx, msg).into()
                    }
                )*
            }
        }

        impl ControlReply {
            /// How many replies there are, the failure included.
            pub const VARIANTS: usize = 1 $($(+ { let _ = stringify!($reply); 1 })?)*;
        }
    };
}
