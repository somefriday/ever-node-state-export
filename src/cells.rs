//! The cells of one state by number: those it shares with persistent states come from their files,
//! the rest from the node's database. Every cell's hash is computed from its bytes and its
//! children's hashes, as ever-block computes it, so no file or database is taken on trust.

use crate::{
    boc::{BocFile, Refs},
    progress::Progress,
};
use anyhow::{anyhow, bail, Context, Result};
use ever_block::{Cell, CellImpl, CellType, DataCell, LevelMask, UInt256};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Cells hashed at a time; each batch is shared out among the threads.
const BATCH: usize = 1 << 20;

/// What a parent needs of a child to be hashed.
struct Child<'a> {
    hash: &'a [u8; 32],
    depth: u16,
    /// Set for a cell whose level isn't 0 (see `hash_cell`).
    object: Option<&'a Cell>,
}

struct Hashed {
    hash: [u8; 32],
    depth: u16,
    object: Option<Cell>,
}

/// A cell's hash and depth, as ever-block computes them. Ordinary cells of level 0, nearly all cells
/// of a state, are hashed here. ever-block hashes the rest: exotic cells (Merkle proofs, pruned
/// branches, libraries, big cells) and cells whose level isn't 0. Children of level 0 take part in
/// that as stubs that carry only their hash and depth; other children are kept as ever-block objects,
/// as their parents need their hashes of every level.
fn hash_cell<'a>(raw: &[u8], refs: &[u32], child: impl Fn(u32) -> Child<'a>) -> Result<Hashed> {
    let (d1, d2) = (raw[0], raw[1]);
    let data = &raw[raw.len() - (d2 >> 1) as usize - (d2 & 1) as usize..];
    // ever-block hashes the data up to its completion tag, which here is in the last byte.
    let tag_in_last_byte = d2 & 1 == 0 || !matches!(data.last(), Some(0 | 0x80));
    if d1 & 0xe8 == 0 && tag_in_last_byte && refs.iter().all(|&id| child(id).object.is_none()) {
        let mut hasher = Sha256::new();
        hasher.update([d1 & 7, d2]);
        hasher.update(data);
        let mut depth = 0;
        for &id in refs {
            let child_depth = child(id).depth;
            depth = depth.max(child_depth + 1);
            hasher.update(child_depth.to_be_bytes());
        }
        for &id in refs {
            hasher.update(child(id).hash);
        }
        let hash: [u8; 32] = hasher.finalize().into();
        if d1 & 16 != 0 && (raw[2..34] != hash || raw[34..36] != depth.to_be_bytes()) {
            bail!("the hash stored in cell {} is wrong", UInt256::from(hash).to_hex_string());
        }
        return Ok(Hashed { hash, depth, object: None });
    }
    let references = refs
        .iter()
        .map(|&id| child(id))
        .map(|child| match child.object {
            Some(object) => object.clone(),
            None => Cell::with_cell_impl(Stub { hash: UInt256::from(*child.hash), depth: child.depth }),
        })
        .collect();
    let cell = Cell::with_cell_impl(DataCell::with_raw_data(references, raw.to_vec(), None)?);
    let hash = cell.repr_hash().inner();
    Ok(Hashed { hash, depth: cell.repr_depth(), object: (cell.level() != 0).then_some(cell) })
}

/// A child of level 0 that ever-block needs only the hash and depth of.
struct Stub {
    hash: UInt256,
    depth: u16,
}

impl CellImpl for Stub {
    fn data(&self) -> &[u8] {
        &[]
    }
    fn raw_data(&self) -> ever_block::Result<&[u8]> {
        ever_block::fail!("a stub cell has no data")
    }
    fn bit_length(&self) -> usize {
        0
    }
    fn references_count(&self) -> usize {
        0
    }
    fn reference(&self, _index: usize) -> ever_block::Result<Cell> {
        ever_block::fail!("a stub cell has no references")
    }
    fn cell_type(&self) -> CellType {
        CellType::Ordinary
    }
    fn level_mask(&self) -> LevelMask {
        LevelMask::with_mask(0)
    }
    fn hash(&self, _index: usize) -> UInt256 {
        self.hash.clone()
    }
    fn depth(&self, _index: usize) -> u16 {
        self.depth
    }
    fn store_hashes(&self) -> bool {
        false
    }
}

/// The cell's ever-block object, kept only for cells whose level isn't 0: there are seldom any, and
/// looking a cell up in an empty map is not free.
fn object(objects: &HashMap<u32, Cell>, i: u32) -> Option<&Cell> {
    if objects.is_empty() {
        None
    } else {
        objects.get(&i)
    }
}

/// A BOC file with the hash and depth of each of its cells.
pub struct HashedBoc {
    file: BocFile,
    hashes: Vec<[u8; 32]>,
    depths: Vec<u16>,
    objects: HashMap<u32, Cell>,
}

