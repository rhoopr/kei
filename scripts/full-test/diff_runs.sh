#!/usr/bin/env bash
# Compare the most recent run against the median of the prior N runs and
# emit a markdown report: phase table with wall-time deltas vs median, then
# a metrics table with median deltas.
#
# Median (not last-run) is the baseline so single noisy runs don't dominate
# the comparison. With < 2 prior runs the script falls back to last-run.
#
# Highlight rules (unchanged from the prior-only version):
#   - wall row when |delta| > 20% AND median > 5s
#   - metric row when |delta| >= per-metric raw threshold
#
# Tunables (env):
#   KEI_FULLTEST_BASELINE_N  number of prior runs to median over (default 5)

set -euo pipefail

runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
baseline_n="${KEI_FULLTEST_BASELINE_N:-5}"

mapfile -t recs < <(find "$runs_dir" -maxdepth 1 -name '*.json' -type f -printf '%f\n' | sort)

if [[ ${#recs[@]} -eq 0 ]]; then
    echo "(no run records found)" >&2
    exit 1
fi

current="$runs_dir/${recs[-1]}"

# Priors are everything except the current run, newest last; we'll take the
# last $baseline_n in Python.
priors=()
if [[ ${#recs[@]} -ge 2 ]]; then
    for f in "${recs[@]:0:${#recs[@]}-1}"; do
        priors+=("$runs_dir/$f")
    done
fi

python3 - "$current" "$baseline_n" "${priors[@]}" <<'PY'
import json, os, re, sys
from statistics import median

cur_path = sys.argv[1]
baseline_n = int(sys.argv[2])
prior_paths = sys.argv[3:]

with open(cur_path) as f:
    cur = json.load(f)

# Use the last N priors (most recent N runs before current).
prior_paths = prior_paths[-baseline_n:] if baseline_n > 0 else []
priors = []
for p in prior_paths:
    with open(p) as f:
        priors.append(json.load(f))

PHASE_META = {
    "static_checks":            (10, "static", None),
    "offline_core":             (20, "offline", None),
    "scenarios":                (25, "scenario", None),
    "nightly_tools":            (30, "nightly", None),
    "package":                  (40, "package", None),
    "docker_full":              (50, "docker", None),
    "live_provider":            (60, "live", 60),
    "test_shell_state_machine": (65, "live-shell", 20),
    "test_shell_concurrency":   (66, "live-shell", 9),
    "test_shell_docker":        (67, "live-shell", 16),
    "live_status":              (70, "live-bin", None),
    "live_libraries":           (71, "live-bin", None),
    "live_albums":              (72, "live-bin", None),
    "live_dryrun":              (73, "live-bin", None),
    "live_config_show":         (74, "live-bin", None),
    "live_verify":              (75, "live-bin", None),
    "live_reconcile_dryrun":    (76, "live-bin", None),
    "live_password_backend":    (77, "live-bin", None),
    "live_import_dryrun":       (78, "live-bin", None),
    "live_import_rehearsal":    (79, "live-bin", None),
    "live_cross_zone_album":    (80, "live-bin", None),
    # Legacy phase names from earlier full-test layouts.
    "gate":                     (10, "0", 1990),
    "nodefault":                (15, "0.5", None),
    "fuzz_build":               (17, "0.75", None),
    "udeps":                    (18, "0.8", None),
    "offline_all":              (19, "0.9", None),
    "scope_matrix":             (19, "0.95", 1),
    "build_release":            (20, "1", None),
    "release_archive_smoke":    (21, "1.5", None),
    "docker_build":             (30, "2", None),
    "docker_puid_smoke":        (31, "2", None),
    "docker_multiarch":         (32, "2", None),
    "docker_version":           (33, "2", None),
    "docker_help":              (34, "2", None),
    "docker_default_cmd":       (35, "2", None),
    "test_live":                (60, "3", 60),
    "test_state_machine":       (65, "4", 20),
    "test_concurrency":         (66, "4", 9),
    "test_docker_live":         (67, "4", 16),
}

ICON = {"pass": "✅", "fail": "❌", "skipped": "⏭", "rate_limited": "🚦"}


def med(values):
    """Median of non-None numeric values, or None if empty."""
    nums = [v for v in values if isinstance(v, (int, float))]
    return median(nums) if nums else None


def fmt_wall_delta(cur_w, base_w):
    if base_w is None or base_w < 5:
        return "-"
    d = (cur_w - base_w) / base_w * 100
    s = f"{d:+.1f}%"
    if abs(d) > 20:
        s = f"**{s}**"
    return s


def sort_key(name):
    if name in PHASE_META:
        return (0, PHASE_META[name][0], name)
    return (1, 0, name)


# --- Phase wall medians ----------------------------------------------------

def phase_wall_median(name):
    return med(r.get("phases", {}).get(name, {}).get("wall_s") for r in priors)


# --- Phase table -----------------------------------------------------------

phases = cur.get("phases", {})
order = sorted(phases.keys(), key=sort_key)

baseline_label = f"median(n={len(priors)})" if priors else "median"

print(f"Branch: `{cur.get('branch','?')}` @ `{cur.get('head','?')}` -- {cur.get('started_at','?')}")
if priors:
    span_first = priors[0].get("started_at", "?")
    span_last = priors[-1].get("started_at", "?")
    print(f"Baseline: {baseline_label} over {len(priors)} prior run(s) from {span_first} to {span_last}")
else:
    print("Baseline: (no prior runs)")
print()
print(f"| # | Phase                      | Result | Tests | Wall (s) | Δ vs {baseline_label} |")
print( "|---|----------------------------|--------|-------|----------|------------|")

total_wall = 0.0
total_tests = 0
total_pass_phases = 0
total_phases = 0
fails = []
for name in order:
    p = phases[name]
    status = p.get("status", "?")
    icon = ICON.get(status, "?")
    wall = p.get("wall_s", 0.0)
    if isinstance(wall, (int, float)):
        total_wall += wall
    meta = PHASE_META.get(name, (999, "?", None))
    phase_num = meta[1]
    test_count = p.get("tests")
    if test_count is None:
        test_count = meta[2]
    if test_count is None:
        tests_cell = "-"
    else:
        tests_cell = str(test_count)
        if status == "pass":
            total_tests += test_count
    base_wall = phase_wall_median(name)
    delta = fmt_wall_delta(wall, base_wall)
    print(f"| {phase_num} | {name:<26} | {icon}     | {tests_cell:>5} | {wall:>8.2f} | {delta:>10} |")
    total_phases += 1
    if status == "pass":
        total_pass_phases += 1
    else:
        fails.append((name, status))

print()
mins, secs = divmod(int(total_wall), 60)
print(f"Phases: {total_pass_phases} / {total_phases} pass. Tests counted: {total_tests}. Total wall: {total_wall:.1f}s ({mins}m {secs:02d}s).")

# --- Run-level metrics ----------------------------------------------------

# (label, key, unit, abs-significance-threshold)
METRICS = [
    ("Binary",          "binary_mb",         "MB",  0.5),
    ("Docker image",    "docker_image_mb",   "MB",  2.0),
    ("Deps (Cargo.lock)", "deps_count",      "",    1),
    ("Audit advisories", "audit_warnings",   "",    1),
    ("src/ unwrap+expect", "src_unwrap_expect", "", 1),
    ("src/ #[allow(..)]", "src_allow_attrs",  "",   1),
    ("Fuzz targets",    "fuzz_target_count", "",    1),
]
cur_m = cur.get("metrics", {}) or {}


def metric_median(key):
    return med((r.get("metrics") or {}).get(key) for r in priors)


def fmt_metric(value, unit):
    if value is None:
        return "-"
    if unit == "MB":
        # Always one decimal so sub-MB drift in the median is visible
        # (median often lands on 24.0 even when current is 24.3).
        return f"{value:.1f}{unit}"
    if unit:
        return f"{value}{unit}"
    if isinstance(value, float):
        # Counts come through as ints; floats here are typically the median
        # of a run of integer-valued samples (e.g. 13.5). Show one decimal.
        return f"{value:.1f}" if not value.is_integer() else f"{int(value)}"
    return str(value)


if cur_m:
    print()
    print("**Run metrics:**")
    print()
    print(f"| Metric             | Current | {baseline_label:<14} | Δ |")
    print( "|--------------------|---------|----------------|---|")
    for label, key, unit, thresh in METRICS:
        cv = cur_m.get(key)
        bv = metric_median(key)
        if cv is None:
            continue
        cs = fmt_metric(cv, unit)
        bs = fmt_metric(bv, unit)
        if bv is None:
            ds = "-"
        else:
            d = cv - bv
            sign = "+" if d >= 0 else ""
            if unit == "MB":
                ds = f"{sign}{d:.1f}"
            elif isinstance(d, float) and not d.is_integer():
                ds = f"{sign}{d:.1f}"
            else:
                ds = f"{sign}{int(d) if isinstance(d, float) else d}"
            if abs(d) >= thresh:
                ds = f"**{ds}**"
        print(f"| {label:<18} | {cs:>7} | {bs:>14} | {ds} |")

# --- Per-phase live metrics ----------------------------------------------

# (label, key, formatter, delta_format) - rendered when at least one phase
# in the current run has the field. Order matches table column order.
LIVE_FIELDS = [
    ("Assets (total)",  "assets_processed",     "{:d}",   "{:+d}"),
    ("Assets dl",       "assets_downloaded",    "{:d}",   "{:+d}"),
    ("MiB tx",          "bytes_transferred_mib","{:.1f}", "{:+.2f}"),
    ("iCloud lines",    "icloud_log_lines",     "{:d}",   "{:+d}"),
]


def phase_metric_median(name, key):
    return med(r.get("phases", {}).get(name, {}).get(key) for r in priors)


# Discover which phases have any live data in the current run, and which
# fields are present anywhere in the current run (skip empty columns).
per_phase_phases = []
present_fields = set()
for name in order:
    p = phases[name]
    has_any = any(p.get(k) is not None for _, k, _, _ in LIVE_FIELDS)
    if has_any:
        per_phase_phases.append(name)
        for _, k, _, _ in LIVE_FIELDS:
            if p.get(k) is not None:
                present_fields.add(k)

if per_phase_phases:
    fields = [f for f in LIVE_FIELDS if f[1] in present_fields]
    print()
    print("**Per-phase live metrics** (current / median):")
    print()
    header = "| Phase                      |" + "|".join(
        f" {label} " for label, _, _, _ in fields
    ) + "|" + "|".join(f" Δ {label} " for label, _, _, _ in fields) + "|"
    sep = "|----------------------------|" + "|".join("-" * (len(label) + 2) for label, _, _, _ in fields) + "|" + "|".join("-" * (len(label) + 4) for label, _, _, _ in fields) + "|"
    print(header)
    print(sep)
    for name in per_phase_phases:
        p = phases[name]
        cells = []
        delta_cells = []
        for label, key, fmt, dfmt in fields:
            cv = p.get(key)
            bv = phase_metric_median(name, key)
            if cv is None:
                cells.append("-")
                delta_cells.append("-")
                continue
            # Median of a count sample is sometimes a half-integer (e.g.
            # 4.5) when the sample size is even - keep one decimal then.
            cv_str = fmt.format(cv) if isinstance(cv, int) or float(cv).is_integer() else f"{cv:.1f}"
            if bv is None:
                cells.append(f"{cv_str}/-")
                delta_cells.append("-")
            else:
                bv_str = fmt.format(int(bv)) if float(bv).is_integer() else f"{bv:.1f}"
                cells.append(f"{cv_str}/{bv_str}")
                d = cv - bv
                delta_cells.append(dfmt.format(int(d)) if isinstance(d, int) or d.is_integer() else f"{d:+.2f}")
        print(f"| {name:<26} | " + " | ".join(cells) + " | " + " | ".join(delta_cells) + " |")

# --- Footer --------------------------------------------------------------

print()
print(f"Run: `{cur_path}`")

if fails:
    print()
    print("**Non-pass phases:**")
    for name, status in fails:
        p = phases[name]
        log = p.get("log_path")
        reason = p.get("reason")
        suffix_parts = []
        if reason:
            suffix_parts.append(f"reason: {reason}")
        if log:
            suffix_parts.append(f"log: `{log}`")
        suffix = (" -- " + " -- ".join(suffix_parts)) if suffix_parts else ""
        print(f"- `{name}`: {status}{suffix}")

# --- Failure context ------------------------------------------------------
# For each failed phase, show the last N lines of the log + grep'd error
# signatures so triage doesn't require opening the file. Skipped /
# rate_limited rows already carry their reason in the bullet above and
# don't have logs, so they're not duplicated here.
FAIL_TAIL_LINES = 30
FAIL_ERR_LINES  = 15
ERR_RE = re.compile(r"error:|panicked|fatal|503 Service|FAILED|ERROR")
# Cargo / tracing logs that pass through `assert_cmd` panic strings come out
# with literal `\x1b[..m` text instead of real ANSI bytes. Strip them so the
# markdown report stays legible. Real ANSI bytes (\x1b directly) are also
# stripped just in case a phase ever pipes raw colored output through tee.
ANSI_LITERAL_RE = re.compile(r"\\x1b\[[0-9;]*m")
ANSI_RE         = re.compile(r"\x1b\[[0-9;]*m")

def clean(line):
    return ANSI_RE.sub("", ANSI_LITERAL_RE.sub("", line))

failed_only = [(n, s) for n, s in fails if s == "fail"]
if failed_only:
    print()
    print("**Failure context:**")
    for name, status in failed_only:
        p = phases[name]
        log = p.get("log_path")
        print()
        print(f"### `{name}`")
        if not log:
            print("(no log captured)")
            continue
        if not os.path.exists(log):
            print(f"(log path `{log}` not present -- /tmp may have been cleaned)")
            continue
        try:
            with open(log, errors="replace") as fh:
                lines = fh.readlines()
        except OSError as e:
            print(f"(could not read log: {e})")
            continue
        tail = lines[-FAIL_TAIL_LINES:]
        errors = [l for l in lines if ERR_RE.search(l)][-FAIL_ERR_LINES:]
        print(f"last {len(tail)} line(s) of `{log}`:")
        print("```")
        for l in tail:
            print(clean(l).rstrip())
        print("```")
        if errors:
            print(f"error signatures (last {len(errors)} match(es) for `{ERR_RE.pattern}`):")
            print("```")
            for l in errors:
                print(clean(l).rstrip())
            print("```")
PY
