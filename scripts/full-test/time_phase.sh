#!/usr/bin/env bash
# Run a phase, capture wall time + exit code + parsed metrics, and append
# a JSON line to the full-test run state dir. Per-phase output is
# captured to $KEI_FULLTEST_LOG_DIR/full-test-<phase>.log for failure
# inspection; the terminal gets a compact progress line instead of every
# individual test line. Set KEI_FULLTEST_VERBOSE=1 to tee full phase output.
#
# Usage: time_phase.sh [--live] <phase-name> -- <command...>
#
# --live marks the phase as touching real Apple. Two effects:
#   1. Before running, check the run state dir for .live-skipped (prereq fail)
#      and .rate-limited (503 hit earlier this run). If
#      either is set, append a `skipped` / `rate_limited` JSON record with
#      wall_s=0 and exit 0 without running the command.
#   2. After running, scan the captured log for Apple's 503 signature. If
#      found, touch .rate-limited so subsequent --live phases auto-skip.
#
# Stdout/stderr from the wrapped command are captured to the phase log. The
# JSON-append step is flock'd so parallel phases (e.g. release build +
# docker build) don't race on the staging file.
#
# Parsed metrics (only emitted when matched):
#   tests                 cargo "test result: ok. N passed" or
#                         lib.sh "RESULTS: N pass, M fail"
#   bytes_transferred_mib kei "Transferred N MiB" log lines (sum). Best-
#                         effort: kei only emits this when bytes_downloaded
#                         > 0, and shell suites' grep filters drop it from
#                         most captured output. Don't rely on it alone.
#   assets_processed      kei summary "(N total)" lines (sum). Always
#                         emitted whenever kei runs a sync, regardless of
#                         download path. Robust workload signal.
#   assets_downloaded     kei summary "N downloaded, M failed" lines
#                         (sum of downloaded). Counts files marked
#                         downloaded (includes verify-skip), not bytes.
#   icloud_log_lines      count of `kei::icloud` log lines (API proxy)

set -u

is_live=0
if [[ "${1:-}" == "--live" ]]; then
    is_live=1
    shift
fi

