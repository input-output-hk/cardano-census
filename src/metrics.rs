use crate::census::{Census, Cumulative, Histogram, Reach};
use crate::probe::Stage;

const PREFIX: &str = "cardano_census_";

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    out.push_str(&format!("# HELP {PREFIX}{name} {help}\n# TYPE {PREFIX}{name} {kind}\n"));
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: &str) {
    out.push_str(PREFIX);
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (i, (k, v)) in labels.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{k}=\"{}\"", escape(v)));
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

fn gauge(out: &mut String, name: &str, help: &str, value: &str) {
    family(out, name, "gauge", help);
    sample(out, name, &[], value);
}

fn histogram(out: &mut String, name: &str, help: &str, h: &Histogram) {
    family(out, name, "histogram", help);
    let bucket = format!("{name}_bucket");
    for (b, c) in h.bounds.iter().zip(&h.counts) {
        sample(out, &bucket, &[("le", &b.to_string())], &c.to_string());
    }
    sample(out, &bucket, &[("le", "+Inf")], &h.count.to_string());
    sample(out, &format!("{name}_sum"), &[], &num(h.sum));
    sample(out, &format!("{name}_count"), &[], &h.count.to_string());
}

/// A gauge family keyed by `le`, cumulative like histogram buckets but carrying weight.
fn within(out: &mut String, name: &str, help: &str, c: &Cumulative) {
    family(out, name, "gauge", help);
    for (b, v) in c.bounds.iter().zip(&c.values) {
        sample(out, name, &[("le", &b.to_string())], &num(*v));
    }
    sample(out, name, &[("le", "+Inf")], &num(c.total));
}

fn build_info(out: &mut String) {
    family(out, "build_info", "gauge", "Version and git revision of the cardano-census that wrote this");
    sample(
        out,
        "build_info",
        &[("version", crate::cli::VERSION), ("rev", crate::cli::GIT_REV)],
        "1",
    );
}

