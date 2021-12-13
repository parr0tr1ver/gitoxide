use std::{
    ops::BitXor,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crate::general::handle::SingleOrMultiIndex::Single;
use arc_swap::ArcSwap;
use git_features::hash;

/// An id to refer to an index file or a multipack index file
pub type IndexId = usize;
pub(crate) type StateId = u32;

/// A way to indicate which pack indices we have seen already and which of them are loaded, along with an idea
/// of whether stored `PackId`s are still usable.
#[derive(Default)]
pub struct SlotIndexMarker {
    /// The generation the `loaded_until_index` belongs to. Indices of different generations are completely incompatible.
    /// This value changes once the internal representation is compacted, something that may happen only if there is no handle
    /// requiring stable pack indices.
    pub(crate) generation: u8,
    /// A unique id identifying the index state as well as all loose databases we have last observed.
    /// If it changes in any way, the value is different.
    pub(crate) state_id: StateId,
}

/// A way to load and refer to a pack uniquely, namespaced by their indexing mechanism, aka multi-pack or not.
pub struct PackId {
    /// Note that if `multipack_index = None`, this index is corresponding to the index id.
    /// So a pack is always identified by its corresponding index.
    /// If it is a multipack index, this is the id / offset of the pack in the `multipack_index`.
    pub(crate) index: IndexId,
    pub(crate) multipack_index: Option<IndexId>,
}

/// An index that changes only if the packs directory changes and its contents is re-read.
#[derive(Default)]
pub struct SlotMapIndex {
    /// The index into the slot map at which we expect an index or pack file. Neither of these might be loaded yet.
    pub(crate) slot_indices: Vec<usize>,
    /// A list of loose object databases as resolved by their alternates file in the `object_directory`. The first entry is this objects
    /// directory loose file database. All other entries are the loose stores of alternates.
    /// It's in an Arc to be shared to Handles, but not to be shared across SlotMapIndices.
    pub(crate) loose_dbs: Arc<Vec<crate::loose::Store>>,

    /// A static value that doesn't ever change for a particular clone of this index.
    pub(crate) generation: u8,
    /// The number of indices loaded thus far when the index of the slot map was last examined, which can change as new indices are loaded
    /// in parallel.
    /// Shared across SlotMapIndex instances of the same generation.
    pub(crate) next_index_to_load: Arc<AtomicUsize>,
    /// Incremented by one up to `slot_indices.len()` once an attempt to load an index completed.
    /// If a load failed, there will also be an increment.
    /// Shared across SlotMapIndex instances of the same generation.
    pub(crate) loaded_indices: Arc<AtomicUsize>,
}

impl SlotMapIndex {
    pub(crate) fn state_id(self: &Arc<SlotMapIndex>) -> StateId {
        // We let the loaded indices take part despite not being part of our own snapshot.
        // This is to account for indices being loaded in parallel without actually changing the snapshot itself.
        let mut hash = hash::crc32(&(Arc::as_ptr(self) as usize).to_be_bytes());
        hash::crc32_update(hash, &self.loaded_indices.load(Ordering::SeqCst).to_be_bytes())
    }

    pub(crate) fn marker(self: &Arc<SlotMapIndex>) -> SlotIndexMarker {
        SlotIndexMarker {
            generation: self.generation,
            state_id: self.state_id(),
        }
    }

    /// Returns true if we already know at least one loose object db, a sign of being initialized
    pub(crate) fn is_initialized(&self) -> bool {
        !self.loose_dbs.is_empty()
    }
}

#[derive(Clone)]
pub(crate) struct OnDiskFile<T: Clone> {
    /// The last known path of the file
    path: Arc<PathBuf>,
    state: OnDiskFileState<T>,
}

#[derive(Clone)]
pub(crate) enum OnDiskFileState<T: Clone> {
    /// The file is on disk and can be loaded from there.
    Unloaded,
    Loaded(T),
    /// The file was loaded, but appeared to be missing on disk after reconciling our state with what's on disk.
    /// As there were handles that required pack-id stability we had to keep the item to allow finding it on later
    /// lookups.
    Garbage(T),
    /// File is missing on disk and could not be loaded when we tried or turned missing after reconciling our state.
    Missing,
}

impl<T: Clone> OnDiskFile<T> {
    /// Return true if we hold a memory map of the file already.
    pub fn is_loaded(&self) -> bool {
        matches!(self.state, OnDiskFileState::Loaded(_) | OnDiskFileState::Garbage(_))
    }

    pub fn loaded(&self) -> Option<&T> {
        use OnDiskFileState::*;
        match &self.state {
            Loaded(v) | Garbage(v) => Some(v),
            Unloaded | Missing => None,
        }
    }

    /// We do it like this as we first have to check for a loaded interior in read-only mode, and then upgrade
    /// when we know that loading is necessary. This also works around borrow check, which is a nice coincidence.
    pub fn do_load(&mut self, load: impl FnOnce(&Path) -> std::io::Result<T>) -> std::io::Result<Option<&T>> {
        use OnDiskFileState::*;
        match &mut self.state {
            Loaded(_) | Garbage(_) => unreachable!("BUG: check before calling this"),
            Missing => Ok(None),
            Unloaded => match load(&self.path) {
                Ok(v) => {
                    self.state = OnDiskFileState::Loaded(v);
                    match &self.state {
                        Loaded(v) => Ok(Some(v)),
                        _ => unreachable!(),
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    self.state = OnDiskFileState::Missing;
                    Ok(None)
                }
                Err(err) => Err(err),
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct IndexFileBundle {
    pub index: OnDiskFile<Arc<git_pack::index::File>>,
    pub data: OnDiskFile<Arc<git_pack::data::File>>,
}

#[derive(Clone)]
pub(crate) struct MultiIndexFileBundle {
    pub multi_index: OnDiskFile<Arc<super::handle::multi_index::File>>,
    pub data: Vec<OnDiskFile<Arc<git_pack::data::File>>>,
}

#[derive(Clone)]
pub(crate) enum IndexAndPacks {
    Index(IndexFileBundle),
    /// Note that there can only be one multi-pack file per repository, but thanks to git alternates, there can be multiple overall.
    MultiIndex(MultiIndexFileBundle),
}

impl IndexAndPacks {
    pub(crate) fn index_path(&self) -> &Path {
        match self {
            IndexAndPacks::Index(index) => &index.index.path,
            IndexAndPacks::MultiIndex(index) => &index.multi_index.path,
        }
    }

    pub(crate) fn new_multi(index_path: PathBuf) -> Self {
        Self::MultiIndex(MultiIndexFileBundle {
            multi_index: OnDiskFile {
                path: Arc::new(index_path),
                state: OnDiskFileState::Unloaded,
            },
            data: todo!(
                "figure we actually have to map it here or find a way to learn about the data files in advance."
            ),
        })
    }

    pub(crate) fn new_single(index_path: PathBuf) -> Self {
        let data_path = index_path.with_extension("pack");
        Self::Index(IndexFileBundle {
            index: OnDiskFile {
                path: Arc::new(index_path),
                state: OnDiskFileState::Unloaded,
            },
            data: OnDiskFile {
                path: Arc::new(data_path),
                state: OnDiskFileState::Unloaded,
            },
        })
    }
}

#[derive(Default)]
pub(crate) struct MutableIndexAndPack {
    pub(crate) files: ArcSwap<Option<IndexAndPacks>>,
    pub(crate) write: parking_lot::Mutex<()>,
}

/// A snapshot about resource usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub num_handles: usize,
    pub num_refreshes: usize,
    pub open_indices: usize,
    pub known_indices: usize,
    pub open_packs: usize,
    pub known_packs: usize,
    pub unused_slots: usize,
}