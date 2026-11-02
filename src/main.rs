//! Exports the state of a chosen masterchain block from a stopped ever-node's database, together with
//! the state of the basechain block that masterchain block lists as latest. The two files are the
//! inputs of tycho-state-updater. `verify` checks such files against a node's own database.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use ever_block::{read_single_root_boc, write_boc, BlockIdExt, HashmapAugType};
use ever_node::{
    collator_test_bundle::{create_engine_allocated, create_engine_telemetry},
    internal_db::{
        InternalDb, InternalDbConfig, CURRENT_DB_VERSION, LAST_APPLIED_MC_BLOCK, SHARD_CLIENT_MC_BLOCK,
    },
    shard_state::ShardStateStuff,
};
use serde_json::json;
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (ever-node ",
    env!("EVER_NODE_VERSION"),
    ", commit ",
    env!("EVER_NODE_COMMIT"),
    ")"
);

#[derive(Parser)]
#[command(version = VERSION, about = "Export the ever-node states of a masterchain block for tycho-state-updater")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the masterchain state of block N and the basechain state it refers to
    Export(ExportArgs),
    /// Check published state files against this node's own database
    Verify(VerifyArgs),
    /// Show what this database holds and which ever-node version this build reads
    Info(InfoArgs),
}

/// Where the node's database is: a folder (routes 2 and 3) or an evernode_cli node (route 1).
#[derive(Args)]
struct Database {
    /// The node's database folder (internal_db_path in its config), not the `db` folder inside it
    #[arg(long, value_name = "DIR", required_unless_present = "evernode", conflicts_with = "evernode")]
    db: Option<PathBuf>,
    /// Name of a node managed by evernode_cli
    #[arg(long, value_name = "NAME")]
    evernode: Option<String>,
    /// evernode_cli's state directory [default: /var/lib/ever-validator]
    #[arg(long, value_name = "DIR", conflicts_with = "db")]
    evernode_state_dir: Option<PathBuf>,
}

#[derive(Args)]
struct ExportArgs {
    #[command(flatten)]
    database: Database,
    /// Number of the masterchain block to export
    #[arg(long, value_name = "N")]
    mc_seqno: u32,
    /// Folder for masterstate.boc, shardstate.boc and manifest.json
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Export block N even if it is a key block (for tests only)
    #[arg(long)]
    allow_key_block: bool,
}

#[derive(Args)]
struct VerifyArgs {
    #[command(flatten)]
    database: Database,
    /// Number of the masterchain block the files should hold
    #[arg(long, value_name = "N")]
    mc_seqno: u32,
    /// Published masterchain state file (masterstate.boc)
    #[arg(long, value_name = "FILE")]
    master: PathBuf,
    /// Published basechain state file (shardstate.boc)
    #[arg(long, value_name = "FILE")]
    shard: PathBuf,
}

#[derive(Args)]
struct InfoArgs {
    #[command(flatten)]
    database: Database,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Export(args) => export(args).await,
        Command::Verify(args) => verify(args).await,
        Command::Info(args) => info(args).await,
    }
}

/// ever-node writes this marker into the database folder when it opens it and removes it on a clean
/// shutdown. A node that finds it at start assumes it crashed.
const RUNNING_MARKER: &str = "ton_node.running";

/// Removes the running marker when the command ends, if opening the database created it.
struct MarkerGuard(Option<PathBuf>);

impl Drop for MarkerGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Opens the node's database given by `--db` or `--evernode`; also returns its folder.
async fn open_database(database: &Database) -> Result<(InternalDb, MarkerGuard, PathBuf)> {
    match (&database.db, &database.evernode) {
        (Some(path), _) => {
            let (db, marker) = open_db(path, "stop the node first").await?;
            Ok((db, marker, path.clone()))
        }
        (None, Some(name)) => {
            let state_dir = database.evernode_state_dir.as_deref();
            let stop = match state_dir {
                Some(dir) => format!("evernode --state-dir {} node stop -n {name} --force", dir.display()),
                None => format!("evernode node stop -n {name} --force"),
            };
            let path = evernode_database(state_dir.unwrap_or(Path::new("/var/lib/ever-validator")), name, &stop)?;
            let (db, marker) = open_db(&path, &format!("stop it first with: {stop}")).await?;
            Ok((db, marker, path))
        }
        (None, None) => unreachable!("clap requires --db or --evernode"),
    }
}

