# state-export

`state-export` reads a stopped ever-node's database and saves the state of one masterchain block,
plus the state of the basechain block it points to. `tycho-state-updater` builds Tycho's starting
state from these two files.


## To start you need

- **The block number `N`.** The coordinator announces it.
- **Synced node.** Node that you are ready to stop. You can prepare a new one with [evernode cli](https://github.com/somefriday/evernode_cli)
- **Right time to stop the node.** Node should be stopped after round starts, at least two blocks ahead the end of elections. Align stop with required mc seq_no
- **Storage**: SDD with about 30 GB of free space
- **Memory.** about 16 GB of free RAM

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

wait until node is fully stopped.

----

### 2. Confirm the block you need is available

#### For rocksdb folder path:
```sh
state-export info --db <rocksdb_folder>
```

#### For evernode cli provisioned node:
```sh
state-export info --evernode <node_name>
```

#### Output example:
```sh
state-export info --evernode <node_name>
database folder: /var/lib/ever-validator/nodes/validator02/node_db
database version: 6 (this build reads version 6)
this build: 0.3.1 (ever-node 0.60.11, commit e4068621a96d78969dd6df3087504d25b06fca6e)
last applied masterchain block: <mc_seq_no> (time 1793648718)
basechain blocks applied up to masterchain block 63402595
newest saved masterchain state: block <mc_seq_no>
blocks that can be exported: <oldest> to <newest>
```

N must be within the range on the last line. The node saves block states in the background, so when
it stops, the states of its last blocks may not be saved yet (`newest saved masterchain state` is
then below the last applied block). The tool builds those states from the newest saved one and the
blocks after it, as the node does when it starts again.

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
state-export export --evernode <node_name> --mc-seqno <N> --out ./export
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
