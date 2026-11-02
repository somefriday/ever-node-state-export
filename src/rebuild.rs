//! Builds a state in memory before it is written out. The cells the state shares with a persistent
//! state come from that file, which is read in one pass; every other cell is read from the node's
//! database, several at a time, and checked against its hash. Reading all of a mainnet state from
//! the database cell by cell takes hours, as each cell is a random read.

use crate::progress::{Progress, Unit};
use anyhow::{anyhow, bail, Context, Result};
use dashmap::DashMap;
use ever_block::{
    BocReader, BocWriter, Cell, DataCell, DoneCellsStorage, SimpleOrderedCellsStorage, UInt256,
    MAX_SAFE_DEPTH,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        Arc,
    },
};

/// The cells of a persistent state, by hash.
pub type Known = HashMap<UInt256, Cell>;

/// Subtrees per thread to share out once the top of the state is read.
const TASKS_PER_THREAD: usize = 64;

/// Builds the cells of a persistent state file in memory and indexes them by hash.
pub fn parse_persistent(data: Vec<u8>) -> Result<(Cell, Known)> {
    let total = boc_cells_count(&data);
    let progress = Arc::new(Progress::new("building its cells", Unit::Cells, total));
    let capacity = total.unwrap_or(0) as usize;
    let cells = Rc::new(RefCell::new(Cells {
        by_index: Vec::with_capacity(capacity),
        by_hash: HashMap::with_capacity(capacity),
        progress: progress.clone(),
    }));
    let root = BocReader::new()
        .set_done_cells_storage(Box::new(SharedCells(cells.clone())))
        .read_inmem(Arc::new(data))?
        .withdraw_single_root()?;
    progress.finish();
    let cells = Rc::try_unwrap(cells).map_err(|_| anyhow!("the BOC reader kept the cells"))?;
    Ok((root, cells.into_inner().by_hash))
}

/// The number of cells a BOC holds, from its header: the magic, a flags byte that ends with the
/// size of a cell reference, the size of an offset, then the cell count.
fn boc_cells_count(data: &[u8]) -> Option<u64> {
    if data.get(..4)? != [0xb5, 0xee, 0x9c, 0x72] {
        return None;
    }
    let ref_size = (*data.get(4)? & 0x07) as usize;
    let count = data.get(6..6 + ref_size)?;
    Some(count.iter().fold(0, |n, &byte| n << 8 | byte as u64))
}

/// Where BocReader keeps the cells it has built: by position, which it needs to resolve
/// references, and by hash, which the rebuild needs.
struct Cells {
    by_index: Vec<Option<Cell>>,
    by_hash: Known,
    progress: Arc<Progress>,
}

struct SharedCells(Rc<RefCell<Cells>>);

impl DoneCellsStorage for SharedCells {
    fn insert(&mut self, index: u32, cell: Cell) -> Result<()> {
        let mut cells = self.0.borrow_mut();
        let index = index as usize;
        if cells.by_index.len() <= index {
            cells.by_index.resize(index + 1, None);
        }
        cells.by_hash.insert(cell.repr_hash(), cell.clone());
        cells.by_index[index] = Some(cell);
        cells.progress.add(1);
        Ok(())
    }

    fn get(&self, index: u32) -> Result<Cell> {
        let cells = self.0.borrow();
        let cell = cells.by_index.get(index as usize).cloned().flatten();
        cell.with_context(|| format!("cell #{index} of the persistent state is missing"))
    }

    fn cleanup(&mut self) -> Result<()> {
        self.0.borrow_mut().by_index = Vec::new();
        Ok(())
    }
}

