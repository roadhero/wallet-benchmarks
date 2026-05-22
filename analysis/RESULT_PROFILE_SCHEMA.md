# Result Profile Schema v1 — wallet-benchmarks#1

**File:** `baseline_profile.json` at repo root.
**Format:** JSON, UTF-8, pretty-printed (2-space indent), Unix line endings.
**Loadable by:** `jq '.' baseline_profile.json` and `python -c 'import json; json.load(open("baseline_profile.json"))'`.
**Schema versioning:** Top-level field `schema_version: 1`. Bump on incompatible field rename/removal; additive changes do not bump.
**Conventions:** snake_case keys, ISO-8601 UTC timestamps (e.g. `2026-05-22T14:33:09Z`), `u64` microTari for amounts, `u64` milliseconds for durations, `null` only where explicitly permitted below.

---

## Top-level structure

```jsonc
{
  "schema_version": 1,
  "run_id": "2026-05-22T14-33-09Z-7f3a",
  "run_start": "2026-05-22T14:33:09Z",
  "run_end":   "2026-05-22T18:11:47Z",
  "config":      { ... },        // §1
  "environment": { ... },        // §2
  "versions":    { ... },        // §3
  "modes": {
    "old_wallet":        { /* 9 scenarios */ },
    "new_wallet":        { /* 9 scenarios */ },
    "payment_processor": { /* 9 scenarios */ }
  },
  "deltas":      { ... },        // §5
  "redaction_denylist": [ ... ]  // §6 (echoed into the profile so its presence is auditable)
}
```

### Top-level metadata

| Field | Type | Required | AC | Example | Notes |
|---|---|---|---|---|---|
| `schema_version` | u32 | yes | — | `1` | Hard constant for v1 schema. |
| `run_id` | string | yes | AC-3 | `"2026-05-22T14-33-09Z-7f3a"` | UTC timestamp + 4-hex-digit random suffix. Filesystem-safe (no `:`). |
| `run_start` | ISO-8601 string | yes | AC-3 | `"2026-05-22T14:33:09Z"` | UTC, second precision. |
| `run_end` | ISO-8601 string | yes | AC-3 | `"2026-05-22T18:11:47Z"` | UTC; equals `run_start` only if harness aborted before any scenario. |

---

## §1 `config` block — 11 keys from issue table (AC-25, AC-36, AC-37)