pub fn render(c: &Census) -> String {
    let mut out = String::new();

    build_info(&mut out);
    family(&mut out, "snapshot_info", "gauge", "Snapshot the census was taken from");
    sample(
        &mut out,
        "snapshot_info",
        &[
            ("source", &c.snapshot_source),
            ("network_magic", &c.network_magic.to_string()),
            ("node_to_client_version", &c.node_to_client_version.to_string()),
        ],
        "1",
    );
    gauge(&mut out, "snapshot_slot", "Slot of the snapshot's ledger point", &c.snapshot_slot.to_string());
    gauge(&mut out, "blp_total", "Big ledger pools in the snapshot", &c.blp_total.to_string());
    gauge(
        &mut out,
        "snapshot_stake_ratio",
        "Sum of relativeStake over the snapshot's pools",
        &num(c.snapshot_stake_ratio),
    );

    gauge(&mut out, "relays_total", "Relay entries in the snapshot", &c.relays_total.to_string());
    gauge(
        &mut out,
        "relays_srv",
        "Relay entries that are SRV record names, each probed at its top-priority targets",
        &c.relays_srv.to_string(),
    );
    if !c.srv_sets.is_empty() {
        family(&mut out, "srv_targets", "gauge", "Top-priority targets behind each SRV relay name");
        for (name, set) in &c.srv_sets {
            sample(&mut out, "srv_targets", &[("name", name)], &set.targets.to_string());
        }
        family(&mut out, "srv_targets_reachable", "gauge", "Targets behind each SRV relay name that returned a tip");
        for (name, set) in &c.srv_sets {
            sample(&mut out, "srv_targets_reachable", &[("name", name)], &set.reachable.to_string());
        }
    }
    gauge(
        &mut out,
        "srv_endpoints_total",
        "Distinct endpoints that are SRV targets",
        &c.srv_endpoints_total.to_string(),
    );
    gauge(
        &mut out,
        "srv_endpoints_reachable",
        "Distinct SRV target endpoints that returned a tip",
        &c.srv_endpoints_reachable.to_string(),
    );
    gauge(
        &mut out,
        "endpoints_total",
        "Distinct socket addresses after resolving and deduplicating the relay entries",
        &c.endpoints_total.to_string(),
    );
    gauge(&mut out, "endpoints_probed", "Endpoints that resolved and were probed", &c.endpoints_probed.to_string());

    family(&mut out, "relays_reachable", "gauge", "Relay entries that returned a tip, by address family that answered");
    sample(&mut out, "relays_reachable", &[("family", "v4")], &c.relays_reachable_v4.to_string());
    sample(&mut out, "relays_reachable", &[("family", "v6")], &c.relays_reachable_v6.to_string());

    family(&mut out, "relays_failed", "gauge", "Relay entries that returned no tip, by the stage that failed");
    for s in Stage::ALL {
        let n = c.relays_failed.get(s.label()).copied().unwrap_or(0);
        sample(&mut out, "relays_failed", &[("stage", s.label())], &n.to_string());
    }

    family(&mut out, "blp", "gauge", "Pools by how many of their relays answered: none, some, or all");
    for r in Reach::ALL {
        let g = &c.blp_by_reach[r.label()];
        sample(&mut out, "blp", &[("reach", r.label())], &g.pools.to_string());
    }
    family(&mut out, "blp_stake_ratio", "gauge", "Summed relativeStake of the pools in each reach class");
    for r in Reach::ALL {
        let g = &c.blp_by_reach[r.label()];
        sample(&mut out, "blp_stake_ratio", &[("reach", r.label())], &num(g.stake_ratio));
    }

    gauge(
        &mut out,
        "reachable_stake_ratio",
        "Stake of pools with at least one relay answering",
        &num(c.reachable_stake_ratio),
    );
    gauge(
        &mut out,
        "relay_weighted_stake_ratio",
        "Stake weighted by the share of each pool's relays that answered",
        &num(c.relay_weighted_stake_ratio),
    );
    within(
        &mut out,
        "relays_reachable_within",
        "Relay entries that returned a tip within le seconds of the first DNS lookup",
        &c.relays_reachable_within,
    );
    within(
        &mut out,
        "stake_reachable_within",
        "Stake of pools whose fastest relay returned a tip within le seconds",
        &c.stake_reachable_within,
    );
    histogram(
        &mut out,
        "blp_relay_reachability",
        "Pools by the share of their relays that answered",
        &c.reachability,
    );
    histogram(
        &mut out,
        "probe_rtt_seconds",
        "Connect through tip response for each endpoint that answered",
        &c.rtt_seconds,
    );

    gauge(&mut out, "tip_block_max", "Highest block number reported by any relay", &c.tip_block_max.to_string());
    gauge(&mut out, "tip_slot_max", "Highest slot reported by any relay", &c.tip_slot_max.to_string());
    gauge(
        &mut out,
        "tips_at_max_block",
        "Distinct block hashes reported at the highest block; more than one is a fork or slot battle at the tip",
        &c.tips_at_max_block.to_string(),
    );
    gauge(
        &mut out,
        "chains",
        "Tip groups after merging tips within fork_tolerance blocks of each other",
        &c.chains.len().to_string(),
    );
    gauge(&mut out, "fork_tolerance_blocks", "Block distance within which tips count as one chain", &c.fork_tolerance.to_string());
    let (main, other): (Vec<_>, Vec<_>) = c.chains.iter().partition(|g| g.main);
    let sum_relays = |gs: &[&crate::census::ChainGroup]| gs.iter().map(|g| g.relays).sum::<u64>();
    let sum_stake = |gs: &[&crate::census::ChainGroup]| gs.iter().map(|g| g.stake_ratio).sum::<f64>();
    family(&mut out, "chain_relays", "gauge", "Answering relay entries on the main chain group and on all others");
    sample(&mut out, "chain_relays", &[("chain", "main")], &sum_relays(&main).to_string());
    sample(&mut out, "chain_relays", &[("chain", "other")], &sum_relays(&other).to_string());
    family(&mut out, "chain_stake_ratio", "gauge", "Stake on the main chain group and on all others, each pool split over its answering relays");
    sample(&mut out, "chain_stake_ratio", &[("chain", "main")], &num(sum_stake(&main)));
    sample(&mut out, "chain_stake_ratio", &[("chain", "other")], &num(sum_stake(&other)));
    within(
        &mut out,
        "relays_within_blocks_of_tip",
        "Answering relay entries whose tip is at most le blocks behind the highest",
        &c.relays_within_blocks_of_tip,
    );
    within(
        &mut out,
        "stake_within_blocks_of_tip",
        "Stake of pools whose most current relay is at most le blocks behind the highest tip",
        &c.stake_within_blocks_of_tip,
    );

    gauge(
        &mut out,
        "asn_db_ranges",
        "Ranges in the loaded ip2asn database, 0 when relays could not be placed in networks",
        &c.asn_db_ranges.to_string(),
    );
    if c.asn_db_ranges > 0 {
        gauge(
            &mut out,
            "asn_min_relays",
            "Autonomous systems hosting fewer relays than this are folded into other",
            &c.asn_min_relays.to_string(),
        );
        type AsnValue = fn(&crate::census::AsnGroup) -> String;
        let series: [(&str, &str, AsnValue); 4] = [
            ("asn_relays", "Relay entries hosted in each autonomous system", |g| g.relays.to_string()),
            ("asn_relays_reachable", "Relay entries in each autonomous system that returned a tip", |g| g.relays_reachable.to_string()),
            ("asn_stake_ratio", "Stake hosted in each autonomous system, each pool split over its relays", |g| num(g.stake_ratio)),
            ("asn_stake_reachable_ratio", "Stake in each autonomous system whose relays returned a tip", |g| num(g.stake_reachable_ratio)),
        ];
        for (name, help, value) in series {
            family(&mut out, name, "gauge", help);
            for g in &c.asn_metrics {
                sample(&mut out, name, &[("asn", &g.asn), ("name", &g.name)], &value(g));
            }
        }
    }
    gauge(
        &mut out,
        "scan_duration_seconds",
        "Wall time from first DNS lookup to last probe",
        &num(c.scan_duration_seconds),
    );
    gauge(
        &mut out,
        "last_run_timestamp_seconds",
        "When this census finished",
        &c.timestamp_seconds.to_string(),
    );
    gauge(&mut out, "success", "1 if the census ran to completion", "1");
    out
}

/// Written in place of the metrics when the run could not complete.
pub fn render_failure(timestamp_seconds: u64) -> String {
    let mut out = String::new();
    build_info(&mut out);
    gauge(
        &mut out,
        "last_run_timestamp_seconds",
        "When this census finished",
        &timestamp_seconds.to_string(),
    );
    gauge(&mut out, "success", "1 if the census ran to completion", "0");
    out
}

/// Shortest representation after rounding away float noise beyond 9 decimals.
/// Adding 0.0 turns the negative zero an empty sum produces into a plain 0.
fn num(x: f64) -> String {
    ((x * 1e9).round() / 1e9 + 0.0).to_string()
}
