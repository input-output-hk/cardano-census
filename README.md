# cardano-census

Probes every relay in a cardano-node peer snapshot once and reports how much of the network answered, weighted by stake. Written for a systemd timer and the node exporter textfile collector, so nothing stays resident.

```
cardano-census --snapshot peer-snapshot.json --output -
cardano-census --snapshot peer-snapshot.json --output /var/lib/cardano-census/cardano-census.prom --report /var/lib/cardano-census/report.json
```

The snapshot is the `peerSnapshotV3` file cardano-node 11.x reads for its big ledger peers. Its `NetworkMagic` picks the network unless `--network-magic` overrides it.

## What counts as reachable

A relay is reachable when it completes the node-to-node handshake and answers one ChainSync `FindIntersect` with its tip, within the timeout. Nothing is fetched beyond the tip, and the tip is taken as the relay reports it.

The timeout applies twice: once to DNS plus connect, then again to handshake plus chainsync. A relay that connects slowly and then stalls can take close to twice the setting before it is counted as failed.

Relay entries that resolve to the same socket address are probed once and all take that result. Metrics that say `relays` count snapshot entries. Metrics that say `endpoints` count distinct addresses probed.

Stake is the snapshot's `relativeStake`, a share of total ledger stake taken from the pool distribution the node uses for the current epoch. It is fixed at the epoch boundary and does not follow live delegation. The snapshot stops adding pools once their stake reaches 90%, so `cardano_census_snapshot_stake_ratio` reads about 0.9 on every well formed snapshot.

## Metrics

All gauges, since each run is a fresh observation. Ratios are 0 to 1.

| Metric | Meaning |
| --- | --- |
| `cardano_census_snapshot_info{file,network_magic,node_to_client_version}` | Snapshot the census was taken from |
| `cardano_census_snapshot_slot` | Slot of the snapshot's ledger point |
| `cardano_census_blp_total` | Big ledger pools in the snapshot |
| `cardano_census_snapshot_stake_ratio` | Sum of `relativeStake` over the snapshot's pools |
| `cardano_census_relays_total` | Relay entries in the snapshot |
| `cardano_census_endpoints_total` | Distinct socket addresses after resolving and deduplicating |
| `cardano_census_endpoints_probed` | Endpoints that resolved and were probed |
| `cardano_census_relays_reachable{family="v4"\|"v6"}` | Relay entries that returned a tip, by the address family that answered |
| `cardano_census_relays_failed{stage="dns"\|"connect"\|"handshake"\|"chainsync"}` | Relay entries that returned no tip, by the stage that failed |
| `cardano_census_blp{reach="none"\|"partial"\|"full"}` | Pools with none, some, or all of their relays answering |
| `cardano_census_blp_stake_ratio{reach=...}` | Summed `relativeStake` of the pools in each reach class |
| `cardano_census_reachable_stake_ratio` | Stake of pools with at least one relay answering. `partial` plus `full` |
| `cardano_census_relay_weighted_stake_ratio` | Stake weighted by the share of each pool's relays that answered |
| `cardano_census_blp_relay_reachability` | Histogram of pools by the share of their relays that answered. Buckets `0`, `0.25`, `0.5`, `0.75`, `1` |
| `cardano_census_probe_rtt_seconds` | Histogram of connect through tip response per answering endpoint. Buckets `0.05` to `10` |
| `cardano_census_tip_block_max`, `cardano_census_tip_slot_max` | Highest block and slot any relay reported |
| `cardano_census_scan_duration_seconds` | Wall time from first DNS lookup to last probe |
| `cardano_census_last_run_timestamp_seconds` | When the run finished |
| `cardano_census_success` | 1 when the run completed, 0 when it could not |

The gap between `reachable_stake_ratio` and `relay_weighted_stake_ratio` is how much of the reachable stake is hanging on partial relay sets. Equal values mean every pool is either fully up or fully down.

When the run fails before probing, for example an unreadable snapshot, the metrics file is replaced with just `success 0` and the timestamp, and the process exits non-zero.

## Report

`--report` writes JSON with the same summary plus one record per relay entry: the endpoint it was probed as, which address family answered, the negotiated N2N version, the tip, the round trip, or the stage and error it failed at.

## NixOS

```nix
{
  imports = [cardano-census.nixosModules.default];

  services.cardano-census = {
    enable = true;
    snapshotFile = "/var/lib/cardano-node/peer-snapshot.json";
  };

  services.prometheus.exporters.node.extraFlags = [
    "--collector.textfile.directory=/var/lib/cardano-census"
  ];
}
```

The module runs the census every 15 minutes as a oneshot service under a dynamic user and writes to `/var/lib/cardano-census`. `interval`, `timeout`, `parallel`, `reportFile`, `textfileDirectory` and `extraArgs` are options.

## Building

```
nix build
nix develop -c cargo build
```

The nix build is a static musl binary.
