#!/usr/bin/env bash
# Fake `minotari` (minotari-cli) used by PrLifecycle unit tests.
#
# Two-step lifecycle simulation:
#   1. `import-view-key ...` — exits 0 immediately, simulating a successful
#      one-shot import. The test reads the argv to confirm the right flags
#      were passed.
#   2. `daemon ...` — sleeps indefinitely (with SIGTERM-graceful trap),
#      simulating the long-running daemon. Does NOT bind a TCP port so the
#      readiness probe loops to the deadline (mirrors fake_pp.sh's intent).
#
# Per MODE_3_REWORK_SPEC.md §13: tests use this fake to exercise lifecycle
# code paths (argv shape, import-then-daemon ordering, teardown) without
# spawning the real `minotari` binary.

set -eu

echo "fake_minotari.sh: spawned (argv: $*)" >&2

# Real `minotari` CLI carries top-level flags (`--network <name>`,
# `--config <path>`) BEFORE the subcommand on argv. Walk past them so this
# fake stays aligned with the real CLI's argv shape (see
# `minotari-cli@52a7287a/minotari/src/cli.rs`).
while [ "$#" -gt 0 ]; do
    case "${1:-}" in
        --network|--config)
            shift 2
            ;;
        --*)
            # Unknown leading flag — preserve and stop skipping so the
            # downstream subcommand parser can complain (real CLI does the
            # same on unknown global flags).
            break
            ;;
        *)
            break
            ;;
    esac
done

subcommand="${1:-}"

case "$subcommand" in
    import-view-key)
        # The harness calls this as a one-shot; exit 0 to advance to
        # `daemon`. No further IO.
        echo "fake_minotari.sh: import-view-key OK" >&2
        exit 0
        ;;
    daemon)
        trap 'echo "fake_minotari.sh: daemon caught SIGTERM, exiting 0" >&2; exit 0' TERM
        while true; do
            sleep 1
        done
        ;;
    *)
        echo "fake_minotari.sh: unrecognised subcommand: ${subcommand}" >&2
        exit 64
        ;;
esac
