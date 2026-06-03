#!/usr/bin/env bash
# Fake `minotari_payment_processor` used by PpLifecycle unit + integration tests.
#
# Behaviour:
#   * Echoes a banner to stdout so callers can confirm spawn happened.
#   * Reacts to SIGTERM with a graceful exit (status 0), simulating PP's own
#     `tokio::signal::ctrl_c` handler. Without the trap, the bash interpreter
#     would itself exit 143 on SIGTERM; the explicit handler lets us assert
#     "PP exited gracefully after SIGTERM" in the teardown test.
#   * Does NOT bind any TCP port — the `wait_ready_times_out_when_fake_never_binds`
#     test relies on this so the readiness probe loops until the deadline.
#   * Sleeps indefinitely otherwise; the parent's `kill_on_drop(true)` plus
#     teardown SIGTERM-then-SIGKILL escalation guarantees cleanup.
#
# Per MODE_3_REWORK_SPEC.md §13: tests use this fake to exercise lifecycle
# code paths (spawn, teardown, drop) without spawning the real PP binary.

set -eu

echo "fake_pp.sh: spawned (argv: $*)" >&2

trap 'echo "fake_pp.sh: caught SIGTERM, exiting 0" >&2; exit 0' TERM

# Loop with short sleeps so the SIGTERM trap fires promptly. A bare
# `sleep infinity` blocks signal handling on some bash builds.
while true; do
    sleep 1
done
