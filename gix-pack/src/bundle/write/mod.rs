use std::{
    io,
    io::{Seek, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gix_features::{interrupt, progress, progress::Progress};
use gix_tempfile::{AutoRemove, ContainingDirectory};

use crate::data;

mod error;
pub use error::Error;
use gix_features::progress::prodash::DynNestedProgress;

mod limited_file;
use limited_file::LimitedFile;
mod types;
use types::PassThrough;
pub use types::{Options, Outcome};

use crate::bundle::write::types::SharedTempFile;

/// The progress ids used in [`write_to_directory()`][crate::Bundle::write_to_directory()].
///
/// Use this information to selectively extract the progress of interest in case the parent application has custom visualization.
#[derive(Debug, Copy, Clone)]
pub enum ProgressId {
    /// The amount of bytes read from the input pack data file.
    ReadPackBytes,
    /// A root progress counting logical steps towards an index file on disk.
    ///
    /// Underneath will be more progress information related to actually producing the index.
    IndexingSteps(PhantomData<crate::index::write::ProgressId>),
}

#[expect(clippy::too_many_arguments)]
fn prepare_complete_and_write<Find: gix_object::Find>(
    options: Options,
    data_file: SharedTempFile,
    entries: &mut dyn Iterator<Item = Result<data::input::Entry, data::input::Error>>,
    progress: &mut dyn DynNestedProgress,
    index_out: &mut dyn io::Write,
    should_interrupt: &AtomicBool,
    pack_version: data::Version,
    lookup: Option<&Find>,
) -> Result<crate::index::write::Outcome, Error> {
    let Options {
        thread_limit,
        index_version,
        object_hash,
        alloc_limit_bytes,
        ..
    } = options;
    if index_version != crate::index::Version::default() {
        return Err(crate::index::write::Error::Unsupported(index_version).into());
    }
    let start = std::time::Instant::now();
    let mut prepared = crate::index::prepare_data_iter(
        {
            let data_file = Arc::clone(&data_file);
            move || new_pack_file_resolver(data_file)
        },
        entries,
        thread_limit,
        progress,
        should_interrupt,
        object_hash,
        alloc_limit_bytes,
        pack_version,
        lookup.map(|find| find as &dyn gix_object::Find),
    )?;
    let appended = !prepared.missing_bases.is_empty();
    let pack_hash = complete_pack(
        &mut prepared,
        &data_file,
        lookup,
        pack_version,
        &options,
        should_interrupt,
    )?;
    if appended {
        prepared.items.sort_by_key(|entry| entry.data.id);
    }
    if let Some(duplicate) = prepared
        .items
        .windows(2)
        .find(|pair| pair[0].data.id == pair[1].data.id)
    {
        return Err(crate::index::write::Error::DuplicateObject {
            object_id: duplicate[0].data.id,
        }
        .into());
    }
    let index_hash = crate::index::encode::write_to(
        index_out,
        prepared.items,
        &pack_hash,
        index_version,
        object_hash,
        &mut progress.add_child_with_id(
            "writing index file".into(),
            crate::index::write::ProgressId::IndexBytesWritten.into(),
        ),
    )
    .map_err(crate::index::write::Error::Io)?;
    progress.show_throughput_with(
        start,
        prepared.num_objects as usize,
        progress::count("objects").expect("unit always set"),
        progress::MessageLevel::Success,
    );
    Ok(crate::index::write::Outcome {
        index_version,
        index_hash,
        data_hash: pack_hash,
        num_objects: prepared.num_objects,
    })
}

fn complete_pack<Find: gix_object::Find>(
    prepared: &mut crate::index::Prepared,
    data_file: &SharedTempFile,
    lookup: Option<&Find>,
    pack_version: data::Version,
    options: &Options,
    should_interrupt: &AtomicBool,
) -> Result<gix_hash::ObjectId, Error> {
    if should_interrupt.load(Ordering::Relaxed) {
        return Err(
            crate::index::write::Error::TreeTraversal(crate::cache::delta::traverse::Error::Interrupted).into(),
        );
    }
    let rewrite = options.iteration_mode == data::input::Mode::Restore || !prepared.missing_bases.is_empty();
    let mut writer = data_file.lock();
    writer.flush()?;
    if !rewrite {
        let pack_hash = prepared
            .pack_hash
            .ok_or(crate::index::write::Error::IteratorInvariantTrailer)?;
        let end = prepared
            .entries_end
            .checked_add(options.object_hash.len_in_bytes() as u64)
            .ok_or_else(|| io::Error::other("pack length overflowed"))?;
        writer.get_mut().truncate_and_seek(end)?;
        return Ok(pack_hash);
    }

    let final_count = (prepared.num_objects as usize)
        .checked_add(prepared.missing_bases.len())
        .ok_or(crate::index::write::Error::IteratorInvariantTooManyObjects(usize::MAX))?;
    let final_count_u32: u32 = final_count
        .try_into()
        .map_err(|_| crate::index::write::Error::IteratorInvariantTooManyObjects(final_count))?;
    #[cfg(target_os = "motor")]
    if final_count > 65_536 {
        return Err(
            crate::index::write::Error::Tree(crate::cache::delta::Error::EntryCountLimit { max_entries: 65_536 })
                .into(),
        );
    }
    prepared
        .items
        .try_reserve_exact(prepared.missing_bases.len())
        .map_err(crate::index::write::Error::from)?;
    writer.get_mut().truncate_and_seek(prepared.entries_end)?;

    #[cfg(target_os = "motor")]
    let alloc_limit_bytes = Some(
        options
            .alloc_limit_bytes
            .unwrap_or(16 * 1024 * 1024)
            .min(16 * 1024 * 1024),
    );
    #[cfg(not(target_os = "motor"))]
    let alloc_limit_bytes = options.alloc_limit_bytes;
    let mut lookup_buf = Vec::new();
    for object_id in std::mem::take(&mut prepared.missing_bases) {
        if should_interrupt.load(Ordering::Relaxed) {
            return Err(
                crate::index::write::Error::TreeTraversal(crate::cache::delta::traverse::Error::Interrupted).into(),
            );
        }
        let lookup = lookup.ok_or(Error::MissingExternalBase { object_id })?;
        let object = lookup
            .try_find(&object_id, &mut lookup_buf)
            .map_err(|source| data::input::Error::Find { object_id, source })?
            .ok_or(Error::MissingExternalBase { object_id })?;
        if alloc_limit_bytes.is_some_and(|limit| object.data.len() > limit) {
            return Err(
                crate::index::write::Error::TreeTraversal(crate::cache::delta::traverse::Error::OutOfMemory).into(),
            );
        }
        let actual = gix_object::compute_hash(options.object_hash, object.kind, object.data)
            .map_err(crate::cache::delta::traverse::Error::ObjectHash)
            .map_err(crate::index::write::Error::TreeTraversal)?;
        if actual != object_id {
            return Err(Error::ExternalBaseIdMismatch {
                expected: object_id,
                actual,
            });
        }
        let entry = data::input::Entry::from_data_obj(&object, prepared.entries_end, options.compression)?;
        let entry_len = entry.bytes_in_pack();
        entry.header.write_to(entry.decompressed_size, &mut *writer)?;
        writer.write_all(
            entry
                .compressed
                .as_deref()
                .ok_or_else(|| io::Error::other("constructed base entry has no compressed data"))?,
        )?;
        prepared.items.push(crate::cache::delta::tree::Item::detached(
            prepared.entries_end,
            crate::index::write::TreeEntry {
                id: object_id,
                crc32: entry
                    .crc32
                    .ok_or_else(|| io::Error::other("constructed base entry has no CRC32"))?,
            },
        ));
        prepared.entries_end = prepared
            .entries_end
            .checked_add(entry_len)
            .ok_or_else(|| io::Error::other("pack length overflowed"))?;
    }
    prepared.num_objects = final_count_u32;

    writer.flush()?;
    let file = writer.get_mut();
    file.rewind()?;
    file.write_all(&data::header::encode(pack_version, final_count_u32))?;
    file.flush()?;
    file.rewind()?;
    let pack_hash = gix_hash::bytes(
        file,
        prepared.entries_end,
        options.object_hash,
        &mut progress::Discard,
        should_interrupt,
    )
    .map_err(crate::index::write::Error::Io)?;
    file.write_all(pack_hash.as_slice())?;
    file.flush()?;
    prepared.pack_hash = Some(pack_hash);
    Ok(pack_hash)
}

impl From<ProgressId> for gix_features::progress::Id {
    fn from(v: ProgressId) -> Self {
        match v {
            ProgressId::ReadPackBytes => *b"BWRB",
            ProgressId::IndexingSteps(_) => *b"BWCI",
        }
    }
}

impl crate::Bundle {
    /// Given a `pack` data stream, write it along with a generated index into the `directory` if `Some` or discard all output if `None`.
    ///
    /// In the latter case, the functionality provided here is more a kind of pack data stream validation.
    ///
    /// * `progress` provides detailed progress information which can be discarded with [`gix_features::progress::Discard`].
    /// * `should_interrupt` is checked regularly and when true, the whole operation will stop.
    /// * `thin_pack_base_object_lookup` supplies missing `REF_DELTA` bases. Bases already present in the incoming pack are
    ///   preserved and aren't added again. `options` further configure how the task is performed.
    ///
    /// # Note
    ///
    /// * the resulting pack may be empty, that is, contains zero objects in some situations. This is a valid reply by a server and should
    ///   be accounted for.
    ///   - Empty packs always have the same name and not handling this case will result in at most one superfluous pack.
    pub fn write_to_directory(
        pack: &mut dyn io::BufRead,
        directory: Option<&Path>,
        progress: &mut dyn DynNestedProgress,
        should_interrupt: &AtomicBool,
        thin_pack_base_object_lookup: Option<impl gix_object::Find>,
        options: Options,
    ) -> Result<Outcome, Error> {
        let _span = gix_features::trace::coarse!("gix_pack::Bundle::write_to_directory()");
        let mut read_progress = progress.add_child_with_id("read pack".into(), ProgressId::ReadPackBytes.into());
        read_progress.init(None, progress::bytes());
        let pack = progress::Read {
            inner: pack,
            progress: progress::ThroughputOnDrop::new(read_progress),
        };

        let object_hash = options.object_hash;
        let data_file = Arc::new(parking_lot::Mutex::new(io::BufWriter::with_capacity(
            64 * 1024,
            LimitedFile::for_target(match directory.as_ref() {
                Some(directory) => gix_tempfile::new(directory, ContainingDirectory::Exists, AutoRemove::Tempfile)?,
                None => gix_tempfile::new(std::env::temp_dir(), ContainingDirectory::Exists, AutoRemove::Tempfile)?,
            }),
        )));
        let pack = PassThrough {
            reader: interrupt::Read {
                inner: pack,
                should_interrupt,
            },
            writer: Some(data_file.clone()),
        };
        let buffered_pack = io::BufReader::new(pack);
        let pack_entries_iter = data::input::BytesToEntriesIter::new_from_header(
            buffered_pack,
            options.iteration_mode,
            data::input::EntryDataMode::Crc32,
            object_hash,
        )?;
        let pack_version = pack_entries_iter.version();
        let WriteOutcome {
            outcome,
            data_path,
            index_path,
            keep_path,
        } = crate::Bundle::inner_write(
            directory,
            progress,
            options,
            data_file,
            Box::new(pack_entries_iter),
            should_interrupt,
            pack_version,
            thin_pack_base_object_lookup,
        )?;

        Ok(Outcome {
            index: outcome,
            object_hash,
            pack_version,
            data_path,
            index_path,
            keep_path,
        })
    }

    /// Equivalent to [`write_to_directory()`][crate::Bundle::write_to_directory()] but offloads reading of the pack into its own thread, hence the `Send + 'static'` bounds.
    ///
    /// # Note
    ///
    /// As it sends portions of the input to a thread it requires the 'static lifetime for the interrupt flags. This can only
    /// be satisfied by a static `AtomicBool` which is only suitable for programs that only run one of these operations at a time
    /// or don't mind that all of them abort when the flag is set.
    pub fn write_to_directory_eagerly(
        pack: Box<dyn io::Read + Send + 'static>,
        pack_size: Option<u64>,
        directory: Option<impl AsRef<Path>>,
        progress: &mut dyn DynNestedProgress,
        should_interrupt: &'static AtomicBool,
        thin_pack_base_object_lookup: Option<impl gix_object::Find + Send + 'static>,
        options: Options,
    ) -> Result<Outcome, Error> {
        let _span = gix_features::trace::coarse!("gix_pack::Bundle::write_to_directory_eagerly()");
        let mut read_progress = progress.add_child_with_id("read pack".into(), ProgressId::ReadPackBytes.into()); /* Bundle Write Read pack Bytes*/
        read_progress.init(pack_size.map(|s| s as usize), progress::bytes());
        let pack = progress::Read {
            inner: pack,
            progress: progress::ThroughputOnDrop::new(read_progress),
        };

        let data_file = Arc::new(parking_lot::Mutex::new(io::BufWriter::new(LimitedFile::for_target(
            match directory.as_ref() {
                Some(directory) => gix_tempfile::new(directory, ContainingDirectory::Exists, AutoRemove::Tempfile)?,
                None => gix_tempfile::new(std::env::temp_dir(), ContainingDirectory::Exists, AutoRemove::Tempfile)?,
            },
        ))));
        let object_hash = options.object_hash;
        let eight_pages = 4096 * 8;
        let pack = PassThrough {
            reader: interrupt::Read {
                inner: pack,
                should_interrupt,
            },
            writer: Some(data_file.clone()),
        };
        let buffered_pack = io::BufReader::with_capacity(eight_pages, pack);
        let pack_entries_iter = data::input::BytesToEntriesIter::new_from_header(
            buffered_pack,
            options.iteration_mode,
            data::input::EntryDataMode::Crc32,
            object_hash,
        )?;
        let pack_version = pack_entries_iter.version();
        let num_objects = pack_entries_iter.size_hint().0;
        let pack_entries_iter =
            gix_features::parallel::EagerIterIf::new(move || num_objects > 25_000, pack_entries_iter, 5_000, 5);

        let WriteOutcome {
            outcome,
            data_path,
            index_path,
            keep_path,
        } = crate::Bundle::inner_write(
            directory,
            progress,
            options,
            data_file,
            Box::new(pack_entries_iter),
            should_interrupt,
            pack_version,
            thin_pack_base_object_lookup,
        )?;

        Ok(Outcome {
            index: outcome,
            object_hash,
            pack_version,
            data_path,
            index_path,
            keep_path,
        })
    }

    #[expect(clippy::too_many_arguments)]
    fn inner_write<'a, Find>(
        directory: Option<impl AsRef<Path>>,
        progress: &mut dyn DynNestedProgress,
        options: Options,
        data_file: SharedTempFile,
        mut pack_entries_iter: Box<dyn Iterator<Item = Result<data::input::Entry, data::input::Error>> + 'a>,
        should_interrupt: &AtomicBool,
        pack_version: data::Version,
        thin_pack_base_object_lookup: Option<Find>,
    ) -> Result<WriteOutcome, Error>
    where
        Find: gix_object::Find,
    {
        let mut indexing_progress = progress.add_child_with_id(
            "create index file".into(),
            ProgressId::IndexingSteps(Default::default()).into(),
        );
        Ok(match directory {
            Some(directory) => {
                let directory = directory.as_ref();
                let mut index_file = gix_tempfile::new(directory, ContainingDirectory::Exists, AutoRemove::Tempfile)?;

                let outcome = prepare_complete_and_write(
                    options,
                    Arc::clone(&data_file),
                    &mut pack_entries_iter,
                    &mut indexing_progress,
                    &mut index_file,
                    should_interrupt,
                    pack_version,
                    thin_pack_base_object_lookup.as_ref(),
                )?;
                drop(pack_entries_iter);

                if outcome.num_objects == 0 {
                    WriteOutcome {
                        outcome,
                        data_path: None,
                        index_path: None,
                        keep_path: None,
                    }
                } else {
                    let data_path = directory.join(format!("pack-{}.pack", outcome.data_hash.to_hex()));
                    let index_path = data_path.with_extension("idx");
                    let keep_path = if data_path.is_file() {
                        // avoid trying to overwrite existing files, we know they have the same content
                        // and this is likely to fail on Windows as negotiation opened the pack.
                        None
                    } else {
                        let keep_path = data_path.with_extension("keep");

                        std::fs::write(&keep_path, b"")?;
                        Arc::try_unwrap(data_file)
                            .expect("only one handle left after pack was consumed")
                            .into_inner()
                            .into_inner()
                            .map_err(|err| Error::from(err.into_error()))?
                            .into_inner()
                            .persist(&data_path)?;
                        Some(keep_path)
                    };
                    if !index_path.is_file() {
                        index_file
                            .persist(&index_path)
                            .inspect_err(|_err| {
                                gix_features::trace::warn!("pack file at \"{}\" is retained despite failing to move the index file into place. You can use plumbing to make it usable.",data_path.display());
                            })?;
                    }
                    WriteOutcome {
                        outcome,
                        data_path: Some(data_path),
                        index_path: Some(index_path),
                        keep_path,
                    }
                }
            }
            None => WriteOutcome {
                outcome: prepare_complete_and_write(
                    options,
                    data_file,
                    &mut pack_entries_iter,
                    &mut indexing_progress,
                    &mut io::sink(),
                    should_interrupt,
                    pack_version,
                    thin_pack_base_object_lookup.as_ref(),
                )?,
                data_path: None,
                index_path: None,
                keep_path: None,
            },
        })
    }
}

fn resolve_entry(range: data::EntryRange, mapped_file: &crate::MMap) -> Option<&[u8]> {
    mapped_file.get(range.start as usize..range.end as usize)
}

#[expect(clippy::type_complexity)] // cannot typedef impl Fn
fn new_pack_file_resolver(
    data_file: SharedTempFile,
) -> io::Result<(
    impl Fn(data::EntryRange, &crate::MMap) -> Option<&[u8]> + Send + Clone,
    crate::MMap,
)> {
    let mut guard = data_file.lock();
    guard.flush()?;
    let mapped_file = crate::mmap::read_only(&guard.get_mut().inner_mut().with_mut(|f| f.path().to_owned())?)?;
    Ok((resolve_entry, mapped_file))
}

struct WriteOutcome {
    outcome: crate::index::write::Outcome,
    data_path: Option<PathBuf>,
    index_path: Option<PathBuf>,
    keep_path: Option<PathBuf>,
}