/// Finds an evernode_cli node's database folder and checks that the node is stopped and runs the
/// ever-node commit this build reads. `stop` is the command that stops the node.
fn evernode_database(state_dir: &Path, name: &str, stop: &str) -> Result<PathBuf> {
    let node_dir = state_dir.join("nodes").join(name);
    let node_json = node_dir.join("node.json");
    let node: serde_json::Value = serde_json::from_slice(&std::fs::read(&node_json).with_context(|| {
        format!(
            "cannot read {} (no evernode node named {name}? evernode keeps node data root-only, so run \
             this as root)",
            node_json.display()
        )
    })?)
    .with_context(|| format!("{} is not valid JSON", node_json.display()))?;

    let image_id = node["image_id"].as_str().context("node.json has no image_id")?;
    let commit = evernode_node_commit(state_dir, image_id)?;
    if commit != env!("EVER_NODE_COMMIT") {
        bail!(
            "evernode node {name} runs ever-node commit {commit}, but this build reads commit {} \
             (ever-node {}). Build the tool for the node's commit: add --build-arg \
             EVER_NODE_REV={commit} to docker build, or run ./set-ever-node-rev.sh {commit} before \
             cargo build",
            env!("EVER_NODE_COMMIT"),
            env!("EVER_NODE_VERSION")
        );
    }

    let container = node["container_name"].as_str().or(node["name"].as_str()).unwrap_or(name);
    if container_is_running(container) == Some(true) {
        bail!("evernode node {name} is running (container {container}): stop it first with: {stop}");
    }
    Ok(node_dir.join("node_db"))
}

/// The ever-node commit an evernode_cli image was built from, from its image record.
fn evernode_node_commit(state_dir: &Path, image_id: &str) -> Result<String> {
    let images = state_dir.join("images");
    let entries = std::fs::read_dir(&images).with_context(|| format!("cannot read {}", images.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let record: serde_json::Value = match std::fs::read(&path).map(|d| serde_json::from_slice(&d)) {
            Ok(Ok(record)) => record,
            _ => continue,
        };
        if record["image_id"].as_str() == Some(image_id) {
            return record["inputs"]["node_commit"]
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("{} has no inputs.node_commit", path.display()));
        }
    }
    bail!("no image record in {} for image {image_id}", images.display())
}

/// Asks Docker whether a container is running. None when Docker can't tell (no docker command, or
/// no such container); the database lock still catches a running node.
fn container_is_running(container: &str) -> Option<bool> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", container])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Opens the database the way the node would, but never restores, checks or upgrades it.
/// `stop` tells the user how to stop the node if it is still running.
async fn open_db(path: &Path, stop: &str) -> Result<(InternalDb, MarkerGuard)> {
    // ever-node creates a new, empty database in a folder that has none; refuse instead.
    if !path.join("db").join("CURRENT").is_file() {
        bail!(
            "{} is not an ever-node database folder (it has no db/CURRENT). Give the folder named \
             by internal_db_path in the node's config, not the db folder inside it",
            path.display()
        );
    }
    check_owner(path)?;
    let marker = path.join(RUNNING_MARKER);
    let guard = MarkerGuard((!marker.exists()).then_some(marker));
    let db_directory = path.to_str().context("the database path is not valid UTF-8")?.to_owned();
    let db = InternalDb::with_update(
        InternalDbConfig { db_directory, ..Default::default() },
        false,
        false,
        false,
        &|| Ok(()),
        None,
        create_engine_telemetry(),
        create_engine_allocated(),
    )
    .await
    .map_err(|error| explain_open_error(error, stop))?;
    Ok((db, guard))
}

/// Files RocksDB creates while the tool has the database open belong to the user running it. If that
/// is not the database's owner, the node can fail to start later, so the tool runs as the owner only.
fn check_owner(path: &Path) -> Result<()> {
    let owner = std::fs::metadata(path.join("db").join("CURRENT"))?.uid();
    let me = current_uid();
    if owner != me {
        let how = match user_name(owner) {
            Some(name) => {
                format!("put `runuser -u {name} --` in front of the command; in Docker, add --user {owner}")
            }
            None => format!("in Docker, add --user {owner} to docker run"),
        };
        bail!(
            "{} belongs to {}, but the tool runs as {}. Run it as the database's owner ({how}), so the \
             files it creates keep the right owner",
            path.display(),
            user_label(owner),
            user_label(me)
        );
    }
    Ok(())
}

