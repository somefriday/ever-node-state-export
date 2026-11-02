//! Blocks and their states in the node's database.
//!
//! The node saves the state of each block in the background, through a queue of up to 1000 states;
//! the states still in the queue when it stops are never saved. ever-node finds a masterchain block
//! by its number in the last applied block's state, so that lookup fails whenever that state wasn't
//! saved. The tool starts from the newest state the node did save instead, and builds the state of a
//! later block by applying the blocks in between, as the node itself does after a restart.

use crate::progress::{Progress, Unit};
use anyhow::{bail, ensure, Context, Result};
use ever_block::BlockIdExt;
use ever_node::{
    collator_test_bundle::{create_engine_allocated, create_engine_telemetry},
    internal_db::{InternalDb, LAST_APPLIED_MC_BLOCK},
    shard_state::ShardStateStuff,
};
use std::sync::Arc;

/// How many blocks back from a block the tool looks for a saved state. The node's queue holds up to
/// 1000 states, so the blocks it hadn't saved when it stopped are well within this.
const MAX_UNSAVED: usize = 10_000;

/// The id of masterchain block `seqno`: from the newest saved masterchain state at or after it, or
/// from the chain of blocks after that state.
pub fn find_mc_block(db: &InternalDb, seqno: u32) -> Result<BlockIdExt> {
    let mut id = last_applied(db)?;
    ensure!(seqno <= id.seq_no(), "the node's last applied block is {}", id.seq_no());
    for _ in 0..MAX_UNSAVED {
        if id.seq_no() == seqno {
            return Ok(id);
        }
        if saved_state(db, &id)? {
            return db.load_shard_state_dynamic(&id)?.find_block_id(seqno);
        }
        id = db.load_block_prev1(&id)?;
    }
    bail!("the node saved none of the last {MAX_UNSAVED} masterchain states")
}

/// The newest masterchain block whose state the node saved.
pub fn newest_saved_mc_state(db: &InternalDb) -> Result<BlockIdExt> {
    let mut id = last_applied(db)?;
    for _ in 0..MAX_UNSAVED {
        if saved_state(db, &id)? {
            return Ok(id);
        }
        id = db.load_block_prev1(&id)?;
    }
    bail!("the node saved none of the last {MAX_UNSAVED} masterchain states")
}

fn last_applied(db: &InternalDb) -> Result<BlockIdExt> {
    let id = db.load_full_node_state(LAST_APPLIED_MC_BLOCK)?.context("the node has applied no block")?;
    Ok(BlockIdExt::clone(&id))
}

fn saved_state(db: &InternalDb, id: &BlockIdExt) -> Result<bool> {
    Ok(db.load_block_handle(id)?.is_some_and(|handle| handle.has_saved_state()))
}

/// The newest saved state at or before block `id`, and the blocks after it up to `id`, oldest
/// first. Each of those blocks must have its data in the database and one previous block.
pub fn saved_state_before(db: &InternalDb, id: &BlockIdExt) -> Result<(Arc<ShardStateStuff>, Vec<BlockIdExt>)> {
    let mut blocks = Vec::new();
    let mut id = id.clone();
    loop {
        let handle = db.load_block_handle(&id)?.with_context(|| format!("no record of block {id}"))?;
        if handle.has_saved_state() {
            let state = db.load_shard_state_dynamic(&id)?;
            blocks.reverse();
            return Ok((state, blocks));
        }
        ensure!(handle.has_data(), "neither the state nor the data of block {id} is in the database");
        ensure!(!handle.has_prev2(), "block {id} merges two shards");
        ensure!(blocks.len() < MAX_UNSAVED, "no state of the {MAX_UNSAVED} blocks before {id} is saved");
        let prev = db.load_block_prev1(&id)?;
        blocks.push(std::mem::replace(&mut id, prev));
    }
}

/// The state of block `id`, which `what` names: the one the node saved, or the newest saved state
/// before it with the state updates of the blocks in between applied. Each update checks the state
/// it applies to and the state it makes against the hashes in the block.
pub async fn load_state(db: &InternalDb, id: &BlockIdExt, what: &str) -> Result<Arc<ShardStateStuff>> {
    let (mut state, blocks) = saved_state_before(db, id)?;
    if blocks.is_empty() {
        return Ok(state);
    }
    let saved = state.block_id().seq_no();
    let what = format!("building the state of {what} from the saved state of block {saved}");
    let progress = Progress::new(what, Unit::Blocks, Some(blocks.len() as u64));
    let (telemetry, allocated) = (create_engine_telemetry(), create_engine_allocated());
    for id in blocks {
        let handle = db.load_block_handle(&id)?.with_context(|| format!("no record of block {id}"))?;
        let block = db.load_block_data(&handle).await?;
        let root = block.block()?.read_state_update()?.apply_for(state.root_cell())?;
        state = ShardStateStuff::from_state_root_cell(id, root, &telemetry, &allocated)?;
        progress.add(1);
    }
    progress.finish();
    Ok(state)
}

/// The blocks a masterchain block lists as the latest of each shard, read from the block itself.
pub async fn listed_blocks(db: &InternalDb, id: &BlockIdExt) -> Result<Vec<BlockIdExt>> {
    let handle = db.load_block_handle(id)?.context("the database has no record of it")?;
    db.load_block_data(&handle).await?.top_blocks_all()
}
