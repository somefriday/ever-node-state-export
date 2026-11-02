//! Single-root BOC files, read and written without building a cell object per cell. A mainnet
//! masterchain state has over 100 million cells: as ever-block objects they take some 40 GB, while
//! here a file costs its own size plus a few bytes per cell.

use crate::{
    cells::Cells,
    progress::{Progress, Unit},
};
use anyhow::{bail, ensure, Result};
use ever_block::{BIG_CELL_D1, MAX_SAFE_DEPTH};
use std::io::Write;

const TAG: [u8; 4] = [0xb5, 0xee, 0x9c, 0x72];
/// The tag of a BOC with Everscale big cells, whose header has two more fields.
const TAG_BIG_CELLS: [u8; 4] = [0xb6, 0xff, 0x9a, 0x73];

/// The children of a cell, by cell number.
#[derive(Clone, Copy, Default)]
pub struct Refs {
    ids: [u32; 4],
    len: u8,
}

impl Refs {
    pub fn push(&mut self, id: u32) {
        self.ids[self.len as usize] = id;
        self.len += 1;
    }
}

impl std::ops::Deref for Refs {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        &self.ids[..self.len as usize]
    }
}

/// The length of a cell's descriptor bytes, stored hashes and data, the way ever-block lays them
/// out (`full_len`), and its number of children; None when `raw` is too short to tell.
pub fn raw_len(raw: &[u8]) -> Option<(usize, usize)> {
    let (&d1, &d2) = (raw.first()?, raw.get(1)?);
    if d1 == BIG_CELL_D1 {
        let len = (d2 as usize) << 16 | (*raw.get(2)? as usize) << 8 | *raw.get(3)? as usize;
        return Some((4 + len, 0));
    }
    let refs = (d1 & 7) as usize;
    let level = (d1 >> 5).count_ones() as usize;
    let pruned = d1 & 8 != 0 && refs == 0 && level != 0;
    let hashes = if d1 & 16 == 0 { 0 } else if pruned { 1 } else { level + 1 };
    Some((2 + hashes * 34 + (d2 >> 1) as usize + (d2 & 1) as usize, refs))
}

/// A BOC file with one root, its cells numbered by their position in it.
pub struct BocFile {
    data: Vec<u8>,
    ref_size: usize,
    root: u32,
    /// Where each cell starts in `data`, then where the last one ends.
    offsets: Vec<u64>,
}

impl BocFile {
    /// Finds every cell of the file and checks that each refers only to cells after it.
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        ensure!(data.len() > 6, "the file is too short for a BOC");
        let tag = &data[..4];
        ensure!(tag == TAG || tag == TAG_BIG_CELLS, "the file is not a BOC");
        let flags = data[4];
        let (has_index, has_crc, ref_size) = (flags & 0x80 != 0, flags & 0x40 != 0, (flags & 7) as usize);
        let offset_size = data[5] as usize;
        ensure!((1..=4).contains(&ref_size) && (1..=8).contains(&offset_size), "the BOC header is damaged");
        let mut pos = 6;
        let mut field = |size: usize| -> Result<usize> {
            let bytes = data.get(pos..pos + size).ok_or_else(|| anyhow::anyhow!("the BOC header is cut off"))?;
            pos += size;
            Ok(bytes.iter().fold(0, |value, &byte| value << 8 | byte as usize))
        };
        let (cells, roots, absent) = (field(ref_size)?, field(ref_size)?, field(ref_size)?);
        let size = field(offset_size)?;
        if tag == TAG_BIG_CELLS {
            field(ref_size)?;
            field(offset_size)?;
        }
        ensure!(roots == 1, "the BOC has {roots} roots, a state has one");
        ensure!(absent == 0, "the BOC has absent cells");
        let root = field(ref_size)?;
        ensure!(root < cells, "the BOC's root is not one of its cells");
        if has_index {
            pos += cells * offset_size;
        }
        let end = pos + size;
        ensure!(
            data.len() == end + 4 * has_crc as usize,
            "the file has {} bytes, its BOC header says {}",
            data.len(),
            end + 4 * has_crc as usize
        );

