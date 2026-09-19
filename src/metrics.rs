use crate::census::{Census, Cumulative, Histogram, Reach};
use crate::probe::Stage;

const PREFIX: &str = "cardano_census_";

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Textfile output with a fixed set of labels stamped on every series.
struct Out {
    text: String,
    base: Vec<(String, String)>,
}

impl Out {
    fn new(base: &[(String, String)]) -> Self {
        Self {
            text: String::new(),
            base: base.to_vec(),
        }
    }

    fn family(&mut self, name: &str, kind: &str, help: &str) {
        self.text
            .push_str(&format!("# HELP {PREFIX}{name} {help}\n# TYPE {PREFIX}{name} {kind}\n"));
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: &str) {
        self.text.push_str(PREFIX);
        self.text.push_str(name);
        // Base labels first; a series that sets the same key keeps its own.
        let mut all: Vec<(&str, &str)> = self
            .base
            .iter()
            .filter(|(k, _)| !labels.iter().any(|(lk, _)| lk == k))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        all.extend_from_slice(labels);
        if !all.is_empty() {
            self.text.push('{');
            for (i, (k, v)) in all.iter().enumerate() {
                if i > 0 {
                    self.text.push(',');
                }
                self.text.push_str(&format!("{k}=\"{}\"", escape(v)));
            }
            self.text.push('}');
        }
        self.text.push(' ');
        self.text.push_str(value);
        self.text.push('\n');
    }

    fn gauge(&mut self, name: &str, help: &str, value: &str) {
        self.family(name, "gauge", help);
        self.sample(name, &[], value);
    }

    fn histogram(&mut self, name: &str, help: &str, h: &Histogram) {
        self.family(name, "histogram", help);
        let bucket = format!("{name}_bucket");
        for (b, c) in h.bounds.iter().zip(&h.counts) {
            self.sample(&bucket, &[("le", &b.to_string())], &c.to_string());
        }
        self.sample(&bucket, &[("le", "+Inf")], &h.count.to_string());
        self.sample(&format!("{name}_sum"), &[], &num(h.sum));
        self.sample(&format!("{name}_count"), &[], &h.count.to_string());
    }

    /// A gauge family keyed by `le`, cumulative like histogram buckets but carrying weight.
    fn within(&mut self, name: &str, help: &str, c: &Cumulative) {
        self.family(name, "gauge", help);
        for (b, v) in c.bounds.iter().zip(&c.values) {
            self.sample(name, &[("le", &b.to_string())], &num(*v));
        }
        self.sample(name, &[("le", "+Inf")], &num(c.total));
    }

    fn build_info(&mut self) {
        self.family("build_info", "gauge", "Version and git revision of the cardano-census that wrote this");
        self.sample(
            "build_info",
            &[("version", crate::cli::VERSION), ("rev", crate::cli::GIT_REV)],
            "1",
        );
    }
}

