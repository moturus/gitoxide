use std::{collections::TryReserveError, sync::atomic::AtomicBool};

use gix_features::{
    progress::{self, DynNestedProgress, Progress},
    threading,
    threading::{Mutable, OwnShared},
};

use crate::{
    cache::delta::{Tree, traverse::util::ItemSliceSync, tree::Item},
    data::EntryRange,
};

mod resolve;
pub(crate) mod util;

/// Shared access to ref-delta child indices awaiting a resolved base, keyed by its object ID.
pub(super) type SharedRefDeltaChildren = OwnShared<Mutable<super::tree::RefDeltaChildren>>;

/// Returned by [`Tree::traverse()`]
#[derive(thiserror::Error, Debug)]
#[allow(missing_docs)]
pub enum Error {
    #[error("{message}")]
    ZlibInflate {
        source: gix_zlib::inflate::Error,
        message: &'static str,
    },
    #[error("The resolver failed to obtain the pack entry bytes for the entry at {pack_offset}")]
    ResolveFailed { pack_offset: u64 },
    #[error(transparent)]
    EntryType(#[from] crate::data::entry::decode::Error),
    #[error("One of the object inspectors failed")]
    Inspect(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("Interrupted")]
    Interrupted,
    #[error("Entry too large to fit in memory")]
    OutOfMemory,
    #[error("Delta traversal exceeds the cumulative allocation allowance")]
    AggregateAllocationLimit,
    #[error("Delta depth exceeds the supported range")]
    DeltaDepthOverflow,
    #[error(
        "The base at {base_pack_offset} was referred to by a ref-delta, but it was never added to the tree as if the pack was still thin."
    )]
    OutOfPackRefDelta {
        /// The base's offset which was from a resolved ref-delta that didn't actually get added to the tree
        base_pack_offset: crate::data::Offset,
    },
    #[error("The ref-delta base object {base_id} could not be found")]
    UnresolvedRefDelta {
        /// The id named by one or more unresolved ref-delta entries.
        base_id: gix_hash::ObjectId,
    },
    #[error("Failed to look up ref-delta base object {base_id}")]
    ExternalBaseLookup {
        base_id: gix_hash::ObjectId,
        source: gix_object::find::Error,
    },
    #[error("External ref-delta base {expected} yielded object {actual}")]
    ExternalBaseIdMismatch {
        expected: gix_hash::ObjectId,
        actual: gix_hash::ObjectId,
    },
    #[error("Failed to hash an object while resolving in-pack ref-deltas")]
    ObjectHash(#[from] gix_hash::hasher::Error),
    #[error("Failed to spawn thread when switching to work-stealing mode")]
    SpawnThread(#[from] std::io::Error),
    #[error(transparent)]
    Delta(#[from] crate::data::delta::apply::Error),
}

impl From<TryReserveError> for Error {
    #[cold]
    fn from(_: TryReserveError) -> Self {
        Self::OutOfMemory
    }
}

/// Additional context passed to the `inspect_object(…)` function of the [`Tree::traverse()`] method.
pub struct Context<'a> {
    /// The pack entry describing the object
    pub entry: &'a crate::data::Entry,
    /// The offset at which `entry` ends in the pack, useful to learn about the exact range of `entry` within the pack.
    pub entry_end: u64,
    /// The decompressed object itself, ready to be decoded.
    pub decompressed: &'a [u8],
    /// The depth at which this object resides in the delta-tree. It represents the number of base objects, with 0 indicating
    /// an 'undeltified' object, and higher values indicating delta objects with the given number of bases.
    pub level: u16,
}

/// Options for [`Tree::traverse()`].
pub struct Options<'a, 's> {
    /// is a progress instance to track progress for each object in the traversal.
    pub object_progress: Box<dyn DynNestedProgress>,
    /// is a progress instance to track the overall progress.
    pub size_progress: &'s mut dyn Progress,
    /// If `Some`, only use the given number of threads. Otherwise, the number of threads to use will be selected based on
    /// the number of available logical cores.
    pub thread_limit: Option<usize>,
    /// Abort the operation if the value is `true`.
    pub should_interrupt: &'a AtomicBool,
    /// specifies what kind of hashes we expect to be stored in oid-delta entries, which is viable to decoding them
    /// with the correct size.
    pub object_hash: gix_hash::Kind,
    /// If `Some`, rejects individual allocations above the given number of bytes while resolving decoded object and
    /// delta result buffers. `Some(0)` rejects all non-empty allocations.
    pub alloc_limit_bytes: Option<usize>,
}

/// The outcome of [`Tree::traverse()`]
pub struct Outcome<T> {
    /// The items that have no children in the pack, i.e. base objects.
    pub roots: Vec<Item<T>>,
    /// The items that children to a root object, i.e. delta objects.
    pub children: Vec<Item<T>>,
}

impl<T> Tree<T>
where
    T: Send,
{
    /// Traverse this tree of delta objects with a function `inspect_object` to process each object at will.
    ///
    /// * `should_run_in_parallel() -> bool` returns true if the underlying pack is big enough to warrant parallel traversal at all.
    /// * `resolve(EntrySlice, &mut Vec<u8>) -> Option<()>` resolves the bytes in the pack for the given `EntrySlice` and stores them in the
    ///   output vector. It returns `Some(())` if the object existed in the pack, or `None` to indicate a resolution error, which would abort the
    ///   operation as well.
    /// * `pack_entries_end` marks one-past-the-last byte of the last entry in the pack, as the last entries size would otherwise
    ///   be unknown as it's not part of the index file.
    /// * `inspect_object(node_data: &mut T, progress: Progress, context: Context<ThreadLocal State>) -> Result<(), CustomError>` is a function
    ///   running for each thread receiving fully decoded objects along with contextual information, which either succeeds with `Ok(())`
    ///   or returns a `CustomError`.
    ///   Note that `node_data` can be modified to allow storing maintaining computation results on a per-object basis. It should contain
    ///   its own mutable per-thread data as required.
    ///
    /// This method returns a vector of all tree items, along with their potentially modified custom node data.
    ///
    /// _Note_ that this method consumed the Tree to assure safe parallel traversal with mutation support.
    pub fn traverse<F, MBFN, E, R>(
        self,
        resolve: F,
        resolve_data: &R,
        pack_entries_end: u64,
        inspect_object: MBFN,
        options: Options<'_, '_>,
    ) -> Result<Outcome<T>, Error>
    where
        F: for<'r> Fn(EntryRange, &'r R) -> Option<&'r [u8]> + Send + Clone,
        R: Send + Sync,
        MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E> + Send + Clone,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.traverse_with_external_bases(resolve, resolve_data, pack_entries_end, inspect_object, None, options)
            .map(|(outcome, _external_bases)| outcome)
    }

    pub(crate) fn traverse_with_external_bases<F, MBFN, E, R>(
        mut self,
        resolve: F,
        resolve_data: &R,
        pack_entries_end: u64,
        inspect_object: MBFN,
        lookup: Option<&dyn gix_object::Find>,
        Options {
            thread_limit,
            mut object_progress,
            size_progress,
            should_interrupt,
            object_hash,
            alloc_limit_bytes,
        }: Options<'_, '_>,
    ) -> Result<(Outcome<T>, Vec<gix_hash::ObjectId>), Error>
    where
        F: for<'r> Fn(EntryRange, &'r R) -> Option<&'r [u8]> + Send + Clone,
        R: Send + Sync,
        MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E> + Send + Clone,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.set_pack_entries_end_and_resolve_ref_offsets(pack_entries_end)?;

        let num_objects = self.num_items();
        let object_counter = {
            let progress = &mut object_progress;
            progress.init(Some(num_objects), progress::count("objects"));
            progress.counter()
        };
        size_progress.init(None, progress::bytes());
        let size_counter = size_progress.counter();
        let resolver_progress = object_progress.add_child("delta resolver".into());

        let start = std::time::Instant::now();
        let (mut root_items, mut child_items_vec, ref_delta_children) = self.take_root_child_and_refs();
        let ref_delta_children =
            (!ref_delta_children.is_empty()).then(|| OwnShared::new(Mutable::new(ref_delta_children)));
        let child_items = ItemSliceSync::new(&mut child_items_vec);
        let allocation_budget = resolve::AllocationBudget::for_target(alloc_limit_bytes);
        // SAFETY: Both item slices come from the same Tree, whose child-index uniqueness invariant still holds.
        #[expect(unsafe_code)]
        unsafe {
            resolve::all(
                &mut root_items,
                &child_items,
                thread_limit,
                num_objects,
                object_counter.clone(),
                size_counter.clone(),
                &resolver_progress,
                resolve.clone(),
                resolve_data,
                inspect_object.clone(),
                ref_delta_children.clone(),
                object_hash,
                &allocation_budget,
                should_interrupt,
            )?;
        }

        let mut external_bases = Vec::new();
        if let (Some(lookup), Some(ref_delta_children)) = (lookup, ref_delta_children.as_ref()) {
            let pending = {
                let pending = threading::lock(ref_delta_children);
                let mut ids = Vec::new();
                ids.try_reserve_exact(pending.len())?;
                ids.extend(pending.keys().copied());
                ids
            };
            external_bases.try_reserve_exact(pending.len())?;
            let mut lookup_buf = Vec::new();
            for base_id in pending {
                if should_interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(Error::Interrupted);
                }
                if !threading::lock(ref_delta_children).contains_key(&base_id) {
                    continue;
                }
                let Some(data) = lookup
                    .try_find(&base_id, &mut lookup_buf)
                    .map_err(|source| Error::ExternalBaseLookup { base_id, source })?
                else {
                    continue;
                };
                let actual = gix_object::compute_hash(object_hash, data.kind, data.data)?;
                if actual != base_id {
                    return Err(Error::ExternalBaseIdMismatch {
                        expected: base_id,
                        actual,
                    });
                }
                let bytes = allocation_budget.copy(data.data)?;
                let children = threading::lock(ref_delta_children)
                    .remove(&base_id)
                    .ok_or(Error::UnresolvedRefDelta { base_id })?;
                // SAFETY: Removed child indices have one parent and can't be processed again.
                #[expect(unsafe_code)]
                unsafe {
                    resolve::from_external(
                        children,
                        &child_items,
                        data.kind,
                        bytes,
                        object_counter.clone(),
                        size_counter.clone(),
                        &resolver_progress,
                        resolve.clone(),
                        resolve_data,
                        inspect_object.clone(),
                        Some(ref_delta_children.clone()),
                        object_hash,
                        &allocation_budget,
                        should_interrupt,
                    )?;
                }
                external_bases.push(base_id);
            }
        }

        if let Some(ref_delta_children) = ref_delta_children {
            if let Some((base_id, _children)) = threading::lock(&ref_delta_children).first_key_value() {
                return Err(Error::UnresolvedRefDelta { base_id: *base_id });
            }
        }

        object_progress.show_throughput(start);
        size_progress.show_throughput(start);

        Ok((
            Outcome {
                roots: root_items,
                children: child_items_vec,
            },
            external_bases,
        ))
    }
}