/// The user the tool runs as.
fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A user for messages: the name and uid, or the uid alone.
fn user_label(uid: u32) -> String {
    match user_name(uid) {
        Some(name) => format!("{name} (uid {uid})"),
        None => format!("uid {uid}"),
    }
}

/// The user name for a uid from /etc/passwd. None inside a container that doesn't know the host's
/// users.
fn user_name(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_owned())
    })
}

/// Turns the two errors a user can act on into plain instructions.
fn explain_open_error(error: anyhow::Error, stop: &str) -> anyhow::Error {
    let message = format!("{error:#}");
    if message.contains("lock file") {
        error.context(format!("the node is running (its database is locked): {stop}"))
    } else if message.contains("DB version") {
        error.context(format!(
            "this build reads database version {CURRENT_DB_VERSION} (ever-node {}). Build the tool \
             for the node's commit (ever-node --version shows it): add --build-arg \
             EVER_NODE_REV=<commit> to docker build, or run ./set-ever-node-rev.sh <commit> before \
             cargo build",
            env!("EVER_NODE_VERSION")
        ))
    } else {
        error
    }
}

/// Block N's masterchain state and the state of the basechain block it lists as latest.
struct Snapshot {
    mc_id: BlockIdExt,
    key_block: bool,
    mc_state: Arc<ShardStateStuff>,
    shard_id: BlockIdExt,
    shard_state: Arc<ShardStateStuff>,
}

fn load_snapshot(db: &InternalDb, mc_seqno: u32, refuse_key_block: bool) -> Result<Snapshot> {
    let handle = db
        .find_mc_block_by_seq_no(mc_seqno)
        .with_context(|| format!("masterchain block {mc_seqno} is not in this database"))?;
    let mc_id = handle.id().clone();
    let key_block = handle.is_key_block()?;
    if key_block && refuse_key_block {
        bail!(
            "masterchain block {mc_seqno} is a key block. A snapshot of the key block that installs \
             a new validator set breaks DePool bookkeeping on Tycho; export a later block"
        );
    }
    let mc_state = db.load_shard_state_dynamic(&mc_id).with_context(|| {
        format!(
            "the state of masterchain block {mc_seqno} is not in this database: the node has deleted \
             it already. `info` shows which blocks can be exported"
        )
    })?;

    let shard_id = single_basechain_block(&mc_state.top_blocks_all()?, mc_seqno)?.clone();
    let shard_state = db.load_shard_state_dynamic(&shard_id).with_context(|| {
        format!(
            "the state of basechain block {} (listed by masterchain block {mc_seqno}) is not in this \
             database: the node had not applied that basechain block yet when it stopped, or has \
             deleted its state since. `info` shows which blocks can be exported. If {mc_seqno} is newer \
             than that, the node stopped before catching up: run it again while the network is still \
             up, or use another node",
            shard_id.seq_no()
        )
    })?;
    Ok(Snapshot { mc_id, key_block, mc_state, shard_id, shard_state })
}

/// tycho-state-updater takes one basechain state, so the basechain must not be split into shards.
fn single_basechain_block(top_blocks: &[BlockIdExt], mc_seqno: u32) -> Result<&BlockIdExt> {
    match top_blocks {
        [block] if block.shard().workchain_id() == 0 => Ok(block),
        _ => bail!(
            "masterchain block {mc_seqno} lists {} shard blocks; tycho-state-updater takes exactly one \
             basechain shard",
            top_blocks.len()
        ),
    }
}