| Field | Type | Required | AC | Default | Notes |
|---|---|---|---|---|---|
| `a_fund` | u64 (microTari) | yes | AC-25 | `10_000_000_000` (10000 tXTM) | Initial funding amount per mode. Default = 10000 × 1e6 µT. |
| `c_min` | u32 | yes | AC-25, AC-36 | `3` | Confirmation depth. Read by every `wait_for_confirmation` call. |
| `volume_target` | u32 | yes | AC-25 | `512` | Target final UTXO count for S1. |
| `doubling_rounds` | u32 | yes | AC-25 | `6` | Serial rounds in S1 (1,2,4,8,16,32). |
| `fanout_outputs_per_tx` | u32 | yes | AC-25 | `8` | 1→8 outputs in S1 fan-out round. |
| `concurrent_batches` | array<u32> | yes | AC-25 | `[8,16,32,64,128]` | S4 N values. |
| `s4_t_budget_ms` | u64 (ms) | yes | AC-25 | `900_000` (15 min) | S4 hard wall-clock budget per N. |
| `s5_m` | u32 | yes | AC-25 | `100` | Individual-arm tx count and recipient list length. |
| `s5_k` | u32 | yes | AC-25 | `10` | Recipients per batch tx in S5 batch arm. |
| `fee_rate` | u64 (µT/gram) | yes | AC-25, AC-37 | `5` | Picked value; recorded here is the AC. |
| `network` | string | yes | AC-25 | `"esmeralda"` | Hard allowlist: only `"esmeralda"` accepted. |
| `base_node_url` | string | yes | AC-25 | `"https://rpc.esmeralda.tari.com"` | Recorded; host-only redacted-form acceptable if `localhost` (record `"local"`). |
| `per_tx_confirmation_timeout_ms` | u64 (ms) | yes | hidden-AC §timeouts | `1_800_000` (30 min) | Generous-but-bounded per-tx confirmation wait (ambiguity #5). |

---

## §2 `environment` block — AC-26

| Field | Type | Required | AC | Example | Source command (Linux / macOS) |
|---|---|---|---|---|---|
| `cpu_model` | string | yes | AC-26 | `"AMD Ryzen 9 5950X 16-Core Processor"` | `grep "model name" /proc/cpuinfo \| head -1` / `sysctl -n machdep.cpu.brand_string` |
| `ram_bytes` | u64 | yes | AC-26 | `68_719_476_736` | `grep MemTotal /proc/meminfo` (× 1024) / `sysctl -n hw.memsize` |
| `disk_type` | string | yes | AC-26 | `"nvme-ssd"` \| `"sata-ssd"` \| `"hdd"` \| `"unknown"` | `lsblk -d -o name,rota` (rota=0 → ssd) / `diskutil info /` (parse "Solid State") |
| `os` | string | yes | AC-26 | `"Linux 6.5.0-21-generic x86_64"` | `uname -srm` (both) |
| `network_path_to_base_node` | string | yes | AC-26 | `"remote"` | Literal `"local"` if `base_node_url` host resolves to loopback (`127.0.0.1`, `::1`, `localhost`); else `"remote"`. |

---

## §3 `versions` block — 3 components from AC-27

| Field | Type | Required | AC | Example | Notes |
|---|---|---|---|---|---|
| `minotari_console_wallet.tag` | string | yes (one of tag/commit) | AC-27 | `"v5.3.1-pre.3"` | Release tag. |
| `minotari_console_wallet.commit` | string (40-hex) | yes (one of tag/commit) | AC-27 | `"766f80ccc20596413ee208311750c11e02a2841d"` | Commit hash. Tag preferred when available. |
| `minotari_cli.commit` | string (40-hex) | yes | AC-27 | `"52a7287a3fe1e7831855649c530534af9f2d4830"` | No releases on `minotari-cli`; commit required. |
| `base_node.tag` or `.commit` | string | yes | AC-27 | `"v5.3.1-pre.3"` | Esmeralda base node version. Captured via `GET https://rpc.esmeralda.tari.com/get_tip_info` if exposed, else recorded from documented pinned version. |
| `harness.commit` | string (40-hex) | yes | AC-27 | `"abc123..."` | Self-disclosure: this harness's git HEAD at run time. |

If a component is built from a commit that has no release tag, `tag` is `null` and `commit` is required. If running off a tag, `commit` may still be filled (recommended).

---

## §4 `modes.<mode>.<scenario>` blocks — 27 cells (AC-8)

Every cell has the **common envelope** below. Scenario-specific fields are listed afterward.

### Common envelope (every cell, AC-9, AC-38)

| Field | Type | Required | AC | Notes |
|---|---|---|---|---|
| `status` | enum string | yes | AC-8, AC-33 | One of `"success"` \| `"failure"` \| `"halted"` \| `"timeout"`. `halted` = scenario decided to stop (e.g. S1 round failure, AC-13). `timeout` = S4 budget elapsed (AC-17) or per-tx timeout. |
| `wall_clock_ms` | u64 (ms) | yes | AC-9 | Whole-scenario duration. |
| `tip_height_start` | u64 | yes | AC-9 | Base-node tip at scenario start. |
| `tip_height_end` | u64 | yes | AC-9 | Base-node tip at scenario end. |
| `fees_paid_microtari` | u64 | yes | AC-9 | Sum of fees emitted by this scenario. `0` for read-only scans. |
| `balance_before_microtari` | u64 | yes | AC-9 | From base-node-reported UTXO sum or wallet `GetBalance`. |
| `balance_after_microtari` | u64 | yes | AC-9 | Same source as `balance_before`. |
| `balance_delta_microtari` | i64 | yes | AC-9, AC-14 | `balance_after - balance_before` (signed). |
| `balance_reconciliation_ok` | bool | yes | AC-9, AC-14 | True if `balance_delta == -(fees_paid + amount_sent_to_others)`. Always recorded even if true. |
| `errors` | object | yes | AC-38 | See below. |

### `errors` sub-object (AC-38)

| Field | Type | Required | Notes |
|---|---|---|---|
| `success_count` | u64 | yes | Successful tx submissions (or block-scans) in this scenario. |
| `rejection_count` | u64 | yes | Mempool/validation rejection per base-node response. |
| `stall_count` | u64 | yes | Tx accepted but unconfirmed past `per_tx_confirmation_timeout_ms` (AC-33). |
| `timeout_count` | u64 | yes | S4 budget timeout (AC-17) or harness-side timeout. |
| `details` | array<object> | yes | One entry per non-success event. Each: `{ txid: string \| null, error_string: string, phase: "construct" \| "sign" \| "broadcast" \| "confirm" \| "scan" }`. |

### B0 scenario (AC-10)

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `t_scan_ms` | u64 | yes | AC-10 | Duration of from-genesis scan only. |
| `blocks_per_sec` | f64 | yes | AC-10 | `(tip_height_end - 0) / (t_scan_ms / 1000)`. |
| `h_tip_start` | u64 | yes | AC-10 | Tip when scan begins. |
| `h_tip_end` | u64 | yes | AC-10 | Tip when scan completes. |
| `peak_rss_bytes` | u64 \| null | yes | AC-10 | Resident set size sampled at 1 Hz. `null` only on platforms where we can't read it (document in run notes). |
| `peak_cpu_pct` | f64 \| null | yes | AC-10 | 0-100 across all cores. `null` permitted with same condition. |
| `utxo_count_verified` | u64 | yes | AC-10 | Expected `0`. |
| `balance_verified_microtari` | u64 | yes | AC-10 | Expected `0`. |
| `outputs_found` | u64 | yes | AC-10 | Expected `0`. |

### S0 scenario (AC-11)

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `utxo_count_verified` | u64 | yes | AC-11 | Expected `1`. |
| `balance_verified_microtari` | u64 | yes | AC-11 | Expected `a_fund`. |
| `t_broadcast_to_mempool_ms` | u64 | yes | AC-11 | From submit-call return to `accepted=true`. |
| `t_broadcast_to_confirmed_ms` | u64 | yes | AC-11 | From submit-call return to depth ≥ `c_min`. |
| `h_birth` | u64 | yes | AC-11, AC-16 | Block height at which the funding UTXO was mined. Drives S3/S7 birthday. |

### S1 scenario (AC-12, AC-13, AC-14)

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `rounds` | array<object> | yes | AC-12 | Length ≤ 7 (6 doubling + 1 fan-out). Halts (AC-13) → array shorter. |
| `rounds[].name` | string | yes | AC-12 | `"doubling-1"`, `"doubling-2"`, …, `"fanout-64"`. |
| `rounds[].tx_count_target` | u32 | yes | AC-12 | Per issue table: 1,2,4,8,16,32,64. |
| `rounds[].outputs_per_tx` | u32 | yes | AC-12 | 2 for doubling rounds, 8 for fan-out. |
| `rounds[].tx_records` | array<object> | yes | AC-12 | Each: `{ txid, t_construct_ms, t_broadcast_ms, t_confirm_ms, status, error_string?, fee_microtari }`. |
| `rounds[].round_wall_clock_ms` | u64 | yes | AC-12 | Round-level duration. |
| `rounds[].round_fees_microtari` | u64 | yes | AC-12, AC-14 | Σ of `tx_records[].fee_microtari`. |
| `rounds[].pre_balance_microtari` | u64 | yes | AC-14 | Pre-round wallet balance. |
| `rounds[].post_balance_microtari` | u64 | yes | AC-14 | Post-round wallet balance. |
| `rounds[].reconciliation_delta_microtari` | i64 | yes | AC-14 | `post - pre - (-round_fees)` — expected 0 if all confirmed; flagged otherwise. |
| `rounds[].failure_count` | u32 | yes | AC-12 | Count of `tx_records[].status != "success"`. |
| `final_utxo_count` | u32 | yes | AC-12 | Expected `512` if scenario ran to completion (or recorded as observed if halted). |
| `halted_at_round` | string \| null | yes | AC-13 | Name of round that triggered halt. `null` if completed. |

### S2 scenario (AC-15, AC-24, AC-34)

Includes B0 metric set + the following:

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `data_dir_wiped` | bool | yes | AC-34 | Asserted `true`. |
| `birthday_set` | u16 | yes | AC-24 | Asserted `0`. |
| `outputs_found` | u32 | yes | AC-15 | Expected `512`. |
| `balance_after_microtari` (override of common) | u64 | yes | AC-15 | Expected `≈ a_fund − Σ S1 fees`. |
| `s1_txids_history_verified` | bool | yes | AC-15 | True if every txid from S1's rounds appears in rediscovered history. |
| `t_scan_ms` | u64 | yes | AC-15 | Same definition as B0. |
| `blocks_per_sec` | f64 | yes | AC-15 | Same. |
| `peak_rss_bytes` | u64 \| null | yes | AC-15 | Same. |
| `peak_cpu_pct` | f64 \| null | yes | AC-15 | Same. |
| `h_tip_start` | u64 | yes | AC-15 | Same. |
| `h_tip_end` | u64 | yes | AC-15 | Same. |

### S3 scenario (AC-16)

Identical shape to S2, except:

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `birthday_set` | u16 | yes | AC-16 | Asserted equal to S0's `h_birth` (encoded back to a 16-bit days-since-2022-01-01 value). |
| `blocks_scanned` | u64 | yes | AC-16 | `h_tip_end − h_birth_block_height`. |

### S4 scenario (AC-17, AC-18, AC-30, AC-31, AC-32, AC-33)

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `sub_blocks` | object | yes | AC-17 | Keyed by stringified N: `"8"`, `"16"`, `"32"`, `"64"`, `"128"`. Exactly 5 keys. |
| `sub_blocks.<N>.n_concurrent` | u32 | yes | AC-17 | Echoed N. |
| `sub_blocks.<N>.budget_elapsed` | bool | yes | AC-17 | True if `S4_T_budget` hit. |
| `sub_blocks.<N>.batch_wall_clock_ms` | u64 | yes | AC-18 | From first dispatch to last terminal. |
| `sub_blocks.<N>.success_rate` | f64 | yes | AC-18 | `success_count / N` in [0,1]. |
| `sub_blocks.<N>.max_serialization_gap_ms` | u64 | yes | AC-18 | Max delta between consecutive `t_construct_complete` events. |
| `sub_blocks.<N>.double_selection_rejections` | u32 | yes | AC-18 | Count of rejections whose reason indicates the same UTXO was selected twice. |
| `sub_blocks.<N>.tx_records` | array<object> | yes | AC-18 | Each: `{ txid, t_submit_ms, t_construct_complete_ms, broadcast_outcome: "accepted" \| "rejected" \| "error", t_confirm_ms \| null, error_string?, rejection_reason? }`. |

### S5 scenario (AC-19, AC-20, AC-21)

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `pre_state_utxos` | u32 | yes | AC-21 | Post-S4 UTXO count, no normalization. |
| `pre_state_balance_microtari` | u64 | yes | AC-21 | Post-S4 balance. |
| `recipient_list_hash` | string (hex) | yes | AC-19 | SHA-256 of canonical recipient JSON; same hash MUST appear under both `arms.batch` and `arms.individual`. |
| `arms.batch.applies` | bool | yes | AC-20 | True only when mode = `payment_processor` (and per ambiguity #3, also true for context-only PP individual-arm run as `arms.batch.applies = false / arms.individual.applies = true`). |
| `arms.batch.t_total_ms` | u64 \| null | yes/conditional | AC-19 | `null` iff `applies=false`. |
| `arms.batch.fees_total_microtari` | u64 \| null | yes/conditional | AC-19 | |
| `arms.batch.fee_per_recipient_microtari` | f64 \| null | yes/conditional | AC-19 | |
| `arms.batch.blocks_consumed` | u64 \| null | yes/conditional | AC-19 | |
| `arms.batch.tx_success_count` | u32 \| null | yes/conditional | AC-19 | |
| `arms.batch.tx_failure_count` | u32 \| null | yes/conditional | AC-19 | |
| `arms.individual` | object | yes | AC-19, AC-20 | Same shape as `arms.batch`. Applies for `new_wallet` and `old_wallet`. |
| `throughput_multiplier` | f64 \| null | yes | AC-19, AC-28 | `t_individual / t_batch`. `null` permitted only when not both arms ran in the same mode (cross-mode multiplier sits in `deltas`). |

### S6 scenario (AC-22, AC-24, AC-34)

Identical shape to S2, plus:

| Field | Type | Req | AC | Notes |
|---|---|---|---|---|
| `delta_vs_s2_ms` | i64 | yes | AC-22 | `t_scan_ms(S6) - t_scan_ms(S2)`. |
| `delta_vs_b0_ratio` | f64 | yes | AC-22 | `t_scan_ms(S6) / t_scan_ms(B0)`. |

### S7 scenario (AC-23)

Identical to S3, with `birthday_set = h_birth` from S0.

---

## §5 `deltas` block — top-level computed values (AC-28)

| Field | Type | Required | AC | Computation |
|---|---|---|---|---|
| `t_scan_s2_minus_b0_ms.<mode>` | i64 | yes for each mode | AC-28 | `S2.t_scan_ms - B0.t_scan_ms`. |
| `t_scan_s6_minus_s2_ms.<mode>` | i64 | yes for each mode | AC-28 | `S6.t_scan_ms - S2.t_scan_ms`. |
| `t_scan_s6_over_b0_ratio.<mode>` | f64 | yes for each mode | AC-28 | `S6.t_scan_ms / B0.t_scan_ms`. |
| `s5_throughput_multiplier.payment_processor_batch_vs_new_wallet_individual` | f64 | yes | AC-19, AC-28 | `new_wallet.S5.arms.individual.t_total_ms / payment_processor.S5.arms.batch.t_total_ms`. |
| `s5_throughput_multiplier.payment_processor_batch_vs_old_wallet_individual` | f64 | yes | AC-19, AC-28 | `old_wallet.S5.arms.individual.t_total_ms / payment_processor.S5.arms.batch.t_total_ms`. |

Any delta where an input cell is `halted`, `timeout`, or `null` is emitted as `null` and a `note` field is added at sibling level explaining why.

---

## §6 `redaction_denylist` — auditable list of forbidden substrings/patterns

This array is serialised into the result profile so the redaction contract is auditable from the artifact alone. arch-test will assert that `serde_json::to_string(&profile)` contains none of the patterns when applied to a populated profile generated with real seed material set via env.

Concrete entries (rules, not values):

| ID | Type | Pattern | Reason |
|---|---|---|---|
| R1 | regex | `\b(?:[a-z]{3,8}\s+){11,23}[a-z]{3,8}\b` | BIP-39-shaped 12/24-word phrase. |
| R2 | substring (from env) | value of `$HARNESS_SEED_OLD`, `$HARNESS_SEED_NEW`, `$HARNESS_SEED_PP` (each whole-string and each individual word) | Captured seed phrases. |
| R3 | regex | `(?i)\bview[-_ ]?key\b\s*[:=]\s*[0-9a-f]{32,}` | Hex-encoded view key. |
| R4 | regex | `(?i)\bspend[-_ ]?key\b\s*[:=]\s*[0-9a-f]{32,}` | Hex-encoded spend key. |
| R5 | substring (from env) | value of `$HARNESS_WALLET_PW` | Wallet passphrase. |
| R6 | regex | `"[0-9a-fA-F]{2048,}"` | Long hex blob — raw signed-tx body threshold. |
| R7 | regex | `"[A-Za-z0-9+/=]{1024,}"` | Long base64 blob — alt-encoding raw tx threshold. |
| R8 | regex | `(?i)\bBearer\s+[A-Za-z0-9._\-]{20,}` | gRPC/HTTP bearer tokens. |
| R9 | substring | the literal username from `$USER`/`$LOGNAME` | $HOME path leakage. |
| R10 | substring | `/Users/<user>` and `/home/<user>` constructed from `$USER` at run-time | Path leakage. |

The denylist itself is committed in source as a `Vec<RedactionRule>` initialised at startup; the JSON copy in the profile is a string-form mirror (no env-derived values written into the JSON — only rule IDs and descriptions).