impl HashedBoc {
    /// Hashes every cell of the file on `threads` threads. A cell can be hashed once its children
    /// are, so the cells are taken by height, the length of the longest path down to a leaf: cells
    /// of the same height don't depend on each other.
    pub fn new(file: BocFile, threads: usize, progress: &Progress) -> Result<Self> {
        let count = file.cells();
        // Every cell refers only to cells after it.
        let mut heights = vec![0u16; count];
        for i in (0..count).rev() {
            heights[i] = file.refs(i as u32).iter().map(|&child| heights[child as usize] + 1).max().unwrap_or(0);
        }
        let mut starts = vec![0; heights.iter().max().map_or(0, |&top| top as usize + 2)];
        for &height in &heights {
            starts[height as usize + 1] += 1;
        }
        for height in 1..starts.len() {
            starts[height] += starts[height - 1];
        }
        let mut by_height = vec![0u32; count];
        let mut next = starts.clone();
        for (i, &height) in heights.iter().enumerate() {
            by_height[next[height as usize]] = i as u32;
            next[height as usize] += 1;
        }
        drop(heights);

        let (hashes, depths, objects) = (vec![[0; 32]; count], vec![0; count], HashMap::new());
        let mut hashed = Self { file, hashes, depths, objects };
        for height in starts.windows(2) {
            for batch in by_height[height[0]..height[1]].chunks(BATCH) {
                let results = std::thread::scope(|scope| {
                    let hashed = &hashed;
                    let parts: Vec<_> = batch
                        .chunks(batch.len().div_ceil(threads.max(1)))
                        .map(|part| {
                            scope.spawn(move || {
                                let hash = |&i: &u32| {
                                    let child = |id: u32| Child {
                                        hash: &hashed.hashes[id as usize],
                                        depth: hashed.depths[id as usize],
                                        object: object(&hashed.objects, id),
                                    };
                                    hash_cell(hashed.file.raw(i), &hashed.file.refs(i), child)
                                        .with_context(|| format!("cell {i} of the file"))
                                };
                                part.iter().map(hash).collect::<Result<Vec<_>>>()
                            })
                        })
                        .collect();
                    let panicked = || anyhow!("a hashing thread panicked");
                    parts.into_iter().map(|part| part.join().map_err(|_| panicked())?).collect::<Result<Vec<_>>>()
                })?;
                for (&i, cell) in batch.iter().zip(results.into_iter().flatten()) {
                    hashed.hashes[i as usize] = cell.hash;
                    hashed.depths[i as usize] = cell.depth;
                    if let Some(object) = cell.object {
                        hashed.objects.insert(i, object);
                    }
                }
                progress.add(batch.len() as u64);
            }
        }
        Ok(hashed)
    }

    pub fn root_hash(&self) -> UInt256 {
        UInt256::from(self.hashes[self.file.root() as usize])
    }
}

/// A cell read from the node's database: its bytes, its children and the hash the database keeps it
/// under.
pub struct DbCell {
    pub raw: Vec<u8>,
    pub refs: Refs,
    pub hash: UInt256,
}

struct ReadCell {
    raw: Vec<u8>,
    refs: Refs,
    hashed: Hashed,
}

/// The cells of one state: those of persistent states, numbered file after file, then those read
/// from the database.
pub struct Cells {
    bases: Vec<HashedBoc>,
    /// The number of each base's first cell, then that of the first cell read from the database.
    starts: Vec<u32>,
    read: Vec<ReadCell>,
    /// The cells of the bases by hash: open addressing, the slot found from the hash's first bytes.
    index: Vec<u32>,
    /// A cell that repeats an earlier one in the bases, and that earlier cell. A well-formed BOC
    /// holds each cell once, so this is normally empty.
    repeats: HashMap<u32, u32>,
}

const EMPTY: u32 = u32::MAX;

/// The positions of `cells`, numbered from `first` on, ordered so that each comes after its
/// children: the order a depth-first walk leaves them in.
fn children_first(cells: &[DbCell], first: u32) -> Result<Vec<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Entered,
        Left,
    }
    let mut marks = vec![Mark::New; cells.len()];
    let mut order = Vec::with_capacity(cells.len());
    for start in 0..cells.len() {
        let mut steps = vec![(start, false)];
        while let Some((k, leave)) = steps.pop() {
            if leave {
                marks[k] = Mark::Left;
                order.push(k);
                continue;
            }
            match marks[k] {
                Mark::Left => continue,
                Mark::Entered => bail!("the cells read from the database refer to each other in a loop"),
                Mark::New => marks[k] = Mark::Entered,
            }
            steps.push((k, true));
            for &id in cells[k].refs.iter() {
                if let Some(child) = id.checked_sub(first) {
                    if marks[child as usize] != Mark::Left {
                        steps.push((child as usize, false));
                    }
                }
            }
        }
    }
    Ok(order)
}