if [[ $# -lt 3 || "$2" != "--" ]]; then
    echo "usage: $0 [--live] <phase-name> -- <command...>" >&2
    exit 64
fi

phase="$1"
shift 2

runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
mkdir -p "$runs_dir"
current="$runs_dir/.current.jsonl"
lockfile="$runs_dir/.lock"
rate_flag="$runs_dir/.rate-limited"
skip_flag="$runs_dir/.live-skipped"

log_dir="${KEI_FULLTEST_LOG_DIR:-/tmp/codex/kei/full-test/logs}"
mkdir -p "$log_dir"
phase_log="$log_dir/full-test-$phase.log"

run_command() {
    if [[ "${KEI_FULLTEST_VERBOSE:-0}" == "1" ]]; then
        "$@" 2>&1 | tee "$phase_log"
        return "${PIPESTATUS[0]}"
    fi

    local progress_to_tty=0
    if [[ -e /dev/tty ]] && { true >/dev/tty; } 2>/dev/null; then
        progress_to_tty=1
    fi

    : >"$phase_log"
    if [[ "$progress_to_tty" -eq 1 ]]; then
        printf '[run] %s (log: %s)\n' "$phase" "$phase_log" >/dev/tty
    else
        printf '[run] %s (log: %s)\n' "$phase" "$phase_log" >&2
    fi
    "$@" >"$phase_log" 2>&1 &
    local pid=$!
    local start_s
    start_s=$(date +%s)
    local frame_index=0
    local frames=('-' $'\\' '|' '/')

    while kill -0 "$pid" 2>/dev/null; do
        local now_s elapsed frame
        now_s=$(date +%s)
        elapsed=$((now_s - start_s))
        frame="${frames[$((frame_index % ${#frames[@]}))]}"
        if [[ "$progress_to_tty" -eq 1 ]]; then
            printf '\r[%s] %s running %ss' "$frame" "$phase" "$elapsed" >/dev/tty
        fi
        frame_index=$((frame_index + 1))
        sleep 0.2
    done

    wait "$pid"
    local rc=$?
    if [[ "$progress_to_tty" -eq 1 ]]; then
        printf '\r%*s\r' 80 '' >/dev/tty
    fi
    return "$rc"
}

# --- Auto-skip path: live phase + a flag is set ---------------------------
if [[ $is_live -eq 1 ]]; then
    skip_status=""
    skip_reason=""
    if [[ -f "$skip_flag" ]]; then
        skip_status="skipped"
        skip_reason=$(tr '\n' ';' <"$skip_flag" | sed 's/;$//')
    elif [[ -f "$rate_flag" ]]; then
        skip_status="rate_limited"
        if ! skip_reason=$(<"$rate_flag"); then
            skip_reason="503 detected earlier this run"
        fi
    fi
    if [[ -n "$skip_status" ]]; then
        script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
        # A failed JSON writer or locked append must not report a successful skip.
        bash -e "$script_dir/record_skip.sh" "$phase" "$skip_status" "$skip_reason" || exit $?
        printf '[%s] %s skipped (%s)\n' "$skip_status" "$phase" "$skip_reason" >&2
        exit 0
    fi
fi

t0=$(date +%s.%N)
run_command "$@"
rc=$?
t1=$(date +%s.%N)

# --- 503 detection: live phase only, after the run ------------------------
if [[ $is_live -eq 1 && -f "$phase_log" ]]; then
    if grep -Eq '503 Service (Temporarily )?Unavailable' "$phase_log"; then
        if [[ ! -f "$rate_flag" ]]; then
            echo "Apple 503 detected in phase $phase (log: $phase_log)" >"$rate_flag"
            echo "*** Apple 503 detected in $phase -- downstream live phases will auto-skip ***" >&2
        fi
    fi
fi

wall=$(python3 -c "print(round($t1 - $t0, 2))" 2>/dev/null || echo "0")
status="pass"
[[ $rc -ne 0 ]] && status="fail"

if [[ "$status" == "fail" ]]; then
    fail_tail_lines="${KEI_FULLTEST_FAIL_TAIL_LINES:-80}"
    {
        echo
        echo "[fail] $phase output tail (last $fail_tail_lines lines from $phase_log):"
        tail -n "$fail_tail_lines" "$phase_log" 2>/dev/null || true
        echo
    } >&2
fi

# Single python pass: extract every metric we care about, emit a JSON
# fragment that the bash side merges into the final record.
metrics_json=$(
    python3 - "$phase_log" <<'PY'
import json, re, sys

tests = 0
tests_seen = False
bytes_mib = 0.0
bytes_seen = False
assets_downloaded = 0
assets_processed = 0
summary_seen = False
icloud_lines = 0

cargo_tests_re = re.compile(r"test result: ok\. (\d+) passed")
shell_tests_re = re.compile(r"RESULTS: (\d+) pass")
transferred_re = re.compile(r"Transferred ([\d.]+)\s*MiB")
# kei summary line: "  3 downloaded, 0 failed (3 total)". The trailing
# "(N total)" is always present; "downloaded" / "failed" appear in that
# fixed order. Anchored on the parenthesised total so we don't pick up
# unrelated "downloaded" mentions elsewhere in the log.
summary_re     = re.compile(
    r"(\d+)\s+downloaded,\s+(\d+)\s+failed\s+\((\d+)\s+total\)"
)
icloud_re      = re.compile(r"kei::icloud")

with open(sys.argv[1], errors="replace") as f:
    for line in f:
        m = cargo_tests_re.search(line)
        if m:
            tests += int(m.group(1)); tests_seen = True
        m = shell_tests_re.search(line)
        if m:
            tests += int(m.group(1)); tests_seen = True
        m = transferred_re.search(line)
        if m:
            bytes_mib += float(m.group(1)); bytes_seen = True
        m = summary_re.search(line)
        if m:
            assets_downloaded += int(m.group(1))
            assets_processed += int(m.group(3))
            summary_seen = True
        if icloud_re.search(line):
            icloud_lines += 1

out = {}
if tests_seen:
    out["tests"] = tests
if bytes_seen:
    out["bytes_transferred_mib"] = round(bytes_mib, 2)
if summary_seen:
    out["assets_downloaded"] = assets_downloaded
    out["assets_processed"] = assets_processed
if icloud_lines:
    out["icloud_log_lines"] = icloud_lines
print(json.dumps(out))
PY
)

# Final record. Always include log_path so diff_runs.sh can surface it on
# failure. Test count + parsed metrics are merged from the python output.
final_json=$(
    python3 - "$phase" "$wall" "$rc" "$status" "$phase_log" "$metrics_json" <<'PY'
import json, sys
phase, wall, rc, status, log_path, metrics_json = sys.argv[1:7]
rec = {
    "phase": phase,
    "wall_s": float(wall),
    "exit": int(rc),
    "status": status,
    "log_path": log_path,
}
rec.update(json.loads(metrics_json or "{}"))
print(json.dumps(rec))
PY
)

# flock'd append: parallel phases (e.g. cargo build --release in
# parallel with `just docker build`) both call time_phase.sh.
(
    flock 9
    echo "$final_json" >>"$current"
) 9>"$lockfile"

# Live status line on stderr.
tests_count=$(printf '%s' "$metrics_json" | python3 -c "import json,sys; d=json.loads(sys.stdin.read() or '{}'); print(d.get('tests',''))")
if [[ -n "$tests_count" ]]; then
    printf '[%s] %s in %ss (rc=%d, %s tests)\n' "$status" "$phase" "$wall" "$rc" "$tests_count" >&2
else
    printf '[%s] %s in %ss (rc=%d)\n' "$status" "$phase" "$wall" "$rc" >&2
fi

exit "$rc"
