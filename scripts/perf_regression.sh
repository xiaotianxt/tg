#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE_REF="${TG_PERF_BASELINE:-v1.4.1}"
CANDIDATE_REF="${TG_PERF_CANDIDATE:-WORKTREE}"
RUNS="${TG_PERF_RUNS:-5}"
DECRYPTED_DIR="${TG_PERF_DECRYPTED_DIR:-${HOME}/.tg/decrypted}"
SESSION="${TG_PERF_SESSION:-}"
QUERY="${TG_PERF_QUERY:-项目}"
OUT_DIR="${TG_PERF_OUT_DIR:-}"
FAIL_THRESHOLD="${TG_PERF_FAIL_THRESHOLD:-}"
KEEP_SOURCES=0

usage() {
  cat <<'USAGE'
Usage: scripts/perf_regression.sh [options]

Build two tg binaries and compare local CLI command latency. Command stdout is
discarded so chat content is not written to the report.

Options:
  --baseline REF       Baseline git ref. Default: TG_PERF_BASELINE or v1.4.1.
  --candidate REF      Candidate git ref, or WORKTREE. Default: WORKTREE.
  --runs N             Timing runs per command. Default: TG_PERF_RUNS or 5.
  --session NAME       Session/display name for session-specific commands.
  --query TEXT         Query keyword for query/search cases. Default: 项目.
  --decrypted-dir DIR  Decrypted database dir. Default: ~/.tg/decrypted.
  --out-dir DIR        Report directory. Default: target/perf/<timestamp>.
  --fail-threshold R   Exit nonzero if candidate/baseline median exceeds R.
  --keep-sources       Keep temporary source snapshots for debugging.
  -h, --help           Show this help.

Environment mirrors the long options: TG_PERF_BASELINE, TG_PERF_CANDIDATE,
TG_PERF_RUNS, TG_PERF_SESSION, TG_PERF_QUERY, TG_PERF_DECRYPTED_DIR,
TG_PERF_OUT_DIR, TG_PERF_FAIL_THRESHOLD.
USAGE
}