/// Render the metrics, with `labels` stamped on every series.
pub fn render(c: &Census, labels: &[(String, String)]) -> String {
    let mut out = Out::new(labels);

    out.build_info();
    out.family("snapshot_info", "gauge", "Snapshot the census was taken from");
    out.sample(
        "snapshot_info",
        &[
            ("source", &c.snapshot_source),
            ("network_magic", &c.network_magic.to_string()),
            ("node_to_client_version", &c.node_to_client_version.to_string()),
        ],
        "1",
    );
    out.gauge("snapshot_slot", "Slot of the snapshot's ledger point", &c.snapshot_slot.to_string());
    out.gauge("blp_total", "Big ledger pools in the snapshot", &c.blp_total.to_string());
    out.gauge(
        "snapshot_stake_ratio",
        "Sum of relativeStake over the snapshot's pools",
        &num(c.snapshot_stake_ratio),
    );

    out.gauge("relays_total", "Relay entries in the snapshot", &c.relays_total.to_string());
    out.gauge(
        "relays_srv",
        "Relay entries that are SRV record names, each probed at its top-priority targets",
        &c.relays_srv.to_string(),
    );
    if !c.srv_sets.is_empty() {
        out.family("srv_targets", "gauge", "Top-priority targets behind each SRV relay name");
        for (name, set) in &c.srv_sets {
            out.sample("srv_targets", &[("name", name)], &set.targets.to_string());
        }
        out.family("srv_targets_reachable", "gauge", "Targets behind each SRV relay name that returned a tip");
        for (name, set) in &c.srv_sets {
            out.sample("srv_targets_reachable", &[("name", name)], &set.reachable.to_string());
        }
        out.family(
            "srv_reach_probability",
            "gauge",
            "Chance a node drawing a target of this SRV name by weight reaches one that answered",
        );
        for (name, set) in &c.srv_sets {
            out.sample("srv_reach_probability", &[("name", name)], &num(set.reach_probability));
        }
    }
    out.gauge(
        "srv_endpoints_total",
        "Distinct endpoints that are SRV targets",
        &c.srv_endpoints_total.to_string(),
    );
    out.gauge(
        "srv_endpoints_reachable",
        "Distinct SRV target endpoints that returned a tip",
        &c.srv_endpoints_reachable.to_string(),
    );
    out.gauge(
        "endpoints_total",
        "Distinct socket addresses after resolving and deduplicating the relay entries",
        &c.endpoints_total.to_string(),
    );
    out.gauge("endpoints_probed", "Endpoints that resolved and were probed", &c.endpoints_probed.to_string());

    out.family("relays_reachable", "gauge", "Relay entries that returned a tip, by address family that answered");
    out.sample("relays_reachable", &[("family", "v4")], &c.relays_reachable_v4.to_string());
    out.sample("relays_reachable", &[("family", "v6")], &c.relays_reachable_v6.to_string());

    if !c.n2n_versions.is_empty() {
        out.family("relays_n2n_version", "gauge", "Answering relay entries by the node-to-node protocol version they negotiated");
        for (v, g) in &c.n2n_versions {
            out.sample("relays_n2n_version", &[("version", v)], &g.relays.to_string());
        }
        out.family("stake_n2n_version", "gauge", "Stake behind answering relays by negotiated node-to-node version, each pool split over its relays");
        for (v, g) in &c.n2n_versions {
            out.sample("stake_n2n_version", &[("version", v)], &num(g.stake_ratio));
        }
    }

    out.family("relays_failed", "gauge", "Relay entries that returned no tip, by the stage that failed");
    for s in Stage::ALL {
        let n = c.relays_failed.get(s.label()).copied().unwrap_or(0);
        out.sample("relays_failed", &[("stage", s.label())], &n.to_string());
    }

    out.family("blp", "gauge", "Pools by how many of their relays answered: none, some, or all");
    for r in Reach::ALL {
        let g = &c.blp_by_reach[r.label()];
        out.sample("blp", &[("reach", r.label())], &g.pools.to_string());
    }
    out.family("blp_stake_ratio", "gauge", "Summed relativeStake of the pools in each reach class");
    for r in Reach::ALL {
        let g = &c.blp_by_reach[r.label()];
        out.sample("blp_stake_ratio", &[("reach", r.label())], &num(g.stake_ratio));
    }

    out.gauge(
        "reachable_stake_ratio",
        "Stake of pools with at least one relay answering",
        &num(c.reachable_stake_ratio),
    );
    out.gauge(
        "relays_ipv4_literal",
        "Relay entries whose address is an IPv4 literal, each also probed with its octets reversed",
        &c.ipv4.relays.to_string(),
    );
    out.family(
        "relays_ipv4_literal_reachable",
        "gauge",
        "IPv4 literal relay entries that returned a tip at the address as given, and at the address with its octets reversed",
    );
    out.sample("relays_ipv4_literal_reachable", &[("spelling", "given")], &c.ipv4.reachable_given.to_string());
    out.sample("relays_ipv4_literal_reachable", &[("spelling", "reversed")], &c.ipv4.reachable_reversed.to_string());
    out.gauge(
        "pools_ipv4_reversed_only",
        "Pools with no relay answering as given and one answering at its octet reversal",
        &c.ipv4.pools_reversed_only.to_string(),
    );
    out.gauge(
        "stake_ipv4_reversed_only_ratio",
        "Stake of pools with no relay answering as given and one answering at its octet reversal",
        &num(c.ipv4.stake_reversed_only_ratio),
    );
    out.gauge(
        "reachable_stake_ratio_with_reversed",
        "Stake of pools with a relay answering as given or at its octet reversal",
        &num(c.reachable_stake_ratio + c.ipv4.stake_reversed_only_ratio),
    );
    out.gauge(
        "relay_weighted_stake_ratio",
        "Stake weighted by the share of each pool's relays that answered, an SRV relay by the weight of its answering targets",
        &num(c.relay_weighted_stake_ratio),
    );
    out.within(
        "relays_reachable_within",
        "Relay entries that returned a tip within le seconds of the first DNS lookup",
        &c.relays_reachable_within,
    );
    out.within(
        "stake_reachable_within",
        "Stake of pools whose fastest relay returned a tip within le seconds",
        &c.stake_reachable_within,
    );
    out.histogram(
        "blp_relay_reachability",
        "Pools by the share of their relays that answered",
        &c.reachability,
    );
    out.histogram(
        "probe_rtt_seconds",
        "Connect through tip response for each endpoint that answered",
        &c.rtt_seconds,
    );

    out.gauge("tip_block_max", "Highest block number reported by any relay", &c.tip_block_max.to_string());
    out.gauge("tip_slot_max", "Highest slot reported by any relay", &c.tip_slot_max.to_string());
    out.gauge(
        "tips_at_max_block",
        "Distinct block hashes reported at the highest block; more than one is a fork or slot battle at the tip",
        &c.tips_at_max_block.to_string(),
    );
    out.gauge(
        "chains",
        "Tip groups after merging tips within fork_tolerance blocks of each other",
        &c.chains.len().to_string(),
    );
    out.gauge("fork_tolerance_blocks", "Block distance within which tips count as one chain", &c.fork_tolerance.to_string());
    let (main, other): (Vec<_>, Vec<_>) = c.chains.iter().partition(|g| g.main);
    let sum_relays = |gs: &[&crate::census::ChainGroup]| gs.iter().map(|g| g.relays).sum::<u64>();
    let sum_stake = |gs: &[&crate::census::ChainGroup]| gs.iter().map(|g| g.stake_ratio).sum::<f64>();
    out.family("chain_relays", "gauge", "Answering relay entries on the main chain group and on all others");
    out.sample("chain_relays", &[("chain", "main")], &sum_relays(&main).to_string());
    out.sample("chain_relays", &[("chain", "other")], &sum_relays(&other).to_string());
    out.family("chain_stake_ratio", "gauge", "Stake on the main chain group and on all others, each pool split over its answering relays");
    out.sample("chain_stake_ratio", &[("chain", "main")], &num(sum_stake(&main)));
    out.sample("chain_stake_ratio", &[("chain", "other")], &num(sum_stake(&other)));
    out.within(
        "relays_within_blocks_of_tip",
        "Answering relay entries whose tip is at most le blocks behind the highest",
        &c.relays_within_blocks_of_tip,
    );
    out.within(
        "stake_within_blocks_of_tip",
        "Stake of pools whose most current relay is at most le blocks behind the highest tip",
        &c.stake_within_blocks_of_tip,
    );

    out.gauge(
        "pools_indexed",
        "Pools the pool index named, 0 when no index was given",
        &c.pools_indexed.to_string(),
    );
    if !c.top_pools.is_empty() {
        out.family(
            "pool_stake_ratio",
            "gauge",
            "Operators with the most stake among pools not fully reachable, named for outreach; pools sharing an identity are one row, and pool_id falls back to the first relay without an index",
        );
        for p in &c.top_pools {
            let relays = p.relays.join(", ");
            out.sample(
                "pool_stake_ratio",
                &[
                    ("pool_id", &p.pool_id),
                    ("ticker", &p.ticker),
                    ("name", &p.name),
                    ("reach", p.reach.label()),
                    ("pools", &p.pools.to_string()),
                    ("relays_total", &p.relays_total.to_string()),
                    ("relays_reachable", &p.relays_reachable.to_string()),
                    ("relays", &relays),
                ],
                &num(p.stake_ratio),
            );
        }
    }
    out.gauge(
        "asn_db_ranges",
        "Ranges in the loaded ip2asn database, 0 when relays could not be placed in networks",
        &c.asn_db_ranges.to_string(),
    );
    if c.asn_db_ranges > 0 {
        out.gauge(
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
            out.family(name, "gauge", help);
            for g in &c.asn_metrics {
                out.sample(name, &[("asn", &g.asn), ("name", &g.name)], &value(g));
            }
        }
    }
    out.gauge(
        "scan_duration_seconds",
        "Wall time from first DNS lookup to last probe",
        &num(c.scan_duration_seconds),
    );
    out.gauge(
        "last_run_timestamp_seconds",
        "When this census finished",
        &c.timestamp_seconds.to_string(),
    );
    out.gauge("success", "1 if the census ran to completion", "1");
    out.text
}