/// Builds the state under `root` in memory. Cells found in `known` are used as they are; every
/// other cell is read from the database, `threads` at a time, rebuilt and checked against its hash.
/// `progress` counts the cells read from the database.
pub fn rebuild(root: &Cell, known: &Known, threads: usize, progress: &Progress) -> Result<Cell> {
    if let Some(cell) = known.get(&root.repr_hash()) {
        return Ok(cell.clone());
    }
    let threads = threads.max(1);
    let built = DashMap::new();

    // Read the top of the tree level by level, until it splits into enough subtrees to share out.
    let mut top: Known = HashMap::new();
    top.insert(root.repr_hash(), root.clone());
    let mut level = vec![root.clone()];
    while !level.is_empty() && level.len() < threads * TASKS_PER_THREAD {
        let mut wanted = Vec::new();
        let mut queued = HashSet::new();
        for cell in &level {
            for i in 0..cell.references_count() {
                let hash = cell.reference_repr_hash(i)?;
                if !known.contains_key(&hash) && !top.contains_key(&hash) && queued.insert(hash) {
                    wanted.push((cell, i));
                }
            }
        }
        level = read_children(&wanted, threads, progress)?;
        for cell in &level {
            top.insert(cell.repr_hash(), cell.clone());
        }
    }

    // Each remaining subtree is read and built by one thread.
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(level.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut pending = HashMap::new();
                    while let Some(task) = level.get(next.fetch_add(1, Relaxed)) {
                        if failed.load(Relaxed) {
                            break;
                        }
                        let result = build_subtree(task, known, &built, &mut pending, Some(&top), progress);
                        if result.is_err() {
                            failed.store(true, Relaxed);
                            return result;
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        workers.into_iter().try_for_each(|worker| worker.join().map_err(|_| anyhow!("a reading thread panicked"))?)
    })?;

    // Then the top of the tree, from the cells read above.
    build_subtree(root, known, &built, &mut top, None, progress)?;
    let rebuilt = built.get(&root.repr_hash()).map(|cell| cell.clone());
    rebuilt.context("the state was not rebuilt")
}

/// Reads the given children from the database, `threads` at a time.
fn read_children(wanted: &[(&Cell, usize)], threads: usize, progress: &Progress) -> Result<Vec<Cell>> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    std::thread::scope(|scope| {
        let parts: Vec<_> = wanted
            .chunks(wanted.len().div_ceil(threads))
            .map(|part| {
                scope.spawn(move || {
                    let read = |&(parent, i): &(&Cell, usize)| -> Result<Cell> {
                        let child = parent.reference(i)?;
                        progress.add(1);
                        Ok(child)
                    };
                    part.iter().map(read).collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        let mut children = Vec::with_capacity(wanted.len());
        for part in parts {
            children.extend(part.join().map_err(|_| anyhow!("a reading thread panicked"))??);
        }
        Ok(children)
    })
}

/// Builds `top` and every cell under it that is neither known nor built yet, children first.
/// Cells already read from the database are taken from `pending` (this thread's) or `read` (shared);
/// the rest are read here.
fn build_subtree(
    top: &Cell,
    known: &Known,
    built: &DashMap<UInt256, Cell>,
    pending: &mut Known,
    read: Option<&Known>,
    progress: &Progress,
) -> Result<()> {
    enum Step {
        Enter(Cell),
        Build(Cell),
    }
    let mut steps = vec![Step::Enter(top.clone())];
    while let Some(step) = steps.pop() {
        match step {
            Step::Enter(cell) => {
                if built.contains_key(&cell.repr_hash()) {
                    continue;
                }
                steps.push(Step::Build(cell.clone()));
                for i in 0..cell.references_count() {
                    let hash = cell.reference_repr_hash(i)?;
                    if known.contains_key(&hash) || built.contains_key(&hash) {
                        continue;
                    }
                    let child = match pending.get(&hash).or_else(|| read?.get(&hash)) {
                        Some(child) => child.clone(),
                        None => {
                            let child = cell.reference(i)?;
                            progress.add(1);
                            pending.insert(hash, child.clone());
                            child
                        }
                    };
                    steps.push(Step::Enter(child));
                }
            }
            Step::Build(cell) => {
                let hash = cell.repr_hash();
                if built.contains_key(&hash) {
                    continue;
                }
                let mut children = Vec::with_capacity(cell.references_count());
                for i in 0..cell.references_count() {
                    let child_hash = cell.reference_repr_hash(i)?;
                    let child = match known.get(&child_hash) {
                        Some(child) => child.clone(),
                        None => built.get(&child_hash).map(|child| child.clone()).context("a child is missing")?,
                    };
                    children.push(child);
                }
                let data = cell.raw_data()?.to_vec();
                let rebuilt = Cell::with_cell_impl(DataCell::with_raw_data(children, data, Some(MAX_SAFE_DEPTH))?);
                if rebuilt.repr_hash() != hash {
                    bail!(
                        "cell {hash:x} read from the database hashes to {:x}: the database is damaged",
                        rebuilt.repr_hash()
                    );
                }
                built.insert(hash.clone(), rebuilt);
                pending.remove(&hash);
            }
        }
    }
    Ok(())
}

/// Serializes a state that is in memory, the same bytes as `write_boc`. `cells` is about how many
/// cells the state has, for the progress lines.
pub fn serialize(root: &Cell, cells: u64, name: &str) -> Result<Vec<u8>> {
    // BocWriter calls its abort check about three times per cell: twice while it puts the cells in
    // order, once while it writes them out.
    let progress = Progress::new(format!("writing {name}"), Unit::Steps, Some(3 * cells)).estimated();
    let abort = || {
        progress.add(1);
        false
    };
    let cells = SimpleOrderedCellsStorage::default();
    let writer = BocWriter::with_params([root.clone()], MAX_SAFE_DEPTH, cells, &abort)?;
    let mut boc = Vec::with_capacity(writer.data_size() + 4 * writer.references_count() + 64);
    writer.write(&mut boc)?;
    progress.finish();
    Ok(boc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ever_block::{write_boc, BuilderData, HashmapE, HashmapType, IBitstring, SliceData};

    /// A dictionary with `n` entries, plus a cell that holds it twice and one of its values
    /// directly, so cells are shared at different depths.
    fn state(n: u32) -> Cell {
        let mut dict = HashmapE::with_bit_len(32);
        for key in 0..n {
            let mut value = BuilderData::new();
            value.append_u64(key as u64 * 7).unwrap();
            let mut child = BuilderData::new();
            child.append_u32(key % 5).unwrap();
            value.checked_append_reference(child.into_cell().unwrap()).unwrap();
            let mut key_bits = BuilderData::new();
            key_bits.append_u32(key).unwrap();
            dict.set_builder(SliceData::load_cell(key_bits.into_cell().unwrap()).unwrap(), &value).unwrap();
        }
        let mut shared = BuilderData::new();
        shared.append_u32(3).unwrap();
        let shared = shared.into_cell().unwrap();
        let mut root = BuilderData::new();
        root.append_u8(1).unwrap();
        for _ in 0..2 {
            let mut holder = BuilderData::new();
            holder.checked_append_reference(dict.data().cloned().unwrap()).unwrap();
            holder.checked_append_reference(shared.clone()).unwrap();
            root.checked_append_reference(holder.into_cell().unwrap()).unwrap();
        }
        root.checked_append_reference(shared).unwrap();
        root.into_cell().unwrap()
    }

    fn all_cells(root: &Cell) -> Known {
        let mut cells = Known::new();
        let mut stack = vec![root.clone()];
        while let Some(cell) = stack.pop() {
            if cells.insert(cell.repr_hash(), cell.clone()).is_none() {
                stack.extend((0..cell.references_count()).map(|i| cell.reference(i).unwrap()));
            }
        }
        cells
    }

    fn check(root: &Cell, known: &Known, threads: usize) -> u64 {
        let progress = Progress::new("reading", Unit::Cells, None);
        let rebuilt = rebuild(root, known, threads, &progress).unwrap();
        assert_eq!(rebuilt.repr_hash(), root.repr_hash());
        assert_eq!(write_boc(&rebuilt).unwrap(), write_boc(root).unwrap());
        assert_eq!(serialize(&rebuilt, 100, "test").unwrap(), write_boc(root).unwrap());
        progress.done()
    }

    #[test]
    fn rebuilds_a_state_from_scratch() {
        let root = state(3000);
        let cells = all_cells(&root).len() as u64;
        // One thread reads every cell but the root exactly once. More threads can now and then
        // read a shared cell twice, when two of them reach it before either has built it.
        assert_eq!(check(&root, &Known::new(), 1), cells - 1);
        for threads in [3, 16] {
            assert!(check(&root, &Known::new(), threads) >= cells - 1);
        }
    }

    #[test]
    fn reads_only_what_the_persistent_state_lacks() {
        let old = state(3000);
        let new = state(3100);
        let known = all_cells(&old);
        let missing = all_cells(&new).keys().filter(|hash| !known.contains_key(*hash)).count() as u64;
        assert!(missing < 1000, "{missing}");
        assert_eq!(check(&new, &known, 1), missing - 1);
        assert!(check(&new, &known, 4) >= missing - 1);
        // the same state as the persistent one needs nothing from the database
        assert_eq!(check(&old, &known, 4), 0);
    }

    #[test]
    fn persistent_state_file_is_read_with_all_its_cells() {
        let root = state(500);
        let boc = write_boc(&root).unwrap();
        assert_eq!(boc_cells_count(&boc), Some(all_cells(&root).len() as u64));
        let (parsed, cells) = parse_persistent(boc).unwrap();
        assert_eq!(parsed.repr_hash(), root.repr_hash());
        assert_eq!(cells.len(), all_cells(&root).len());
    }
}
