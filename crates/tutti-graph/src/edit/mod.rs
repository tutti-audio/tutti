//! How a graph is edited: the [`Editor`](crate::Editor) a control thread
//! holds, with the commit it sends to the executor, and the
//! [`GraphBuilder`](crate::GraphBuilder) for a graph that does not change
//! while it plays.

pub(crate) mod builder;
pub(crate) mod editor;
