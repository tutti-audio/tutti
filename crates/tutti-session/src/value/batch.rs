//! What the verbs take: [`IntoAssign`] for `set` and [`IntoRead`] for `get`,
//! implemented for one item, tuples of up to 12 (nested too), arrays, `Vec`s
//! and `Option`s, and the [`Batch`] a `set` runs an argument through.
//!
//! A write has two phases. Phase 1 stages every item: it resolves its target,
//! runs every `modify` in argument order (each sees the value the call has
//! built so far for its target) and checks the value. Phase 2 stores the final
//! value of each target and cannot fail.

use std::any::Any;

use super::error::{SetRefusal, ValueErrorKind};
use super::state::Params;
use super::target::{AnyAssign, Assign, Info, Modify, Staged, TargetId, sealed::Store};
use super::watcher::{Landed, Landing};

/// An item that failed: its position, and what failed there.
#[derive(Debug)]
pub struct Failure {
    pub at: usize,
    pub target: Option<TargetId>,
    pub kind: ValueErrorKind,
}

impl std::fmt::Debug for dyn Staged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Staged").finish_non_exhaustive()
    }
}

/// One target's final value in a batch, with the cell it is stored through
/// and the base it was first read at.
struct Slot<T: Store> {
    id: TargetId,
    cell: T::Cell,
    base: T::Value,
    value: T::Value,
}

impl<T: Store> Staged for Slot<T> {
    fn id(&self) -> TargetId {
        self.id
    }

    fn value_mut(&mut self) -> &mut dyn Any {
        &mut self.value
    }

    fn accepts(&self, value: &dyn Any) -> Result<(), SetRefusal> {
        let value = value
            .downcast_ref::<T::Value>()
            .expect("a target id names one value type");
        T::check(&self.cell, value)
    }

    fn store(self: Box<Self>) -> Option<Landing> {
        T::store(&self.cell, &self.value);
        // A value equal to the base is no change.
        (self.value != self.base).then(|| Landing::new(self.id, self.base, self.value))
    }
}

/// A `set`'s argument, staged and not yet landed.
#[derive(Debug)]
pub struct Batch<'a> {
    params: &'a Params,
    slots: Vec<Box<dyn Staged>>,
    /// The position of the item being staged.
    index: usize,
    /// The assignments staged so far: each `Assign`, `Modify` and `AnyAssign`.
    writes: usize,
}

impl<'a> Batch<'a> {
    pub fn new(params: &'a Params) -> Self {
        Self {
            params,
            slots: Vec::new(),
            index: 0,
            writes: 0,
        }
    }

    /// Stores every target's final value, in the order each was first named,
    /// and answers the targets whose value is not the base they had.
    pub fn store(self) -> Landed {
        let writes = self.writes;
        let landings = self.slots.into_iter().filter_map(Staged::store).collect();
        Landed::new(writes, landings)
    }

    fn fail(&self, target: TargetId, kind: ValueErrorKind) -> Failure {
        Failure {
            at: self.index,
            target: Some(target),
            kind,
        }
    }

    /// The failure of a target that resolved to no cell: the refusal its
    /// declaration gives, else that it is not there.
    fn unresolved<T: Store>(&self, target: T) -> Failure {
        let kind = target
            .unresolved(self.params)
            .map_or(ValueErrorKind::NoSuchTarget, ValueErrorKind::Refused);
        self.fail(target.id(), kind)
    }

