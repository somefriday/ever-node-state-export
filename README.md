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

## Build the tool

### Docker:

```sh
git clone https://github.com/somefriday/ever-node-state-export.git
cd ever-node-state-export
docker build --target binary --output . .
install state-export /usr/local/bin/
```


For another ever-node version, run `./set-ever-node-rev.sh <commit>` before `cargo build`.

## State Export

At this point you  need synced node that can be shut down. Use evernode_cli for create + dockerize one for simplicity.

----

### 1. Stop node

#### Native ever-node
```sh
systemctl stop <ever_node_service>
```

#### Dockerized node
```sh
docker stop --time 30 <container_name>
```

#### Evernode cli
```sh
evernode node stop -n <name> --force
```

wait until node is fulli stopped.

----

### 2. Confirm the block you need is available

#### For rocksdb folder path:
```sh
state-export info --db <rocksdb_folder>
```

#### For evernode cli provisioned node:
```sh
state-export info --evernode validator02
```

#### Output example:
```sh
state-export info --evernode validator02
database folder: /var/lib/ever-validator/nodes/validator02/node_db
database version: 6 (this build reads version 6)
this build: 0.3.0 (ever-node 0.60.11, commit e4068621a96d78969dd6df3087504d25b06fca6e)
last applied masterchain block: 63402596 (time 1793648718)
basechain blocks applied up to masterchain block 63402595
```

----

### 3. Export:

### For rocksdb folder path:

```sh
state-export export --db <rocksdb_folder> --mc-seqno <N> --out /tmp/export
```
The database folder is the one named by `internal_db_path` in the node's `config.json`. It contains
`db/`, `archive/`, `catchains/` and a few others; give that folder, not the `db/` inside it.

### For evernode cli provisioned node:
```sh
state-export export --evernode <name> --mc-seqno <N> --out ./export
```
The tool finds the node's database and checks that the node runs the ever-node commit the tool was
built for. If evernode_cli keeps its data somewhere other than `/var/lib/ever-validator`, add
`--evernode-state-dir <dir>`.


### For node in docker:

Stop the node container with `docker stop` (or `docker compose stop` in its compose folder). Don't
use `docker compose down`: it removes the container, and the export needs it.

```sh
docker run --rm --volumes-from <container_name> -v "$PWD/export:/out" \
    state-export export --db <container_rocksdb_path> --mc-seqno <N> --out /out
```

`--volumes-from` shows the tool the node container's files at the same paths, so give the database
path as the container sees it (its `internal_db_path`).

The result goes to `/tmp/export` because the node's user can write there.

----


## The result

The output folder holds:

- `masterstate.boc`: the masterchain state of block N;
- `shardstate.boc`: the basechain state of the block N points to;
- `manifest.json`: block ids and state hashes.


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
