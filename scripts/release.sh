#!/usr/bin/env bash
set -euo pipefail

REPO_SLUG="${REPO_SLUG:-xiaotianxt/tgreader}"
TAP_NAME="${TAP_NAME:-xiaotianxt/tgreader}"
FORMULA_REF="${FORMULA_REF:-xiaotianxt/tgreader/tgreader}"
WORKFLOW="${WORKFLOW:-release.yml}"

RUN_TESTS=1
UPDATE_TAP=1
BREW_VERIFY=1
WATCH_RELEASE=1

usage() {
  cat <<'USAGE'
Usage: scripts/release.sh [options]

Create a tgreader release from the current Cargo.toml version, wait for GitHub
Actions to publish the arm64 artifact, update the Homebrew tap, and verify brew.

Options:
  --skip-tests         Do not run cargo test before tagging.
  --skip-tap           Do not update the Homebrew tap formula.
  --skip-brew-verify   Do not run brew update/upgrade/test after tap update.
  --no-watch           Push the tag but do not wait for the release workflow.
  -h, --help           Show this help.

Environment:
  REPO_SLUG            GitHub repo slug. Default: xiaotianxt/tgreader
  TAP_NAME             Homebrew tap name. Default: xiaotianxt/tgreader
  FORMULA_REF          Brew formula ref. Default: xiaotianxt/tgreader/tgreader
  WORKFLOW             Release workflow file/name. Default: release.yml
USAGE
}

log() {
  printf '==> %s\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-tests)
      RUN_TESTS=0
      ;;
    --skip-tap)
      UPDATE_TAP=0
      BREW_VERIFY=0
      ;;
    --skip-brew-verify)
      BREW_VERIFY=0
      ;;
    --no-watch)
      WATCH_RELEASE=0
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

need_cmd cargo
need_cmd git
need_cmd gh
need_cmd python3
if [[ "$UPDATE_TAP" -eq 1 || "$BREW_VERIFY" -eq 1 ]]; then
  need_cmd brew
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(python3 - <<'PY'
from pathlib import Path
import re

text = Path("Cargo.toml").read_text()
match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
if not match:
    raise SystemExit("Cargo.toml version not found")
print(match.group(1))
PY
)"
TAG="v${VERSION}"
ASSET_NAME="tgreader-${TAG}-darwin-arm64.tar.gz"
ASSET_URL="https://github.com/${REPO_SLUG}/releases/download/${TAG}/${ASSET_NAME}"

log "preparing ${TAG}"

[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash changes first"

log "fetching origin/main and tags"
git fetch origin main --tags

HEAD_SHA="$(git rev-parse HEAD)"
ORIGIN_MAIN_SHA="$(git rev-parse origin/main)"
if [[ "$HEAD_SHA" != "$ORIGIN_MAIN_SHA" ]]; then
  if git merge-base --is-ancestor origin/main HEAD; then
    log "pushing current HEAD to origin/main"
    git push origin HEAD:main
  else
    die "current HEAD is not origin/main and cannot fast-forward it"
  fi
fi

if [[ "$RUN_TESTS" -eq 1 ]]; then
  log "running cargo test"
  cargo test
fi

if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  TAG_SHA="$(git rev-list -n 1 "$TAG")"
  [[ "$TAG_SHA" == "$HEAD_SHA" ]] || die "local tag ${TAG} points to ${TAG_SHA}, not HEAD ${HEAD_SHA}"
  log "local tag ${TAG} already exists"
else
  log "creating tag ${TAG}"
  git tag "$TAG"
fi

REMOTE_TAG_SHA="$(git ls-remote --tags origin "refs/tags/${TAG}" | awk '{print $1}')"
if [[ -n "$REMOTE_TAG_SHA" ]]; then
  [[ "$REMOTE_TAG_SHA" == "$HEAD_SHA" ]] || die "remote tag ${TAG} points to ${REMOTE_TAG_SHA}, not HEAD ${HEAD_SHA}"
  log "remote tag ${TAG} already exists"
else
  log "pushing tag ${TAG}"
  git push origin "$TAG"
fi

if ! gh release view "$TAG" --repo "$REPO_SLUG" >/dev/null 2>&1; then
  [[ "$WATCH_RELEASE" -eq 1 ]] || die "release ${TAG} does not exist yet; rerun without --no-watch"

  log "waiting for release workflow run"
  RUN_ID=""
  for _ in {1..60}; do
    RUN_ID="$(
      gh run list \
        --repo "$REPO_SLUG" \
        --workflow "$WORKFLOW" \
        --branch "$TAG" \
        --limit 1 \
        --json databaseId \
        --jq '.[0].databaseId // empty'
    )"
    [[ -n "$RUN_ID" ]] && break
    sleep 5
  done
  [[ -n "$RUN_ID" ]] || die "release workflow run for ${TAG} was not found"

  gh run watch "$RUN_ID" --repo "$REPO_SLUG" --exit-status