    /// Stages the value `make` builds from the target's value so far in this
    /// batch, or from its base when it has none.
    fn apply<T: Store>(
        &mut self,
        target: T,
        make: impl FnOnce(&T::Value) -> T::Value,
    ) -> Result<(), Failure> {
        let id = target.id();
        self.writes += 1;
        // A target id holds the node type its key names, and one node type
        // declares one unit at an address, so every target of one id has one
        // value type: a key of another node type resolves to no cell and
        // never reaches a slot. The downcasts cannot fail, bar a plugin
        // parameter named in two units (below).
        const ONE_TYPE: &str = "one target id names one value type";
        match self.slots.iter().position(|s| s.id() == id) {
            Some(at) => {
                let slot = &mut self.slots[at];
                // One plugin parameter has one id in every unit, so a second
                // target of it may hold another value type than the staged one.
                let Some(current) = slot.value_mut().downcast_ref::<T::Value>() else {
                    let refusal = ValueErrorKind::Refused(SetRefusal::MixedValueTypes);
                    return Err(self.fail(id, refusal));
                };
                let next = make(current);
                let accepted = slot.accepts(&next);
                if accepted.is_ok() {
                    *slot.value_mut().downcast_mut::<T::Value>().expect(ONE_TYPE) = next;
                }
                accepted.map_err(|r| self.fail(id, ValueErrorKind::Refused(r)))?;
            }
            None => {
                let (cell, base) = target
                    .resolve(self.params)
                    .ok_or_else(|| self.unresolved(target))?;
                let next = make(&base);
                T::check(&cell, &next).map_err(|r| self.fail(id, ValueErrorKind::Refused(r)))?;
                self.slots.push(Box::new(Slot::<T> {
                    id,
                    cell,
                    base,
                    value: next,
                }));
            }
        }
        self.index += 1;
        Ok(())
    }

    /// Reads the base of `target`.
    fn read_base<T: Store>(&mut self, target: T) -> Result<T::Value, Failure> {
        let (_, base) = target
            .resolve(self.params)
            .ok_or_else(|| self.unresolved(target))?;
        self.index += 1;
        Ok(base)
    }

    pub fn params(&self) -> &Params {
        self.params
    }

    /// Counts an item that stages nothing (`None`).
    fn skip(&mut self) {
        self.index += 1;
    }
}

/// The sealed halves of the argument traits, named `Sealed` as a sealed trait is.
mod seal {
    pub mod stage {
        use super::super::{Batch, Failure};

        /// An item or a collection of items a batch stages.
        pub trait Sealed {
            fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure>;
        }
    }

    pub mod read {
        use super::super::{Batch, Failure};

        /// An item or a collection of reads a batch answers.
        pub trait Sealed {
            /// What the read answers.
            type Output;

            fn read(self, batch: &mut Batch<'_>) -> Result<Self::Output, Failure>;
        }
    }
}
pub use seal::read::Sealed as Readable;
pub use seal::stage::Sealed as Stageable;

/// What [`Session::set`](crate::Session::set) takes: an
/// [`Assign`], a [`Modify`] or an [`AnyAssign`], or tuples of up to 12 of them (nested
/// too), arrays, `Vec`s of one item type, and `Option`s (`None` writes nothing
/// but keeps its position). Sealed.
pub trait IntoAssign<K>: Stageable {}

/// What [`Session::get`](crate::Session::get) takes: a target (its base), or
/// tuples of up to 12 of them (nested too), arrays, `Vec`s and `Option`s. It
/// answers in the argument's shape. Sealed.
pub trait IntoRead: Readable {}

impl<T: Store> Stageable for Assign<T> {
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        let value = self.value;
        batch.apply(self.target, move |_| value)
    }
}
impl<K, T: Store> IntoAssign<K> for Assign<T> {}

impl<T: Store, F> Stageable for Modify<T, F>
where
    F: FnOnce(&T::Value) -> T::Value,
{
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        batch.apply(self.target, self.edit)
    }
}
impl<K, T: Store, F> IntoAssign<K> for Modify<T, F> where F: FnOnce(&T::Value) -> T::Value {}

impl Stageable for AnyAssign {
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        self.0.stage(batch)
    }
}
impl<K> IntoAssign<K> for AnyAssign {}

impl<A: Stageable> Stageable for Option<A> {
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        match self {
            Some(item) => item.stage(batch),
            None => {
                batch.skip();
                Ok(())
            }
        }
    }
}
impl<K, A: IntoAssign<K>> IntoAssign<K> for Option<A> {}

impl<A: Stageable> Stageable for Vec<A> {
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        self.into_iter().try_for_each(|item| item.stage(batch))
    }
}
impl<K, A: IntoAssign<K>> IntoAssign<K> for Vec<A> {}