        let mut offsets = Vec::with_capacity(cells + 1);
        for i in 0..cells {
            offsets.push(pos as u64);
            let Some((len, refs)) = raw_len(&data[pos..end]) else { bail!("cell {i} of the BOC is cut off") };
            ensure!(data[pos] == BIG_CELL_D1 || refs <= 4, "cell {i} of the BOC is damaged");
            let refs_end = pos + len + refs * ref_size;
            ensure!(refs_end <= end, "cell {i} of the BOC is cut off");
            for child in data[pos + len..refs_end].chunks(ref_size) {
                let child = child.iter().fold(0, |value, &byte| value << 8 | byte as usize);
                ensure!(child > i && child < cells, "cell {i} of the BOC refers to cell {child}");
            }
            pos = refs_end;
        }
        ensure!(pos == end, "the BOC has data after its last cell");
        offsets.push(pos as u64);
        Ok(Self { data, ref_size, root: root as u32, offsets })
    }

    pub fn cells(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn root(&self) -> u32 {
        self.root
    }

    /// The cell's descriptor bytes, stored hashes and data: all of it but the references.
    pub fn raw(&self, i: u32) -> &[u8] {
        let start = self.offsets[i as usize] as usize;
        let (len, _) = raw_len(&self.data[start..]).expect("checked by parse");
        &self.data[start..start + len]
    }

    pub fn refs(&self, i: u32) -> Refs {
        let (start, end) = (self.offsets[i as usize] as usize, self.offsets[i as usize + 1] as usize);
        let (len, count) = raw_len(&self.data[start..]).expect("checked by parse");
        let mut refs = Refs::default();
        for child in self.data[start + len..end].chunks(self.ref_size).take(count) {
            refs.push(child.iter().fold(0, |value, &byte| value << 8 | byte as u32));
        }
        refs
    }
}

/// The cells under `root` in the order ever-block's BocWriter writes them. It walks the tree depth
/// first, entering a child unless an earlier child of the same cell or any cell already entered is
/// the same cell, and writes the cells in the reverse of the order it leaves them.
fn order(cells: &Cells, root: u32) -> Vec<u32> {
    enum Step {
        Enter(u32),
        Leave(u32),
    }
    let mut entered = vec![false; cells.len()];
    let mut left = Vec::new();
    let mut steps = vec![Step::Enter(root)];
    while let Some(step) = steps.pop() {
        match step {
            Step::Leave(id) => left.push(id),
            Step::Enter(id) => {
                if std::mem::replace(&mut entered[id as usize], true) {
                    continue;
                }
                steps.push(Step::Leave(id));
                let mut children = Refs::default();
                for &child in cells.refs(id).iter() {
                    if !children.contains(&child) && !entered[child as usize] {
                        children.push(child);
                    }
                }
                steps.extend(children.iter().rev().map(|&child| Step::Enter(child)));
            }
        }
    }
    left.reverse();
    left
}

/// Writes the state under `root` byte for byte as ever-block's `write_boc` would. `what` names the
/// step for its progress line.
pub fn write(cells: &Cells, root: u32, out: impl Write, what: &str) -> Result<()> {
    let depth = cells.depth(root);
    ensure!(depth <= MAX_SAFE_DEPTH, "the state is too deep: {depth} > {MAX_SAFE_DEPTH}");
    let order = order(cells, root);
    let mut position = vec![u32::MAX; cells.len()];
    let (mut data_size, mut references, mut big_cells, mut big_cells_size) = (0, 0, 0, 0);
    for (n, &id) in order.iter().enumerate() {
        position[id as usize] = n as u32;
        let raw = cells.raw(id);
        data_size += raw.len();
        references += cells.refs(id).len();
        if raw[0] == BIG_CELL_D1 {
            big_cells += 1;
            big_cells_size += raw.len();
        }
    }
    let bytes_for = |n: usize| (usize::BITS - n.leading_zeros()).div_ceil(8) as usize;
    let ref_size = bytes_for(order.len());
    let size = data_size + references * ref_size;
    let offset_size = bytes_for(size);

    let progress = Progress::new(what, Unit::Cells, Some(order.len() as u64));
    let mut out = std::io::BufWriter::with_capacity(8 << 20, out);
    out.write_all(if big_cells > 0 { &TAG_BIG_CELLS } else { &TAG })?;
    out.write_all(&[ref_size as u8, offset_size as u8])?;
    let mut fields = vec![(order.len(), ref_size), (1, ref_size), (0, ref_size), (size, offset_size)];
    if big_cells > 0 {
        fields.extend([(big_cells, ref_size), (big_cells_size, offset_size)]);
    }
    // the root is the first cell
    fields.push((0, ref_size));
    for (value, size) in fields {
        out.write_all(&(value as u64).to_be_bytes()[8 - size..])?;
    }
    for &id in &order {
        out.write_all(cells.raw(id))?;
        for &child in cells.refs(id).iter() {
            out.write_all(&position[child as usize].to_be_bytes()[4 - ref_size..])?;
        }
        progress.add(1);
    }
    out.flush()?;
    progress.finish();
    Ok(())
}