/// Written in place of the metrics when the run could not complete.
pub fn render_failure(timestamp_seconds: u64, labels: &[(String, String)]) -> String {
    let mut out = Out::new(labels);
    out.build_info();
    out.gauge(
        "last_run_timestamp_seconds",
        "When this census finished",
        &timestamp_seconds.to_string(),
    );
    out.gauge("success", "1 if the census ran to completion", "0");
    out.text
}

/// Shortest representation after rounding away float noise beyond 9 decimals.
/// Adding 0.0 turns the negative zero an empty sum produces into a plain 0.
fn num(x: f64) -> String {
    ((x * 1e9).round() / 1e9 + 0.0).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn base_labels_stamp_every_series() {
        let text = render_failure(7, &labels(&[("environment", "preview"), ("group", "preview1")]));
        assert!(text.contains("cardano_census_success{environment=\"preview\",group=\"preview1\"} 0\n"));
        assert!(text.contains("cardano_census_last_run_timestamp_seconds{environment=\"preview\",group=\"preview1\"} 7\n"));
        assert!(text.contains("cardano_census_build_info{environment=\"preview\",group=\"preview1\",version=\""));
    }

    #[test]
    fn series_labels_win_over_base_labels() {
        let mut out = Out::new(&labels(&[("le", "base"), ("env", "x")]));
        out.sample("s", &[("le", "5")], "1");
        assert_eq!(out.text, "cardano_census_s{env=\"x\",le=\"5\"} 1\n");
    }

    #[test]
    fn no_labels_means_no_braces() {
        let mut out = Out::new(&[]);
        out.sample("s", &[], "1");
        assert_eq!(out.text, "cardano_census_s 1\n");
    }
}
