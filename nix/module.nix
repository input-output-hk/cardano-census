{
  config,
  lib,
  ...
}: let
  cfg = config.services.cardano-census;
  inherit (lib) mkEnableOption mkIf mkOption optional optionals types;

  metricsFile = "${cfg.textfileDirectory}/cardano-census.prom";
in {
  options.services.cardano-census = {
    enable = mkEnableOption "a timed reachability census of the Cardano big ledger peers";

    package = mkOption {
      type = types.package;
      description = "The cardano-census package to run.";
    };

    snapshotFile = mkOption {
      type = types.str;
      example = "/var/lib/cardano-node/peer-snapshot.json";
      description = "peerSnapshotV3 file naming the pools and relays to probe.";
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
      default = 10;
      description = "Seconds allowed for the connect, and again for handshake plus chainsync.";
    };

    parallel = mkOption {
      type = types.ints.positive;
      default = 128;
      description = "Relays probed at once.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      example = ["--prefer-ipv4"];
      description = "Further command line arguments.";
    };
  };

  config = mkIf cfg.enable {
    systemd.services.cardano-census = {
      description = "Reachability census of the Cardano big ledger peers";
      after = ["network-online.target"];
      wants = ["network-online.target"];

      serviceConfig = {
        Type = "oneshot";
        ExecStart = lib.escapeShellArgs ([
            "${cfg.package}/bin/cardano-census"
            "--snapshot"
            cfg.snapshotFile
            "--output"
            metricsFile
            "--timeout"
            (toString cfg.timeout)
            "--parallel"
            (toString cfg.parallel)
          ]
          ++ optionals (cfg.reportFile != null) ["--report" cfg.reportFile]
          ++ cfg.extraArgs);
        TimeoutStartSec = "20min";

        DynamicUser = true;
        StateDirectory = "cardano-census";
        ReadWritePaths = lib.unique (
          [cfg.textfileDirectory]
          ++ optional (cfg.reportFile != null) (dirOf cfg.reportFile)
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
