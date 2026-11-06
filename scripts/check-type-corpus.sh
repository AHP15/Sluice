#!/usr/bin/env bash
# T-069 / R-018: run the Postgres type-fidelity corpus and REFUSE a hollow pass.
#
# R-018: an opt-in test nothing runs is indistinguishable from an absent one —
# it passes at commit 100, breaks at commit 400, and reports nothing. So this
# gate fails unless the corpus actually ran: a `TYPE-CORPUS: SKIPPED` line, a
# missing summary line, or zero cases/checks are all failures, as is any test
# failure. The live driver suites are gated the same way.
#
# Needs TEST_PG_URL (the corpus) and a server at SLUICE_TEST_PG_HOST/PORT
# (default localhost:55432, user postgres / sluice_dev — the live suites).
set -euo pipefail

if [[ -z "${TEST_PG_URL:-}" ]]; then
  echo "::error::TEST_PG_URL is not set; the corpus would skip" >&2
  exit 1
fi

log=$(mktemp)
cargo test -p sluice-postgres --test type_corpus --test live --test pool_reset --test redaction_corpus --test filters_live --test navigator_live --test cancel_live --test paging_live --test budget_live -- --nocapture 2>&1 | tee "$log"

summary=$(grep -E '^TYPE-CORPUS:' "$log" || true)
if [[ -z "$summary" ]]; then
  echo "::error::type corpus printed no TYPE-CORPUS summary line" >&2
  exit 1
fi
if grep -q 'TYPE-CORPUS: SKIPPED' <<<"$summary"; then
  echo "::error::type corpus skipped: $summary" >&2
  exit 1
fi
if [[ "$summary" =~ TYPE-CORPUS:\ ([0-9]+)\ cases,\ ([0-9]+)\ checks ]]; then
  cases=${BASH_REMATCH[1]}
  checks=${BASH_REMATCH[2]}
else
  echo "::error::unrecognised corpus summary: $summary" >&2
  exit 1
fi
if (( cases == 0 || checks == 0 )); then
  echo "::error::type corpus ran zero cases or checks: $summary" >&2
  exit 1
fi
# The live suites skip (rather than fail) when no server is reachable; in CI a
# server is always provided, so a skip means the wiring broke.
if grep -q 'SKIP: no Postgres reachable' "$log"; then
  echo "::error::a live Postgres suite skipped — the service container is not reachable" >&2
  exit 1
fi

echo "type corpus gate: $cases cases, $checks checks, live suites ran"