async fn export(args: ExportArgs) -> Result<()> {
    // The database checks come first and change nothing; then the output folder is checked before
    // the export itself, which takes minutes on mainnet.
    let (db, _marker, _) = open_database(&args.database).await?;
    let out = &args.out;
    let me = || user_label(current_uid());
    std::fs::create_dir_all(out)
        .with_context(|| format!("cannot create the output folder {} as {}", out.display(), me()))?;
    let probe = out.join(".write-test");
    std::fs::write(&probe, b"")
        .with_context(|| format!("cannot write to the output folder {} as {}", out.display(), me()))?;
    std::fs::remove_file(&probe)?;

    let snapshot = load_snapshot(&db, args.mc_seqno, !args.allow_key_block)?;
    let Snapshot { mc_id, key_block, mc_state, shard_id, shard_state } = snapshot;

    let write = |name: &str, data: Vec<u8>| {
        let path = out.join(name);
        std::fs::write(&path, data).with_context(|| format!("cannot write {}", path.display()))
    };
    write("masterstate.boc", write_boc(mc_state.root_cell())?)?;
    write("shardstate.boc", write_boc(shard_state.root_cell())?)?;

    let vset = mc_state.config_params()?.validator_set()?;
    let mut accounts = 0u64;
    shard_state.state()?.read_accounts()?.iterate_objects(|_| {
        accounts += 1;
        Ok(true)
    })?;
    let manifest = json!({
        "masterchain": {
            "block": id_json(&mc_id),
            "key_block": key_block,
            "file": "masterstate.boc",
            "state_root_hash": mc_state.root_cell().repr_hash().as_hex_string(),
            "gen_utime": mc_state.state()?.gen_time(),
            "global_id": mc_state.state()?.global_id(),
            "validator_set": { "utime_since": vset.utime_since(), "utime_until": vset.utime_until() },
        },
        "basechain": {
            "block": id_json(&shard_id),
            "file": "shardstate.boc",
            "state_root_hash": shard_state.root_cell().repr_hash().as_hex_string(),
            "gen_utime": shard_state.state()?.gen_time(),
            "accounts": accounts,
        },
        "exporter": VERSION,
    });
    write("manifest.json", serde_json::to_vec_pretty(&manifest)?)?;

    println!("masterchain block {} -> masterstate.boc", mc_id.seq_no());
    println!("basechain block {} -> shardstate.boc", shard_id.seq_no());
    Ok(())
}

async fn verify(args: VerifyArgs) -> Result<()> {
    let (db, _marker, _) = open_database(&args.database).await?;
    let snapshot = load_snapshot(&db, args.mc_seqno, false)?;
    if snapshot.key_block {
        println!("note: masterchain block {} is a key block", args.mc_seqno);
    }
    let checks = [
        ("masterchain", &args.master, &snapshot.mc_id, snapshot.mc_state.root_cell().repr_hash()),
        ("basechain", &args.shard, &snapshot.shard_id, snapshot.shard_state.root_cell().repr_hash()),
    ];
    let mut all_match = true;
    for (chain, file, block, expected) in checks {
        let data = std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
        let actual = read_single_root_boc(&data)
            .with_context(|| format!("{} is not a state file", file.display()))?
            .repr_hash();
        if actual == expected {
            println!("{chain}: {} matches block {}", file.display(), block.seq_no());
        } else {
            all_match = false;
            println!(
                "{chain}: {} does NOT match block {}: the file's root hash is {}, the block's state hash is {}",
                file.display(),
                block.seq_no(),
                actual.as_hex_string(),
                expected.as_hex_string()
            );
        }
    }
    if !all_match {
        bail!("the files do not match this node's database");
    }
    Ok(())
}

async fn info(args: InfoArgs) -> Result<()> {
    let (db, _marker, folder) = open_database(&args.database).await?;
    let last_id = db
        .load_full_node_state(LAST_APPLIED_MC_BLOCK)?
        .context("this database has no applied masterchain block")?;
    let last = db.load_block_handle(&last_id)?.context("cannot load the last applied block")?;
    let range = exportable_blocks(&db, last_id.seq_no());
    let basechain_done = db.load_full_node_state(SHARD_CLIENT_MC_BLOCK)?.map(|id| id.seq_no());

    println!("database folder: {}", folder.display());
    println!("database version: {} (this build reads version {CURRENT_DB_VERSION})", db.load_db_version()?);
    println!("this build: {VERSION}");
    println!(
        "last applied masterchain block: {} (time {}{})",
        last_id.seq_no(),
        last.gen_utime()?,
        if last.is_key_block()? { ", key block" } else { "" }
    );
    if let Some(done) = basechain_done {
        println!("basechain blocks applied up to masterchain block {done}");
    }
    match range {
        Some((oldest, newest)) => println!("blocks that can be exported: {oldest} to {newest}"),
        None => println!("blocks that can be exported: none"),
    }
    Ok(())
}

