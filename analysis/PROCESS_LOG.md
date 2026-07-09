# Process log

## 2026-07-06: scope change during an operator-absence window (corrected)

During the run-9 validation cycle, a config-surface change (commit
294f327, the wallet_ready_deadline_ms decoupling) was designed,
implemented, and committed while the operator was away, after a
blocking defect made the approved validation settings unrunnable. The
fix itself was evidence-backed, default-preserving, and later received
retroactive signoff on review of the full commit body, diff, and
revert-cost analysis; the process was still wrong. Correction adopted
for the remainder of this branch: any scope expansion beyond an
approved design, during anyone's absence window, is surfaced and held
pending explicit signoff, no matter how obvious the fix seems; silence
is not signoff. Related: two never-pushed commits were re-created
(message-only amend plus reparenting cherry-pick, trees verified
byte-identical) to place a classification rationale in the redesign
commit body; the pre-amend chain is preserved on the local ref
backup-pre-amend until this PR merges.

Signed off: Dennis Vorobyov

## 2026-07-09: Rule 13 misapplication by the operator, corrected on review pushback

The operator misapplied Rule 13 to within-mechanism engineering value
calls (the D-1 settle timeout, 600 s vs a 300 s figure that had only
ever been a labeled design-discussion assumption) and to a commit
boundary decision that the original proposal itself authorized (two
defects batched per its "batch only if they can't be split" caveat).
The implementation review pushed back with cited evidence from the
authorization turns, including a nonexistent commit reference in the
original finding. Retraction applied, and the two artifacts produced
under the misapplied correction (a timeout config knob and a
"recurrence" log entry) were dropped from the branch tip, preserved on
the local ref backup-retracted-items until this PR merges. Rule
discipline runs both ways: Rule 13 is meant to prevent scope expansion
during absence, not to gate value discussions within authorized
mechanisms.

Signed off: Dennis Vorobyov
