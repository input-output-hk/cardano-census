# cardano-census

Probes every relay in a cardano-node peer snapshot once and reports how much of the network answered, weighted by stake. Written for a systemd timer and the node exporter textfile collector, so nothing stays resident.

```
cardano-census --snapshot peer-snapshot.json --output -
cardano-census --node-socket /run/cardano-node/node.socket --network-magic mainnet --output -
cardano-census --snapshot peer-snapshot.json --output /var/lib/cardano-census/cardano-census.prom --report /var/lib/cardano-census/report.json
```

The snapshot is the `peerSnapshotV3` file cardano-node 11.x reads for its big ledger peers. Its `NetworkMagic` picks the network unless `--network-magic` overrides it.

Progress goes to stderr, one line per phase, so a journal shows what a run is waiting on. A full mainnet run takes about a minute at the defaults.

With `--node-socket` the snapshot is asked of a local cardano-node instead, over the node-to-client socket, so each run sees the pools the node itself would use. The socket carries no network identity, so `--network-magic` is required there. It takes a number or `mainnet`. There is no remote form of this query.

## What counts as reachable

A relay is reachable when it completes the node-to-node handshake and answers one ChainSync `FindIntersect` with its tip, within the timeout. Nothing is fetched beyond the tip, and the tip is taken as the relay reports it.

The timeout is one budget per relay covering DNS, connect, handshake and the tip together, 60 seconds by default. It is deliberately long so that slow relays land in the `*_reachable_within` buckets below instead of being counted as failed. Pick the bucket that matches your own definition of reachable; `le="10"` is a common one.

Relay entries that resolve to the same socket address are probed once and all take that result. Metrics that say `relays` count snapshot entries. Metrics that say `endpoints` count distinct addresses probed.

A relay entry without a port is an SRV record name, which is how the ledger registers a relay by service record. It is looked up as SRV, every target at the lowest priority value is probed, and the entry counts as reachable if any of them answers. A node picks one of those targets by weight, so this is the union of what a node might reach. An entry whose SRV lookup fails or returns nothing fails at the `srv` stage.

Stake is the snapshot's `relativeStake`, a share of total ledger stake taken from the pool distribution the node uses for the current epoch. It is fixed at the epoch boundary and does not follow live delegation. The snapshot stops adding pools once their stake reaches 90%, so `cardano_census_snapshot_stake_ratio` reads about 0.9 on every well formed snapshot.

## Metrics

All gauges, since each run is a fresh observation. Ratios are 0 to 1.

| Metric | Meaning |
| --- | --- |
| `cardano_census_snapshot_info{source,network_magic,node_to_client_version}` | Snapshot the census was taken from. `source` is the file or the node socket |
| `cardano_census_snapshot_slot` | Slot of the snapshot's ledger point |
| `cardano_census_blp_total` | Big ledger pools in the snapshot |
| `cardano_census_snapshot_stake_ratio` | Sum of `relativeStake` over the snapshot's pools |
| `cardano_census_relays_total` | Relay entries in the snapshot |
| `cardano_census_relays_srv` | Relay entries that are SRV record names |
| `cardano_census_endpoints_total` | Distinct socket addresses after resolving and deduplicating, SRV targets included |
| `cardano_census_endpoints_probed` | Endpoints that resolved and were probed |
| `cardano_census_relays_reachable{family="v4"\|"v6"}` | Relay entries that returned a tip, by the address family that answered |
| `cardano_census_relays_failed{stage="srv"\|"dns"\|"connect"\|"handshake"\|"chainsync"}` | Relay entries that returned no tip, by the stage that failed |
| `cardano_census_blp{reach="none"\|"partial"\|"full"}` | Pools with none, some, or all of their relays answering |
| `cardano_census_blp_stake_ratio{reach=...}` | Summed `relativeStake` of the pools in each reach class |
| `cardano_census_reachable_stake_ratio` | Stake of pools with at least one relay answering. `partial` plus `full` |
| `cardano_census_relay_weighted_stake_ratio` | Stake weighted by the share of each pool's relays that answered |
| `cardano_census_relays_reachable_within{le}` | Relay entries that returned a tip within `le` seconds. Cumulative, `le` in `1 2.5 5 10 15 20 30 45 60 +Inf` |
| `cardano_census_stake_reachable_within{le}` | Stake of pools whose fastest relay returned a tip within `le` seconds. Same buckets, `+Inf` equals `reachable_stake_ratio` |
| `cardano_census_blp_relay_reachability` | Histogram of pools by the share of their relays that answered. Buckets `0`, `0.25`, `0.5`, `0.75`, `1` |
| `cardano_census_probe_rtt_seconds` | Histogram of connect through tip response per answering endpoint. Buckets `0.05` to `60` |
| `cardano_census_tip_block_max`, `cardano_census_tip_slot_max` | Highest block and slot any relay reported |
| `cardano_census_scan_duration_seconds` | Wall time from first DNS lookup to last probe |
| `cardano_census_last_run_timestamp_seconds` | When the run finished |
| `cardano_census_success` | 1 when the run completed, 0 when it could not |

The gap between `reachable_stake_ratio` and `relay_weighted_stake_ratio` is how much of the reachable stake is hanging on partial relay sets. Equal values mean every pool is either fully up or fully down. The spread of `stake_reachable_within` across its buckets shows how much stake is reachable only slowly.

When the run fails before probing, for example an unreadable snapshot, the metrics file is replaced with just `success 0` and the timestamp, and the process exits non-zero.

## Report

`--report` writes JSON with the same summary plus one record per relay entry: the endpoints it was probed at, which address family answered, the negotiated N2N version, the tip, the round trip, or the stage and error it failed at. An SRV entry lists every target endpoint and reports its best one.

## NixOS

```nix
{
  imports = [cardano-census.nixosModules.default];

  services.cardano-census = {
    enable = true;
    nodeSocket = "/run/cardano-node/node.socket";
    networkMagic = 764824073;
  };

  services.prometheus.exporters.node.extraFlags = [
    "--collector.textfile.directory=/var/lib/cardano-census"
  ];
}
```

Set `snapshotFile` instead of `nodeSocket` to read a file. With a socket the service joins `nodeSocketGroup`, `cardano-node` by default, to reach it.

The module runs the census every 15 minutes as a oneshot service under a dynamic user and writes to `/var/lib/cardano-census`. `interval`, `timeout`, `parallel`, `reportFile`, `textfileDirectory` and `extraArgs` are options.

## Building

```
nix build
nix develop -c cargo build
nix flake check
```

The nix build is a static musl binary. `nix flake check` runs clippy and the unit tests. Hydra builds `hydraJobs.required`, an aggregate of the package, both checks and the dev shell.