log() {
  printf '==> %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline)
      [[ $# -ge 2 ]] || die "--baseline requires a ref"
      BASELINE_REF="$2"
      shift
      ;;
    --candidate)
      [[ $# -ge 2 ]] || die "--candidate requires a ref"
      CANDIDATE_REF="$2"
      shift
      ;;
    --runs)
      [[ $# -ge 2 ]] || die "--runs requires a number"
      RUNS="$2"
      shift
      ;;
    --session)
      [[ $# -ge 2 ]] || die "--session requires a value"
      SESSION="$2"
      shift
      ;;
    --query)
      [[ $# -ge 2 ]] || die "--query requires a value"
      QUERY="$2"
      shift
      ;;
    --decrypted-dir)
      [[ $# -ge 2 ]] || die "--decrypted-dir requires a path"
      DECRYPTED_DIR="$2"
      shift
      ;;
    --out-dir)
      [[ $# -ge 2 ]] || die "--out-dir requires a path"
      OUT_DIR="$2"
      shift
      ;;
    --fail-threshold)
      [[ $# -ge 2 ]] || die "--fail-threshold requires a ratio"
      FAIL_THRESHOLD="$2"
      shift
      ;;
    --keep-sources|--keep-worktrees)
      KEEP_SOURCES=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
  shift
done

[[ "$RUNS" =~ ^[0-9]+$ ]] || die "--runs must be a positive integer"
[[ "$RUNS" -gt 0 ]] || die "--runs must be greater than 0"
if [[ -n "$FAIL_THRESHOLD" ]]; then
  [[ "$FAIL_THRESHOLD" =~ ^[0-9]+([.][0-9]+)?$ ]] || die "--fail-threshold must be a numeric ratio"
fi
[[ -d "$DECRYPTED_DIR" ]] || die "decrypted dir not found: $DECRYPTED_DIR"

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="${ROOT}/target/perf/$(date '+%Y%m%d-%H%M%S')"
fi
mkdir -p "$OUT_DIR"
SOURCE_ROOT="${OUT_DIR}/sources"
BUILD_ROOT="${ROOT}/target/perf-build"
mkdir -p "$SOURCE_ROOT" "$BUILD_ROOT"

cleanup() {
  if [[ "$KEEP_SOURCES" -eq 0 && -d "$SOURCE_ROOT" ]]; then
    rm -rf "$SOURCE_ROOT"
  fi
}
trap cleanup EXIT

build_binary() {
  local label="$1"
  local ref="$2"
  local source_dir
  local target_dir="${BUILD_ROOT}/${label}"

  if [[ "$ref" == "WORKTREE" ]]; then
    source_dir="$ROOT"
  else
    source_dir="${SOURCE_ROOT}/${label}"
    rm -rf "$source_dir"
    mkdir -p "$source_dir"
    if ! git -C "$ROOT" archive "$ref" | tar -x -C "$source_dir"; then
      return 1
    fi
  fi

  log "building ${label} (${ref})"
  if ! CARGO_TARGET_DIR="$target_dir" cargo build \
    --release \
    --bin tg \
    --manifest-path "${source_dir}/Cargo.toml" >/dev/null; then
    return 1
  fi
  printf '%s/release/tg\n' "$target_dir"
}

measure_case_once() {
  local bin="$1"
  local case_name="$2"

  case "$case_name" in
    sessions)
      measure_once "$bin" sessions --decrypted-dir "$DECRYPTED_DIR" --top 30 --jobs 1
      ;;
    messages)
      measure_once "$bin" messages "$SESSION" --decrypted-dir "$DECRYPTED_DIR" --limit 50 --jobs 1
      ;;
    query)
      measure_once "$bin" query --decrypted-dir "$DECRYPTED_DIR" --session "$SESSION" --contains "$QUERY" --fields time,sender,body --limit 50 --jobs 1
      ;;
    image-list)
      measure_once "$bin" image "$SESSION" --decrypted-dir "$DECRYPTED_DIR" --list --limit 20 --jobs 1
      ;;
    voice-list)
      measure_once "$bin" voice "$SESSION" --decrypted-dir "$DECRYPTED_DIR" --list --limit 20 --jobs 1
      ;;
    *)
      return 1
      ;;
  esac
}

case_requires_session() {
  case "$1" in
    messages|query|image-list|voice-list)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

measure_once() {
  local bin="$1"
  shift
  local time_file
  local status

  time_file="$(mktemp "${OUT_DIR}/time.XXXXXX")"
  if /usr/bin/time -p "$bin" "$@" >/dev/null 2>"$time_file"; then
    status=0
  else
    status=$?
  fi

  if [[ "$status" -ne 0 ]]; then
    rm -f "$time_file"
    return "$status"
  fi

  awk '/^real / { print $2; found=1 } END { if (!found) exit 1 }' "$time_file"
  rm -f "$time_file"
}

median() {
  sort -n | awk '
    { values[NR] = $1 }
    END {
      if (NR == 0) exit 1
      if (NR % 2 == 1) {
        printf "%.6f", values[(NR + 1) / 2]
      } else {
        printf "%.6f", (values[NR / 2] + values[NR / 2 + 1]) / 2
      }
    }
  '
}

join_samples() {
  awk 'BEGIN { out="" } { out = out (out == "" ? "" : " ") $1 } END { print out }'
}

run_case() {
  local binary_label="$1"
  local ref="$2"
  local bin="$3"
  local case_name="$4"
  local samples=()
  local sample
  local status
  local i

  if case_requires_session "$case_name" && [[ -z "$SESSION" ]]; then
    printf '%s,%s,%s,skip,no-session,,\n' "$case_name" "$binary_label" "$ref" >>"$CSV"
    return 0
  fi

  if measure_case_once "$bin" "$case_name" >/dev/null; then
    status=0
  else
    status=$?
  fi

  if [[ "$status" -ne 0 ]]; then
    printf '%s,%s,%s,skip,unsupported-or-failed,,\n' "$case_name" "$binary_label" "$ref" >>"$CSV"
    return 0
  fi

  for i in $(seq 1 "$RUNS"); do
    sample="$(measure_case_once "$bin" "$case_name")" || {
      printf '%s,%s,%s,skip,failed-during-run,,\n' "$case_name" "$binary_label" "$ref" >>"$CSV"
      return 0
    }
    samples+=("$sample")
  done

  local med
  med="$(printf '%s\n' "${samples[@]}" | median)"
  local joined
  joined="$(printf '%s\n' "${samples[@]}" | join_samples)"
  printf '%s,%s,%s,%s,ok,%s,"%s"\n' \
    "$case_name" "$binary_label" "$ref" "$RUNS" "$med" "$joined" >>"$CSV"
}

BASELINE_BIN="$(build_binary baseline "$BASELINE_REF")" || die "failed to build baseline ${BASELINE_REF}"
CANDIDATE_BIN="$(build_binary candidate "$CANDIDATE_REF")" || die "failed to build candidate ${CANDIDATE_REF}"

resolve_sha() {
  if [[ "$1" == "WORKTREE" ]]; then
    git -C "$ROOT" rev-parse HEAD
  else
    git -C "$ROOT" rev-parse "$1^{commit}"
  fi
}

BASELINE_SHA="$(resolve_sha "$BASELINE_REF")"
CANDIDATE_SHA="$(resolve_sha "$CANDIDATE_REF")"

cat >"${OUT_DIR}/metadata.txt" <<EOF
baseline_ref=${BASELINE_REF}
baseline_sha=${BASELINE_SHA}
candidate_ref=${CANDIDATE_REF}
candidate_sha=${CANDIDATE_SHA}
runs=${RUNS}
session_set=$([[ -n "$SESSION" ]] && printf yes || printf no)
decrypted_dir_set=yes
fail_threshold=${FAIL_THRESHOLD:-}
system=$(uname -srm)
rustc=$(rustc --version)
EOF

CSV="${OUT_DIR}/summary.csv"
printf 'case,binary,ref,runs,status,median_seconds,samples\n' >"$CSV"

CASES="sessions messages query image-list voice-list"
for case_name in $CASES; do
  log "benchmarking ${case_name}"
  run_case baseline "$BASELINE_REF" "$BASELINE_BIN" "$case_name"
  run_case candidate "$CANDIDATE_REF" "$CANDIDATE_BIN" "$case_name"
done

PERF_STATUS=0
if awk -F, -v fail_threshold="$FAIL_THRESHOLD" '
  BEGIN {
    printf "\n%-14s %-11s %-11s %-9s %-8s\n", "case", "baseline", "candidate", "ratio", "status"
    printf "%-14s %-11s %-11s %-9s %-8s\n", "--------------", "-----------", "-----------", "---------", "--------"
  }
  NR > 1 {
    key = $1
    if (!(key in seen)) {
      seen[key] = 1
      order[++order_count] = key
    }
    if ($2 == "baseline") {
      b[key] = $6
      bs[key] = $5
    } else if ($2 == "candidate") {
      c[key] = $6
      cs[key] = $5
    }
  }
  END {
    for (i = 1; i <= order_count; i++) {
      key = order[i]
      if (bs[key] != "ok" || cs[key] != "ok") {
        printf "%-14s %-11s %-11s %-9s %-8s\n", key, bs[key], cs[key], "-", "skip"
      } else {
        ratio = c[key] / b[key]
        status = ratio > 1.20 ? "slower" : (ratio < 0.80 ? "faster" : "ok")
        ratio_text = sprintf("%.2fx", ratio)
        printf "%-14s %-11.6f %-11.6f %-9s %-8s\n", key, b[key], c[key], ratio_text, status
        if (fail_threshold != "" && ratio > fail_threshold + 0) {
          failed = 1
        }
      }
    }
    exit failed ? 3 : 0
  }
' "$CSV"; then
  PERF_STATUS=0
else
  PERF_STATUS=$?
fi

printf '\nReport: %s\n' "$OUT_DIR"
exit "$PERF_STATUS"