/// A block can be exported when the node keeps both its own state and the state of the basechain
/// block it lists. The node deletes old states, and a node that doesn't validate applies basechain
/// blocks somewhat after masterchain blocks, so the range can end below the last applied block.
fn exportable_blocks(db: &InternalDb, last: u32) -> Option<(u32, u32)> {
    let exportable = |seqno: u32| load_snapshot(db, seqno, false).is_ok();
    // Newest: step down from the last applied block in doubling steps until a block is exportable,
    // then narrow down between it and the last block that wasn't.
    let (mut found, mut above) = (None, last + 1);
    let mut step = 1u32;
    let mut probe = Some(last);
    while let Some(seqno) = probe {
        if exportable(seqno) {
            found = Some(seqno);
            break;
        }
        above = seqno;
        probe = seqno.checked_sub(step);
        step = step.saturating_mul(2);
    }
    let mut newest = found?;
    while newest + 1 < above {
        let middle = newest + (above - newest) / 2;
        if exportable(middle) { newest = middle } else { above = middle }
    }
    // Oldest: below the newest, exportable blocks are contiguous down to the oldest kept state.
    let (mut low, mut high) = (0, newest);
    while low < high {
        let middle = low + (high - low) / 2;
        if exportable(middle) { high = middle } else { low = middle + 1 }
    }
    Some((high, newest))
}

fn id_json(id: &BlockIdExt) -> serde_json::Value {
    json!({
        "workchain": id.shard().workchain_id(),
        "shard": format!("{:016x}", id.shard().shard_prefix_with_tag()),
        "seqno": id.seq_no(),
        "root_hash": id.root_hash().as_hex_string(),
        "file_hash": id.file_hash().as_hex_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ever_block::{ShardIdent, UInt256};

    fn block(workchain: i32, shard: u64) -> BlockIdExt {
        let shard = ShardIdent::with_tagged_prefix(workchain, shard).unwrap();
        BlockIdExt::with_params(shard, 1, UInt256::default(), UInt256::default())
    }

    fn parse(args: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("state-export").chain(args.split_whitespace()))
    }

    #[test]
    fn database_options_combine_as_documented() {
        assert!(parse("export --db /d --mc-seqno 1 --out /o").is_ok());
        assert!(parse("export --evernode n --mc-seqno 1 --out /o").is_ok());
        assert!(parse("export --evernode n --evernode-state-dir /s --mc-seqno 1 --out /o").is_ok());
        assert!(parse("info --evernode n --evernode-state-dir /s").is_ok());
        assert!(parse("export --db /d --evernode n --mc-seqno 1 --out /o").is_err());
        assert!(parse("export --mc-seqno 1 --out /o").is_err());
        assert!(parse("export --db /d --evernode-state-dir /s --mc-seqno 1 --out /o").is_err());
        assert!(parse("export --db /d --out /o").is_err());
    }

    #[test]
    fn one_basechain_shard_is_accepted() {
        let blocks = [block(0, 0x8000_0000_0000_0000)];
        assert!(single_basechain_block(&blocks, 10).is_ok());
    }

    #[test]
    fn split_basechain_is_refused() {
        let blocks = [block(0, 0x4000_0000_0000_0000), block(0, 0xc000_0000_0000_0000)];
        let error = single_basechain_block(&blocks, 10).unwrap_err().to_string();
        assert!(error.contains("lists 2 shard blocks"), "{error}");
    }

    #[test]
    fn other_workchain_is_refused() {
        let blocks = [block(1, 0x8000_0000_0000_0000)];
        assert!(single_basechain_block(&blocks, 10).is_err());
    }

    #[test]
    fn lock_error_says_to_stop_the_node() {
        let error = anyhow::anyhow!("IO error: While lock file: /db/db/LOCK: Resource temporarily unavailable");
        let message = explain_open_error(error, "stop the node first").to_string();
        assert!(message.contains("the node is running"), "{message}");
        assert!(message.contains("stop the node first"), "{message}");
    }

    #[test]
    fn version_error_names_the_build() {
        let error = anyhow::anyhow!("DB version 5 does not correspond to current supported one 6.");
        let message = explain_open_error(error, "stop the node first").to_string();
        assert!(message.contains("this build reads database version"), "{message}");
        assert!(message.contains("EVER_NODE_REV=<commit>"), "{message}");
    }
}
