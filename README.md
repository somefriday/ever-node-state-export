# state-export

`state-export` reads a stopped ever-node's database and saves the state of one masterchain block,
plus the state of the basechain block it points to. `tycho-state-updater` builds Tycho's starting
state from these two files.

## Before you start

- **The block number `N`.** The coordinator announces it.
- **Docker** to build the tool, on the node's server or another machine (building without Docker
  works too).
- **Time with the node stopped.** The tool reads the node's database directly, which it can't do
  while the node runs. Each route below shows how to stop the node.
- **Room for the result**: about the size of the node's state (several GB on mainnet).
- **Memory.** About twice the size of the node's masterchain state: about 15 GB of free RAM on
  mainnet.

The commands below are for root, as validator servers are usually run. As another user, put `sudo` in
front of them, and of `docker` too unless you're in the `docker` group.

## 1. Build the tool

```sh
git clone https://github.com/somefriday/ever-node-state-export.git
cd ever-node-state-export
docker build --target binary --output . .
install state-export /usr/local/bin/
```

`state-export --version` shows the tool's version and the ever-node version it reads, for example
`state-export 0.2.0 (ever-node 0.60.11, commit e4068621a96d78969dd6df3087504d25b06fca6e)`. The tool
runs on Ubuntu 22.04 and 24.04 and on Debian 11, 12 and 13, on x86_64 and arm64.

For route 3, build the image too (in the same folder):

```sh
docker build -t state-export .
```

### For another ever-node version

The tool reads the node's database with ever-node's own code, so it must be built for your node's
ever-node version. The default is ever-node 0.60.11 (commit `e4068621`). For another version, add
`--build-arg EVER_NODE_REV=<commit>` to the `docker build` commands; `ever-node --version` shows your
node's commit ("EVER NODE git commit"). If you're not sure, build the default: the tool refuses a
database it can't read and says what to build.

### Without Docker

If the node's server has no Docker, build on another machine and copy the `state-export` file over.
When the two machines have different architectures, add `--platform linux/amd64` or
`--platform linux/arm64` to `docker build`.

Or build on the server itself:

```sh
apt update
apt install build-essential git curl pkg-config cmake clang libclang-dev libzstd-dev perl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y     # skip if rustup is installed
. "$HOME/.cargo/env"
git clone https://github.com/somefriday/ever-node-state-export.git
cd ever-node-state-export
cargo build --release
install target/release/state-export /usr/local/bin/
```

For another ever-node version, run `./set-ever-node-rev.sh <commit>` before `cargo build`.

## 2. Export

Follow the route that matches how you run your node.

### Route 1: node managed by evernode_cli

Stop the node. At the cutover every validator is in the current validator set, and evernode_cli
stops such a node only with `--force`:

```sh
evernode node stop -n <name> --force
```

Export:

```sh
state-export export --evernode <name> --mc-seqno <N> --out ./export
```

The tool finds the node's database and checks that the node runs the ever-node commit the tool was
built for. If evernode_cli keeps its data somewhere other than `/var/lib/ever-validator`, add
`--evernode-state-dir <dir>`.

### Route 2: database folder on this machine

For a node running as a service, or any database folder you can reach. Stop the node however you
run it.

The database folder is the one named by `internal_db_path` in the node's `config.json`. It contains
`db/`, `archive/`, `catchains/` and a few others; give that folder, not the `db/` inside it.

```sh
state-export export --db <rocksdb_folder> --mc-seqno <N> --out /tmp/export
```

The result goes to `/tmp/export` because the node's user can write there.

### Route 3: node in a Docker container

Stop the node container with `docker stop` (or `docker compose stop` in its compose folder). Don't
use `docker compose down`: it removes the container, and the export needs it.

```sh
docker stop <container_name>
```

Export, with the image from step 1:

```sh
docker run --rm --volumes-from <container_name> -v "$PWD/export:/out" \
    state-export export --db <container_rocksdb_path> --mc-seqno <N> --out /out
```

`--volumes-from` shows the tool the node container's files at the same paths, so give the database
path as the container sees it (its `internal_db_path`).

### While it runs

The tool says what it's doing, and for the long steps how far it has got and how long is left.