enum At<'a> {
    Base(&'a HashedBoc, u32),
    Read(&'a ReadCell),
}

impl Cells {
    pub fn new(bases: Vec<HashedBoc>) -> Self {
        let mut starts = vec![0u32];
        for base in &bases {
            starts.push(starts.last().unwrap() + base.file.cells() as u32);
        }
        let count = *starts.last().unwrap() as usize;
        let slots = (count + count / 2).next_power_of_two().max(1);
        let index = vec![EMPTY; slots];
        let mut cells = Self { bases, starts, read: Vec::new(), index, repeats: HashMap::new() };
        for id in 0..count as u32 {
            let mut slot = cells.slot(cells.hash(id));
            loop {
                match cells.index[slot] {
                    EMPTY => {
                        cells.index[slot] = id;
                        break;
                    }
                    other if cells.hash(other) == cells.hash(id) => {
                        cells.repeats.insert(id, other);
                        break;
                    }
                    _ => slot = (slot + 1) & (slots - 1),
                }
            }
        }
        cells
    }

    fn slot(&self, hash: &[u8; 32]) -> usize {
        u64::from_le_bytes(hash[..8].try_into().unwrap()) as usize & (self.index.len() - 1)
    }

    /// The number of a cell of the bases.
    pub fn find(&self, hash: &UInt256) -> Option<u32> {
        let hash = hash.as_slice();
        let mut slot = self.slot(hash);
        loop {
            match self.index[slot] {
                EMPTY => return None,
                id if self.hash(id) == hash => return Some(id),
                _ => slot = (slot + 1) & (self.index.len() - 1),
            }
        }
    }

    /// Adds cells read from the database, numbered from `len()` on in the order given; they refer to
    /// each other and to cells of the bases. Fails unless each hashes to the hash the database keeps
    /// it under.
    pub fn add_read(&mut self, cells: Vec<DbCell>) -> Result<()> {
        let first = self.len() as u32;
        let mut hashed: Vec<Option<Hashed>> = (0..cells.len()).map(|_| None).collect();
        for k in children_first(&cells, first)? {
            let cell = &cells[k];
            let child = |id: u32| match id.checked_sub(first) {
                Some(k) => {
                    let child = hashed[k as usize].as_ref().expect("children are hashed first");
                    Child { hash: &child.hash, depth: child.depth, object: child.object.as_ref() }
                }
                None => self.child(id),
            };
            let result = hash_cell(&cell.raw, &cell.refs, child)?;
            if result.hash != *cell.hash.as_slice() {
                bail!(
                    "cell {:x} read from the database hashes to {}: the database is damaged",
                    cell.hash,
                    UInt256::from(result.hash).to_hex_string()
                );
            }
            hashed[k] = Some(result);
        }
        let read = cells.into_iter().zip(hashed);
        let read = read.map(|(DbCell { raw, refs, .. }, hashed)| ReadCell { raw, refs, hashed: hashed.unwrap() });
        self.read.extend(read);
        Ok(())
    }

    pub fn len(&self) -> usize {
        *self.starts.last().unwrap() as usize + self.read.len()
    }

    fn at(&self, id: u32) -> At<'_> {
        let base = self.starts.partition_point(|&start| start <= id) - 1;
        match self.bases.get(base) {
            Some(file) => At::Base(file, id - self.starts[base]),
            None => At::Read(&self.read[(id - self.starts[base]) as usize]),
        }
    }

    fn child(&self, id: u32) -> Child<'_> {
        match self.at(id) {
            At::Base(file, i) => Child {
                hash: &file.hashes[i as usize],
                depth: file.depths[i as usize],
                object: object(&file.objects, i),
            },
            At::Read(ReadCell { hashed, .. }) => {
                Child { hash: &hashed.hash, depth: hashed.depth, object: hashed.object.as_ref() }
            }
        }
    }

    pub fn hash(&self, id: u32) -> &[u8; 32] {
        self.child(id).hash
    }

    pub fn depth(&self, id: u32) -> u16 {
        self.child(id).depth
    }

    /// The cell's descriptor bytes, stored hashes and data: what precedes its references in a BOC.
    pub fn raw(&self, id: u32) -> &[u8] {
        match self.at(id) {
            At::Base(file, i) => file.file.raw(i),
            At::Read(cell) => &cell.raw,
        }
    }

    pub fn refs(&self, id: u32) -> Refs {
        match self.at(id) {
            At::Base(file, i) => {
                let start = id - i;
                let mut refs = Refs::default();
                for &child in file.file.refs(i).iter() {
                    let child = start + child;
                    let repeated = if self.repeats.is_empty() { None } else { self.repeats.get(&child) };
                    refs.push(repeated.copied().unwrap_or(child));
                }
                refs
            }
            At::Read(cell) => cell.refs,
        }
    }
}
