use arc_swap::access::Access;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::{
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
};

use crate::{
    general::{handle, store, store::StateId},
    RefreshMode,
};

pub(crate) enum Outcome {
    /// Drop all data and fully replace it with `indices`.
    /// This happens if we have witnessed a generational change invalidating all of our ids and causing currently loaded
    /// indices and maps to be dropped.
    Replace(Snapshot),
    /// Despite all values being full copies, indices are still compatible to what was before. This also means
    /// the caller can continue searching the added indices and loose-dbs.
    /// Or in other words, new indices were only added to the known list, and what was seen before is known not to have changed.
    /// Besides that, the full internal state can be replaced as with `Replace`.
    ReplaceStable(Snapshot),
}

pub(crate) struct Snapshot {
    /// Indices ready for object lookup or contains checks, ordered usually by modification data, recent ones first.
    pub(crate) indices: Vec<handle::IndexLookup>,
    /// A set of loose objects dbs to search once packed objects weren't found.
    pub(crate) loose_dbs: Arc<Vec<crate::loose::Store>>,
    /// remember what this state represents and to compare to other states.
    pub(crate) marker: store::SlotIndexMarker,
}

mod error {
    use crate::pack;
    use std::path::PathBuf;

    /// Returned by [`compound::Store::at()`]
    #[derive(thiserror::Error, Debug)]
    #[allow(missing_docs)]
    pub enum Error {
        #[error("The objects directory at '{0}' is not an accessible directory")]
        Inaccessible(PathBuf),
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Alternate(#[from] crate::alternate::Error),
    }
}

use crate::general::store::{IndexAndPacks, MultiIndexFileBundle, MutableIndexAndPack, OnDiskFile, OnDiskFileState};
pub use error::Error;

impl super::Store {
    /// If `None` is returned, there is new indices and the caller should give up. This is a possibility even if it's allowed to refresh
    /// as here might be no change to pick up.
    pub(crate) fn load_one_index(
        &self,
        refresh_mode: RefreshMode,
        marker: &store::SlotIndexMarker,
    ) -> Result<Option<Outcome>, Error> {
        let index = self.index.load();
        let state_id = index.state_id();
        if !index.is_initialized() {
            // TODO: figure out what kind of refreshes we need. This one loads in the initial slot map, but I think this cost is paid
            //       in full during instantiation.
            return self.consolidate_with_disk_state();
        }

        let outcome = {
            if marker.generation != index.generation {
                self.collect_replace_outcome(false /*stable*/)
            } else if marker.state_id == index.state_id() {
                // always compare to the latest state
                // Nothing changed in the mean time, try to load another index…

                // …and if that didn't yield anything new consider refreshing our disk state.
                match refresh_mode {
                    RefreshMode::Never => return Ok(None),
                    RefreshMode::AfterAllIndicesLoaded => return self.consolidate_with_disk_state(),
                }
            } else {
                self.collect_replace_outcome(true /*stable*/)
            }
        };
        Ok(Some(outcome))
    }