fi

log "reading release asset digest"
ASSET_SHA="$(
  gh release view "$TAG" \
    --repo "$REPO_SLUG" \
    --json assets \
    --jq ".assets[] | select(.name == \"${ASSET_NAME}\") | .digest // empty"
)"
if [[ "$ASSET_SHA" == sha256:* ]]; then
  ASSET_SHA="${ASSET_SHA#sha256:}"
fi

if [[ -z "$ASSET_SHA" ]]; then
  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT
  gh release download "$TAG" --repo "$REPO_SLUG" --pattern "$ASSET_NAME" --dir "$TMP_DIR"
  ASSET_SHA="$(shasum -a 256 "${TMP_DIR}/${ASSET_NAME}" | awk '{print $1}')"
fi
[[ -n "$ASSET_SHA" ]] || die "could not determine sha256 for ${ASSET_NAME}"

log "asset sha256 ${ASSET_SHA}"

if [[ "$UPDATE_TAP" -eq 1 ]]; then
  TAP_DIR="$(brew --repo "$TAP_NAME")"
  FORMULA_PATH="${TAP_DIR}/Formula/tgreader.rb"
  [[ -f "$FORMULA_PATH" ]] || die "formula not found: ${FORMULA_PATH}"
  [[ -z "$(git -C "$TAP_DIR" status --porcelain)" ]] || die "tap working tree is dirty: ${TAP_DIR}"

  log "updating tap ${TAP_NAME}"
  git -C "$TAP_DIR" pull --ff-only
  python3 - "$FORMULA_PATH" "$VERSION" "$ASSET_URL" "$ASSET_SHA" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
version = sys.argv[2]
asset_url = sys.argv[3]
sha = sys.argv[4]

text = path.read_text()
text = re.sub(r'(?m)^  url ".*"$', f'  url "{asset_url}"', text, count=1)
text = re.sub(r'(?m)^  sha256 ".*"$', f'  sha256 "{sha}"', text, count=1)
text = re.sub(r'(?m)^  version ".*"$', f'  version "{version}"', text, count=1)
path.write_text(text)
PY

  if git -C "$TAP_DIR" diff --quiet -- Formula/tgreader.rb; then
    log "tap already points to ${VERSION}"
  else
    git -C "$TAP_DIR" diff --check -- Formula/tgreader.rb
    git -C "$TAP_DIR" add Formula/tgreader.rb
    git -C "$TAP_DIR" commit -m "tgreader ${VERSION}"
    git -C "$TAP_DIR" push origin main
  fi
fi

if [[ "$BREW_VERIFY" -eq 1 ]]; then
  log "verifying Homebrew install"
  brew update
  brew upgrade "$FORMULA_REF" || brew reinstall "$FORMULA_REF"
  tgreader --version
  brew test "$FORMULA_REF"
fi

log "release ${TAG} complete"