impl<A: Stageable, const N: usize> Stageable for [A; N] {
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        self.into_iter().try_for_each(|item| item.stage(batch))
    }
}
impl<K, A: IntoAssign<K>, const N: usize> IntoAssign<K> for [A; N] {}

impl<T: Store> Readable for T {
    type Output = T::Value;

    fn read(self, batch: &mut Batch<'_>) -> Result<T::Value, Failure> {
        batch.read_base(self)
    }
}
impl<T: Store> IntoRead for T {}

impl<T: Store> Readable for Info<T>
where
    Info<T>: Describe,
{
    type Output = <Info<T> as Describe>::Output;

    fn read(self, batch: &mut Batch<'_>) -> Result<Self::Output, Failure> {
        let r = self.describe(batch.params());
        batch.finish_read(r)
    }
}
impl<T: Store> IntoRead for Info<T> where Info<T>: Describe {}

/// What a target's [`Info`] answers.
pub trait Describe {
    type Output;
    fn describe(self, params: &Params) -> Result<Self::Output, (TargetId, ValueErrorKind)>;
}

impl Batch<'_> {
    fn finish_read<O>(&mut self, r: Result<O, (TargetId, ValueErrorKind)>) -> Result<O, Failure> {
        match r {
            Ok(o) => {
                self.index += 1;
                Ok(o)
            }
            Err((id, kind)) => Err(self.fail(id, kind)),
        }
    }
}

impl<A: Readable> Readable for Option<A> {
    type Output = Option<A::Output>;

    fn read(self, batch: &mut Batch<'_>) -> Result<Self::Output, Failure> {
        match self {
            Some(item) => item.read(batch).map(Some),
            None => {
                batch.skip();
                Ok(None)
            }
        }
    }
}
impl<A: IntoRead> IntoRead for Option<A> {}

impl<A: Readable> Readable for Vec<A> {
    type Output = Vec<A::Output>;

    fn read(self, batch: &mut Batch<'_>) -> Result<Self::Output, Failure> {
        self.into_iter().map(|item| item.read(batch)).collect()
    }
}
impl<A: IntoRead> IntoRead for Vec<A> {}

impl<A: Readable, const N: usize> Readable for [A; N] {
    type Output = [A::Output; N];

    fn read(self, batch: &mut Batch<'_>) -> Result<Self::Output, Failure> {
        let mut out = Vec::with_capacity(N);
        for item in self {
            out.push(item.read(batch)?);
        }
        Ok(out
            .try_into()
            .unwrap_or_else(|_| unreachable!("N items were read")))
    }
}
impl<A: IntoRead, const N: usize> IntoRead for [A; N] {}

impl Stageable for () {
    fn stage(self, _: &mut Batch<'_>) -> Result<(), Failure> {
        Ok(())
    }
}
impl<K> IntoAssign<K> for () {}
impl IntoRead for () {}
impl Readable for () {
    type Output = ();

    fn read(self, _: &mut Batch<'_>) -> Result<(), Failure> {
        Ok(())
    }
}

macro_rules! tuples {
    ($(($($name:ident $at:tt),+))+) => {$(
        impl<$($name: Stageable),+> Stageable for ($($name,)+) {
            fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
                $(self.$at.stage(batch)?;)+
                Ok(())
            }
        }
        impl<K, $($name: IntoAssign<K>),+> IntoAssign<K> for ($($name,)+) {}
        impl<$($name: IntoRead),+> IntoRead for ($($name,)+) {}
        impl<$($name: Readable),+> Readable for ($($name,)+) {
            type Output = ($($name::Output,)+);

            fn read(self, batch: &mut Batch<'_>) -> Result<Self::Output, Failure> {
                Ok(($(self.$at.read(batch)?,)+))
            }
        }
    )+};
}

tuples! {
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
    (A 0, B 1, C 2, D 3, E 4)
    (A 0, B 1, C 2, D 3, E 4, F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K10 10)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K10 10, L 11)
}
