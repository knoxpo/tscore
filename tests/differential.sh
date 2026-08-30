#!/bin/bash
# Differential gate: every golden program must produce identical output
# across execution modes. stdout compared exactly; stderr compared as
# SORTED lines (actor-thread warnings legitimately race the main thread's
# output ordering).
#
# Usage: tests/differential.sh [path-to-tscore]
set -u
BIN="${1:-./target/release/tscore}"
BASE_ENV="TSC_NO_JIT=1"
MODES=(
    ""                                                # default (JIT + nursery)
    "TSC_OSR_THRESHOLD=10 TSC_JIT_THRESHOLD=1"        # aggressive JIT
    "TSC_NO_NURSERY=1"                                # nursery off
    "TSC_NO_NURSERY=1 TSC_OSR_THRESHOLD=10 TSC_JIT_THRESHOLD=1"
    "TSC_NURSERY_BYTES=1"                             # GC stress: minor per safepoint
    "TSC_NURSERY_BYTES=1 TSC_OSR_THRESHOLD=10 TSC_JIT_THRESHOLD=1"
)

fail=0
for f in tests/golden/*.ts; do
    base_out=$(env $BASE_ENV "$BIN" run "$f" --no-stats-export 2>/tmp/tsc_diff_err_a)
    base_err=$(sort /tmp/tsc_diff_err_a)
    for mode in "${MODES[@]}"; do
        out=$(env $mode "$BIN" run "$f" --no-stats-export 2>/tmp/tsc_diff_err_b)
        err=$(sort /tmp/tsc_diff_err_b)
        if [ "$out" != "$base_out" ]; then
            echo "STDOUT DIFF [$mode] $f"
            fail=1
        fi
        if [ "$err" != "$base_err" ]; then
            echo "STDERR DIFF [$mode] $f"
            fail=1
        fi
    done
done
if [ $fail -eq 0 ]; then
    echo "DIFFERENTIAL-OK (${#MODES[@]} modes vs interp baseline)"
fi
exit $fail