It starts from the newest persistent state the node keeps on disk: the node saves its whole state
every 36 hours or so. The tool reads that file in one pass and takes from the database only the cells
that changed since, so on mainnet the export takes minutes rather than hours. Each file is read back
and checked against the block's state hash once it is written. A node that keeps no persistent
states (`"skip_saving_persistent_states": true` in its config) makes the tool read every cell from
the database instead, which takes hours on mainnet. `--threads` sets how many threads read cells
from the database and hash them (the default is one per CPU, and at least 16).

If you stop an export with Ctrl-C, the database folder keeps the file `ton_node.running`, which tells
the node it crashed. If the node had stopped cleanly before the export (its log shows `Node stopped`
when you stopped it), delete that file before you start the node.

## 3. The result

The output folder holds:

- `masterstate.boc`: the masterchain state of block N;
- `shardstate.boc`: the basechain state of the block N points to;
- `manifest.json`: block ids and state hashes.

## Which blocks can I export?

Run `info` the same way as `export`, with the node stopped, but without `--mc-seqno` and `--out`.
For route 2:

```sh
state-export info --db <rocksdb_folder>
```

It shows the database version, the node's last block and the range of blocks it can export.

## If something goes wrong

### While building

| Message | What to do |
|---|---|
| `unknown flag: --output` or `failed to parse platform` | Your Docker builds without BuildKit. With the `docker.io` package of Ubuntu or Debian 13: `apt install docker-buildx`. With Debian 11's or 12's (Docker 20.10): put `DOCKER_BUILDKIT=1` in front of `docker build`. |

### Before the export starts

| Message | What to do |
|---|---|
| `the node is running (its database is locked)` or `evernode node ... is running` | Stop the node, as your route shows. |
| `... is not an ever-node database folder` | Give the folder named by `internal_db_path`, not the `db/` inside it. |
| `... belongs to ..., but the tool runs as ...` | Run the tool as the database's owner, as the message says (`runuser -u <user> --` in front of the command); files it created as another user could stop the node from starting later. In route 3, add `--user <uid>` to `docker run`, and first give the output folder to that user: `mkdir -p export && chown <uid> export`. |
| `cannot write to the output folder ... as ...` | Choose a folder that user can write to, or give the folder to that user with `chown`. |
| `docker: ... No such container` | Route 3: the node container is gone, for example after `docker compose down`. Mount the node's database volume instead: replace `--volumes-from <node container>` with `-v <volume>:/db` and give `--db /db` (`docker volume ls` lists the volumes). |
| `this build reads database version ...` or `... runs ever-node commit ...` | The tool was built for another ever-node version than your node runs. Rebuild it for your node's commit, as the message says (see [For another ever-node version](#for-another-ever-node-version)). |

### During the export

| Message | What to do |
|---|---|
| `cannot use the persistent state of ...` | The node's saved state file is damaged or incomplete. The tool goes on without it and reads every cell from the database, which takes hours on mainnet. |
| `cannot read key block ...` | The node no longer has the key block in its archive, so the tool can't tell which basechain persistent state to start from. It goes on and reads every basechain cell from the database, which can take an hour or more on mainnet. |
| `... read from the database hashes to ...: the database is damaged` | The node's database is damaged. Use another node. |
| `... was written wrong` | The file read back from the disk differs from what the tool wrote. Check the disk and export again. |

### About the block

| Message | What to do |
|---|---|
| `masterchain block N is a key block` | N is the block that switches validator sets, and a snapshot of it breaks DePool bookkeeping on Tycho. Check the number with the coordinator. |
| `masterchain block N is not in this database` | The node stopped before block N. Use a node that got further. |
| `the state of masterchain block N is not in this database` | The node has already deleted it. Use a node that stopped soon after block N; `info` shows which blocks a node can export. |
| `the state of basechain block ... is not in this database` | The node stopped before it applied the basechain block that N points to, or has deleted it since. If `info` ends before N, start the node again while the network is still up, let it catch up, stop it and export again. Otherwise use a node that stopped soon after block N. |
| `... lists N shard blocks` | The basechain was split into shards at block N. Tell the coordinator: the migration needs an unsplit basechain. |