    /// refresh and possibly clear out our existing data structures, causing all pack ids to be invalidated.
    fn consolidate_with_disk_state(&self) -> Result<Option<Outcome>, Error> {
        let index = self.index.load();
        let index_state = Arc::as_ptr(&index) as usize;
        let previous_generation = index.generation;

        // IMPORTANT: get a lock after we recorded the previous state.
        let objects_directory = self.path.lock();

        // Now we know the index isn't going to change anymore, even though threads might still load indices in the meantime.
        let index = self.index.load();
        if index_state != Arc::as_ptr(&index) as usize {
            // Someone else took the look before and changed the index. Return it without doing any additional work.
            return Ok(Some(
                self.collect_replace_outcome(index.generation == previous_generation),
            ));
        }

        let was_uninitialized = !index.is_initialized();
        let needs_stable_indices = self.maintain_stable_indices(&objects_directory);

        self.num_disk_state_consolidation.fetch_add(1, Ordering::Relaxed);
        let db_paths: Vec<_> = std::iter::once(objects_directory.clone())
            .chain(crate::alternate::resolve(&*objects_directory)?)
            .collect();

        // turn db paths into loose object databases. Reuse what's there, but only if it is in the right order.
        let loose_dbs = if was_uninitialized
            || db_paths.len() != index.loose_dbs.len()
            || db_paths
                .iter()
                .zip(index.loose_dbs.iter().map(|ldb| &ldb.path))
                .any(|(lhs, rhs)| lhs != rhs)
        {
            Arc::new(db_paths.iter().map(|p| crate::loose::Store::at(p)).collect::<Vec<_>>())
        } else {
            Arc::clone(&index.loose_dbs)
        };

        // Outside of this method we will never assign new slot indices.
        let mut indices_by_modification_time = Vec::with_capacity(index.slot_indices.len());
        for db_path in db_paths {
            let packs = db_path.join("pack");
            let entries = match std::fs::read_dir(&packs) {
                Ok(e) => e,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            indices_by_modification_time.extend(
                entries
                    .filter_map(Result::ok)
                    .filter_map(|e| e.metadata().map(|md| (e.path(), md)).ok())
                    .filter(|(_, md)| md.file_type().is_file())
                    .filter(|(p, _)| {
                        let ext = p.extension();
                        ext == Some(OsStr::new("idx"))
                            || (ext.is_none() && p.file_name() == Some(OsStr::new("multi-pack-index")))
                    })
                    .map(|(p, md)| md.modified().map_err(Error::from).map(|mod_time| (p, mod_time)))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        // Like libgit2, sort by modification date, newest first, to serve as good starting point.
        // Git itself doesn't change the order which may safe time, and relies on a LRU sorting on lookup later.
        // We can do that to in the handle.
        indices_by_modification_time.sort_by(|l, r| l.1.cmp(&r.1).reverse());
        let mut idx_by_index_path: BTreeMap<_, _> = index
            .slot_indices
            .iter()
            .filter_map(|&idx| {
                let f = &self.files[idx];
                Option::as_ref(&f.files.load()).map(|f| (f.index_path().to_owned(), idx))
            })
            .collect();

        let mut existing_slot_map_indices = Vec::new(); // these indices into the slot map still exist there/didn't change
        let mut index_paths_to_add = was_uninitialized
            .then(|| Vec::with_capacity(indices_by_modification_time.len()))
            .unwrap_or_default();
        for index_path in indices_by_modification_time.into_iter().map(|(p, _mtime)| p) {
            match idx_by_index_path.remove(&index_path) {
                Some(slot_idx) => {
                    let f = &self.files[slot_idx];
                    Self::assure_slot_for_path(&objects_directory, f, index_path, false /*allow init*/)?;
                    existing_slot_map_indices.push(slot_idx);
                }
                None => index_paths_to_add.push(index_path),
            }
        }

        let (min_slot_index, max_slot_index) = (index.slot_indices.iter().min(), index.slot_indices.iter().max());

        // deleted items - remove their slots AFTER we have set the new index if we may alter indices, otherwise we only declare them garbage.
        // removing slots may cause pack loading to fail, and they will then reload their indices.
        for (index_path, slot_idx) in idx_by_index_path {}

        todo!("consolidate")
    }

    fn assure_slot_for_path(
        lock: &parking_lot::MutexGuard<'_, PathBuf>,
        f: &MutableIndexAndPack,
        index_path: PathBuf,
        may_init: bool,
    ) -> Result<(), Error> {
        match Option::as_ref(&f.files.load()) {
            Some(files) => {
                assert_eq!(
                    files.index_path(),
                    index_path,
                    "Parallel writers cannot change the file the slot points to."
                );
            }
            None => {
                if may_init {
                    let _lock = f.write.lock();
                    let mut files = f.files.load_full();
                    let files_mut = Arc::make_mut(&mut files);
                    assert!(
                        files_mut.is_none(),
                        "BUG: There must be no race between us checking and obtaining a lock."
                    );
                    *files_mut = if index_path.extension() == Some(OsStr::new("idx")) {
                        IndexAndPacks::new_single(index_path)
                    } else {
                        IndexAndPacks::new_multi(index_path)
                    }
                    .into();
                    f.files.store(files);
                } else {
                    unreachable!("BUG: a slot can never be deleted if we have it recorded in the index WHILE changing said index. There shouldn't be a race")
                }
            }
        }
        Ok(())
    }

    /// Stability means that indices returned by this API will remain valid.
    /// Without that constraint, we may unload unused packs and indices, and may rebuild the slotmap index.
    ///
    /// Note that this must be called with a lock to the relevant state held to assure these values don't change while
    /// we are working on said index.
    fn maintain_stable_indices(&self, _guard: &parking_lot::MutexGuard<'_, PathBuf>) -> bool {
        self.num_handles_stable.load(Ordering::SeqCst) == 0
    }

    pub(crate) fn collect_snapshot(&self) -> Snapshot {
        let index = self.index.load();
        let indices = if index.is_initialized() {
            index
                .slot_indices
                .iter()
                .map(|idx| (*idx, &self.files[*idx]))
                .filter_map(|(id, file)| {
                    let lookup = match (&**file.files.load()).as_ref()? {
                        store::IndexAndPacks::Index(bundle) => handle::SingleOrMultiIndex::Single {
                            index: bundle.index.loaded()?.clone(),
                            data: bundle.data.loaded().cloned(),
                        },
                        store::IndexAndPacks::MultiIndex(multi) => handle::SingleOrMultiIndex::Multi {
                            index: multi.multi_index.loaded()?.clone(),
                            data: multi.data.iter().map(|f| f.loaded().cloned()).collect(),
                        },
                    };
                    handle::IndexLookup { file: lookup, id }.into()
                })
                .collect()
        } else {
            Vec::new()
        };

        Snapshot {
            indices,
            loose_dbs: Arc::clone(&index.loose_dbs),
            marker: index.marker(),
        }
    }

    fn collect_replace_outcome(&self, is_stable: bool) -> Outcome {
        let snapshot = self.collect_snapshot();
        if is_stable {
            Outcome::ReplaceStable(snapshot)
        } else {
            Outcome::Replace(snapshot)
        }
    }
}