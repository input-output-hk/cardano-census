{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.cardano-census;
  inherit (lib) mkEnableOption mkIf mkOption optional optionals types;

  metricsFile = "${cfg.textfileDirectory}/cardano-census.prom";
  fromNode = cfg.nodeSocket != null;

  asnFile = "/var/lib/cardano-census/ip2asn.tsv";
  refreshAsnDatabase = pkgs.writeShellScript "cardano-census-refresh-asn-db" ''
    set -eu
    db="$STATE_DIRECTORY/ip2asn.tsv"
    gz="$STATE_DIRECTORY/ip2asn-combined.tsv.gz"
    if [ -s "$db" ] && [ -z "$(find "$db" -mmin +${toString (cfg.asnDatabase.maxAgeHours * 60)})" ]; then
      exit 0
    fi
    if [ -e "$gz" ]; then
      curl -sSfL --max-time 120 -z "$gz" -o "$gz" ${lib.escapeShellArg cfg.asnDatabase.url}
    else
      curl -sSfL --max-time 120 -o "$gz" ${lib.escapeShellArg cfg.asnDatabase.url}
    fi
    gzip -dc "$gz" > "$db.tmp"
    mv "$db.tmp" "$db"
  '';
in {
  options.services.cardano-census = {
    enable = mkEnableOption "a timed reachability census of the Cardano big ledger peers";

    package = mkOption {
      type = types.package;
      description = "The cardano-census package to run.";
    };

    snapshotFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/var/lib/cardano-node/peer-snapshot.json";
      description = "peerSnapshotV3 file naming the pools and relays to probe. Set this or nodeSocket.";
    };

    nodeSocket = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/cardano-node/node.socket";
      description = "Query the snapshot from this local cardano-node socket instead of a file. Needs networkMagic.";
    };

    networkMagic = mkOption {
      type = types.nullOr types.ints.unsigned;
      default = null;
      example = 764824073;
      description = "Network magic for the node handshake. Taken from the snapshot file when null.";
    };

    nodeSocketGroup = mkOption {
      type = types.str;
      default = "cardano-node";
      description = "Group with write access to the node socket; the service joins it when nodeSocket is set.";
    };

    textfileDirectory = mkOption {
      type = types.str;
      default = "/var/lib/cardano-census";
      description = ''
        Directory the metrics file `cardano-census.prom` is written to.
        Point the node exporter textfile collector at it.
      '';
    };

    reportFile = mkOption {
      type = types.nullOr types.str;
      default = "/var/lib/cardano-census/report.json";
      description = "Per-relay JSON report from the latest run, or null to skip it.";
    };

    interval = mkOption {
      type = types.str;
      default = "15min";
      description = "Time between runs, as a systemd OnUnitActiveSec span.";
    };

    timeout = mkOption {
      type = types.ints.positive;
      default = 60;
      description = "Seconds allowed per relay for DNS, connect, handshake and tip together.";
    };

    parallel = mkOption {
      type = types.ints.positive;
      default = 128;
      description = "Relays probed at once.";
    };

    forkTolerance = mkOption {
      type = types.ints.unsigned;
      default = 10;
      description = "Tips this many blocks apart or closer count as the same chain.";
    };

    asnDatabase = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Keep a copy of iptoasn.com's ip2asn database so relays are placed in autonomous systems.";
      };

      url = mkOption {
        type = types.str;
        default = "https://iptoasn.com/data/ip2asn-combined.tsv.gz";
        description = "Where the gzipped ip2asn TSV is fetched from.";
      };

      maxAgeHours = mkOption {
        type = types.ints.positive;
        default = 24;
        description = "Check for a newer database when the local copy is older than this.";
      };

      minRelays = mkOption {
        type = types.ints.unsigned;
        default = 3;
        description = "Autonomous systems hosting fewer relays than this are folded into one `other` series.";
      };
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      example = ["--prefer-ipv4"];
      description = "Further command line arguments.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = (cfg.snapshotFile != null) != fromNode;
        message = "services.cardano-census: set exactly one of snapshotFile or nodeSocket";
      }
      {
        assertion = !fromNode || cfg.networkMagic != null;
        message = "services.cardano-census: nodeSocket needs networkMagic";
      }
    ];

    systemd.services.cardano-census = {
      description = "Reachability census of the Cardano big ledger peers";
      after = ["network-online.target"];
      wants = ["network-online.target"];
      path = optionals cfg.asnDatabase.enable (with pkgs; [coreutils curl findutils gzip]);
      environment = lib.optionalAttrs cfg.asnDatabase.enable {
        SSL_CERT_FILE = "/etc/ssl/certs/ca-certificates.crt";
      };

      serviceConfig = {
        Type = "oneshot";
        ExecStart = lib.escapeShellArgs ([
            "${cfg.package}/bin/cardano-census"
            "--output"
            metricsFile
            "--timeout"
            (toString cfg.timeout)
            "--parallel"
            (toString cfg.parallel)
            "--fork-tolerance"
            (toString cfg.forkTolerance)
          ]
          ++ optionals (cfg.snapshotFile != null) ["--snapshot" cfg.snapshotFile]
          ++ optionals fromNode ["--node-socket" cfg.nodeSocket]
          ++ optionals (cfg.networkMagic != null) ["--network-magic" (toString cfg.networkMagic)]
          ++ optionals (cfg.reportFile != null) ["--report" cfg.reportFile]
          ++ optionals cfg.asnDatabase.enable [
            "--asn-db"
            asnFile
            "--asn-min-relays"
            (toString cfg.asnDatabase.minRelays)
          ]
          ++ cfg.extraArgs);
        # A failed refresh keeps the previous copy; the census runs either way.
        ExecStartPre = optional cfg.asnDatabase.enable "-${refreshAsnDatabase}";
        TimeoutStartSec = "20min";

        DynamicUser = true;
        SupplementaryGroups = optional fromNode cfg.nodeSocketGroup;
        StateDirectory = "cardano-census";
        ReadWritePaths = lib.unique (
          [cfg.textfileDirectory]
          ++ optional (cfg.reportFile != null) (dirOf cfg.reportFile)
          ++ optional fromNode (dirOf cfg.nodeSocket)
        );
        UMask = "0022";

        CapabilityBoundingSet = "";
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX"];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
      };
    };

    systemd.timers.cardano-census = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "5min";
        OnUnitActiveSec = cfg.interval;
        RandomizedDelaySec = "60s";
        Unit = "cardano-census.service";
      };
    };
  };
}
