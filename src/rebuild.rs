//! Rebuilds a state from the persistent states the node keeps on disk and the cells that changed
//! since, which come from the node's database. Reading all of a mainnet state from the database cell
//! by cell takes hours, as each cell is a random read; a persistent state file is read in one pass.

use crate::{
    boc::Refs,
    cells::{Cells, DbCell, HashedBoc},
    progress::Progress,
};
use anyhow::{anyhow, ensure, Result};
use dashmap::DashMap;
use ever_block::{Cell, UInt256};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// The cells of the state under `root`, and the number of its root: the cells of `bases` it shares
/// and the others read from the database, `threads` at a time. `progress` counts the cells read.
pub fn rebuild(root: &Cell, bases: Vec<HashedBoc>, threads: usize, progress: &Progress) -> Result<(Cells, u32)> {
    let mut cells = Cells::new(bases);
    let (id, read) = read_changed(root, &cells, threads, progress)?;
    cells.add_read(read)?;
    ensure!(*cells.hash(id) == *root.repr_hash().as_slice(), "the state was rebuilt wrong");
    Ok((cells, id))
}

/// Cells of the database with the numbers they get.
type Numbered<T> = Vec<(u32, T)>;

/// Reads every cell under `root` that `cells` lacks, level by level, sharing out each level among
/// `threads`. The cells read are numbered from `cells.len()` on in the order they are found. Returns
/// the root's number and the cells read.
fn read_changed(root: &Cell, cells: &Cells, threads: usize, progress: &Progress) -> Result<(u32, Vec<DbCell>)> {
    if let Some(id) = cells.find(&root.repr_hash()) {
        return Ok((id, Vec::new()));
    }
    let first = cells.len() as u32;
    let found: DashMap<UInt256, u32> = DashMap::new();
    found.insert(root.repr_hash(), first);
    let next = AtomicU32::new(first + 1);
    // Reads the children of the cells of `part` that are new, and keeps each cell's bytes.
    let read_part = |part: &[(u32, Cell)]| -> Result<(Numbered<DbCell>, Numbered<Cell>)> {
        let mut done = Vec::with_capacity(part.len());
        let mut children = Vec::new();
        for (id, cell) in part {
            let mut refs = Refs::default();
            for i in 0..cell.references_count() {
                let hash = cell.reference_repr_hash(i)?;
                let child = match cells.find(&hash) {
                    Some(child) => child,
                    None => {
                        let mut new = false;
                        let child = *found.entry(hash).or_insert_with(|| {
                            new = true;
                            next.fetch_add(1, Relaxed)
                        });
                        if new {
                            children.push((child, cell.reference(i)?));
                            progress.add(1);
                        }
                        child
                    }
                };
                refs.push(child);
            }
            done.push((*id, DbCell { raw: cell.raw_data()?.to_vec(), refs, hash: cell.repr_hash() }));
        }
        Ok((done, children))
    };

    let mut read = Vec::new();
    let mut level = vec![(first, root.clone())];
    while !level.is_empty() {
        let parts = std::thread::scope(|scope| {
            let workers: Vec<_> = level
                .chunks(level.len().div_ceil(threads.max(1)))
                .map(|part| scope.spawn(|| read_part(part)))
                .collect();
            let panicked = || anyhow!("a reading thread panicked");
            workers.into_iter().map(|part| part.join().map_err(|_| panicked())?).collect::<Result<Vec<_>>>()
        })?;
        level = Vec::new();
        for (done, children) in parts {
            read.extend(done);
            level.extend(children);
        }
    }
    read.sort_unstable_by_key(|(id, _)| *id);
    ensure!(read.len() as u32 == next.load(Relaxed) - first, "a cell was not read");
    Ok((first, read.into_iter().map(|(_, cell)| cell).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        boc::{self, BocFile},
        progress::Unit,
    };
    use ever_block::{
        create_big_cell, write_boc, BuilderData, CellImpl, CellType, DataCell, HashmapE, HashmapType, IBitstring,
        LevelMask, MerkleProof, Serializable, SliceData,
    };
    use std::collections::HashSet;

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

    /// A cell with 3 bits of data, so its last byte is partly filled, that stores its own hash.
    fn stores_its_hash() -> Cell {
        let mut cell = BuilderData::new();
        cell.append_bits(0b101, 3).unwrap();
        let cell = cell.into_cell().unwrap();
        let mut raw = cell.raw_data().unwrap().to_vec();
        raw[0] |= 16;
        raw.splice(2..2, cell.repr_hash().as_slice().iter().copied().chain(cell.repr_depth().to_be_bytes()));
        Cell::with_cell_impl(DataCell::with_raw_data(vec![], raw, None).unwrap())
    }

    /// `state(n)` with the cells the export handles besides ordinary ones: a Merkle proof with pruned
    /// branches (cells whose level isn't 0), a big cell and a cell that stores its hash.
    fn exotic_state(n: u32) -> Cell {
        let base = state(n);
        let keep: HashSet<UInt256> = [base.repr_hash(), base.reference(0).unwrap().repr_hash()].into();
        let proof = MerkleProof::create(&base, |hash| keep.contains(hash)).unwrap().serialize().unwrap();
        let big = create_big_cell(&vec![0x5a; 3000]).unwrap();
        let mut root = BuilderData::new();
        root.append_u32(n).unwrap();
        for cell in [proof, big, stores_its_hash(), base] {
            root.checked_append_reference(cell).unwrap();
        }
        root.into_cell().unwrap()
    }

    fn all_cells(root: &Cell) -> HashSet<UInt256> {
        let mut seen = HashSet::new();
        let mut stack = vec![root.clone()];
        while let Some(cell) = stack.pop() {
            if seen.insert(cell.repr_hash()) {
                stack.extend((0..cell.references_count()).map(|i| cell.reference(i).unwrap()));
            }
        }
        seen
    }

    fn hashed(root: &Cell) -> HashedBoc {
        let progress = Progress::new("hashing", Unit::Cells, None);
        HashedBoc::new(BocFile::parse(write_boc(root).unwrap()).unwrap(), 3, &progress).unwrap()
    }

    /// Exports `root` with the cells of `base` known and the rest read from `root`'s own cells,
    /// standing in for the database. Checks that the file is the one `write_boc` writes and returns
    /// how many cells were read.
    fn export(root: &Cell, base: Option<&Cell>, threads: usize) -> u64 {
        let progress = Progress::new("reading", Unit::Cells, None);
        let bases = base.map(hashed).into_iter().collect();
        let (cells, id) = rebuild(root, bases, threads, &progress).unwrap();
        let mut file = Vec::new();
        boc::write(&cells, id, &mut file, "test").unwrap();
        assert!(file == write_boc(root).unwrap(), "the file differs from write_boc's");
        progress.done()
    }

    #[test]
    fn rebuilds_a_state_from_scratch() {
        for root in [state(3000), exotic_state(300)] {
            for threads in [1, 3, 16] {
                // every cell but the root, which the caller has, is read exactly once
                assert_eq!(export(&root, None, threads), all_cells(&root).len() as u64 - 1);
            }
        }
    }

    #[test]
    fn reads_only_what_the_persistent_state_lacks() {
        for (old, new) in [(state(3000), state(3100)), (exotic_state(300), exotic_state(310))] {
            let known = all_cells(&old);
            let mut missing = HashSet::new();
            let mut stack = vec![new.clone()];
            while let Some(cell) = stack.pop() {
                if !known.contains(&cell.repr_hash()) && missing.insert(cell.repr_hash()) {
                    stack.extend((0..cell.references_count()).map(|i| cell.reference(i).unwrap()));
                }
            }
            assert!(missing.len() < all_cells(&new).len() / 2, "{missing:?}");
            for threads in [1, 4] {
                assert_eq!(export(&new, Some(&old), threads), missing.len() as u64 - 1);
            }
            // the same state as the persistent one needs nothing from the database
            assert_eq!(export(&old, Some(&old), 4), 0);
        }
    }

    #[test]
    fn files_hash_like_ever_block() {
        for root in [state(500), exotic_state(200)] {
            assert_eq!(hashed(&root).root_hash(), root.repr_hash());
        }
        let boc = write_boc(&state(50)).unwrap();
        assert!(BocFile::parse(boc[..boc.len() - 1].to_vec()).is_err());
        // a hash stored in a cell is checked
        let mut boc = write_boc(&exotic_state(20)).unwrap();
        let hash = stores_its_hash().repr_hash();
        let at = boc.windows(32).position(|bytes| bytes == hash.as_slice()).unwrap();
        boc[at] ^= 1;
        let progress = Progress::new("hashing", Unit::Cells, None);
        assert!(HashedBoc::new(BocFile::parse(boc).unwrap(), 2, &progress).is_err());
    }

    /// A database cell whose bytes don't match the hash the database keeps it under.
    struct Damaged(Cell, Vec<u8>);

    impl CellImpl for Damaged {
        fn data(&self) -> &[u8] {
            self.0.data()
        }
        fn raw_data(&self) -> ever_block::Result<&[u8]> {
            Ok(&self.1)
        }
        fn bit_length(&self) -> usize {
            self.0.bit_length()
        }
        fn references_count(&self) -> usize {
            self.0.references_count()
        }
        fn reference(&self, index: usize) -> ever_block::Result<Cell> {
            self.0.reference(index)
        }
        fn cell_type(&self) -> CellType {
            self.0.cell_type()
        }
        fn level_mask(&self) -> LevelMask {
            self.0.level_mask()
        }
        fn hash(&self, index: usize) -> UInt256 {
            self.0.hash(index)
        }
        fn depth(&self, index: usize) -> u16 {
            self.0.depth(index)
        }
        fn store_hashes(&self) -> bool {
            false
        }
    }

    #[test]
    fn damaged_database_is_reported() {
        let root = state(100);
        let mut raw = root.raw_data().unwrap().to_vec();
        *raw.last_mut().unwrap() ^= 1;
        let damaged = Cell::with_cell_impl(Damaged(root, raw));
        let progress = Progress::new("reading", Unit::Cells, None);
        let error = rebuild(&damaged, Vec::new(), 2, &progress).err().unwrap();
        assert!(format!("{error:#}").contains("the database is damaged"), "{error:#}");
    }
}
