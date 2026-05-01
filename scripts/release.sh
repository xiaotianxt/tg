#!/usr/bin/env bash
set -euo pipefail

REPO_SLUG="${REPO_SLUG:-xiaotianxt/tg}"
TAP_NAME="${TAP_NAME:-xiaotianxt/tap}"
FORMULA_REF="${FORMULA_REF:-xiaotianxt/tap/tg}"
WORKFLOW="${WORKFLOW:-release.yml}"

RUN_CHECKS=1
UPDATE_TAP=1
BREW_VERIFY=1
WATCH_RELEASE=1
BUMP_KIND="patch"
VERSION_OVERRIDE=""

usage() {
  cat <<'USAGE'
Usage: scripts/release.sh [options]

Create a tg release, bumping Cargo.toml when needed, wait for GitHub
Actions to publish the arm64 artifact, update the Homebrew tap, and verify brew.

Options:
  --bump LEVEL         Bump level when current version is already tagged on
                       another commit.
                       One of: patch, minor, major. Default: patch.
  --version VERSION    Release this exact version, updating Cargo files first.
  --skip-checks        Do not run local checks before tagging.
  --skip-tests         Alias for --skip-checks.
  --skip-tap           Do not update the Homebrew tap formula.
  --skip-brew-verify   Do not run brew update/upgrade/test after tap update.
  --no-watch           Push the tag but do not wait for the release workflow.
  -h, --help           Show this help.

Environment:
  REPO_SLUG            GitHub repo slug. Default: xiaotianxt/tg
  TAP_NAME             Homebrew tap name. Default: xiaotianxt/tap
  FORMULA_REF          Brew formula ref. Default: xiaotianxt/tap/tg
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

package_version() {
  python3 - <<'PY'
from pathlib import Path
import re

text = Path("Cargo.toml").read_text()
match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
if not match:
    raise SystemExit("Cargo.toml version not found")
print(match.group(1))
PY
}

bump_version() {
  python3 - "$1" "$2" <<'PY'
import re
import sys

version = sys.argv[1]
kind = sys.argv[2]
match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
if not match:
    raise SystemExit(f"can only auto-bump x.y.z versions, got: {version}")

major, minor, patch = map(int, match.groups())
if kind == "major":
    major += 1
    minor = 0
    patch = 0
elif kind == "minor":
    minor += 1
    patch = 0
elif kind == "patch":
    patch += 1
else:
    raise SystemExit(f"unknown bump level: {kind}")

print(f"{major}.{minor}.{patch}")
PY
}

set_package_version() {
  python3 - "$1" <<'PY'
from pathlib import Path
import re
import sys

version = sys.argv[1]

def replace_package_version(path, package_name=None):
    lines = path.read_text().splitlines(keepends=True)
    in_package = False
    saw_name = package_name is None

    for i, line in enumerate(lines):
        stripped = line.strip()

        if stripped == "[[package]]" or stripped == "[package]":
            in_package = True
            saw_name = package_name is None
            continue

        if in_package and stripped.startswith("[") and stripped not in {"[package]", "[[package]]"}:
            in_package = False
            saw_name = package_name is None

        if not in_package:
            continue

        if package_name is not None:
            name_match = re.match(r'\s*name\s*=\s*"([^"]+)"', line)
            if name_match:
                saw_name = name_match.group(1) == package_name
                continue

        if saw_name and re.match(r"\s*version\s*=", line):
            lines[i] = re.sub(r'=\s*"[^"]+"', f'= "{version}"', line, count=1)
            path.write_text("".join(lines))
            return

    raise SystemExit(f"package version not found in {path}")

replace_package_version(Path("Cargo.toml"))
replace_package_version(Path("Cargo.lock"), "tg")
PY
}

local_tag_commit() {
  git rev-parse -q --verify "refs/tags/${1}^{}" 2>/dev/null || true
}

remote_tag_commit() {
  local tag="$1"
  local sha

  sha="$(git ls-remote --tags origin "refs/tags/${tag}^{}" | awk '{print $1}')"
  if [[ -z "$sha" ]]; then
    sha="$(git ls-remote --tags origin "refs/tags/${tag}" | awk '{print $1}')"
  fi

  printf '%s' "$sha"
}

tag_commit() {
  local tag="$1"
  local sha

  sha="$(local_tag_commit "$tag")"
  if [[ -z "$sha" ]]; then
    sha="$(remote_tag_commit "$tag")"
  fi

  printf '%s' "$sha"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bump)
      [[ $# -ge 2 ]] || die "--bump requires patch, minor, or major"
      BUMP_KIND="$2"
      case "$BUMP_KIND" in
        patch|minor|major) ;;
        *) die "--bump must be one of: patch, minor, major" ;;
      esac
      shift
      ;;
    --version)
      [[ $# -ge 2 ]] || die "--version requires a version"
      VERSION_OVERRIDE="$2"
      [[ "$VERSION_OVERRIDE" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--version must be x.y.z"
      shift
      ;;
    --skip-checks|--skip-tests)
      RUN_CHECKS=0
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
if [[ "$RUN_CHECKS" -eq 1 ]]; then
  need_cmd make
fi
if [[ "$UPDATE_TAP" -eq 1 || "$BREW_VERIFY" -eq 1 ]]; then
  need_cmd brew
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash changes first"

TAP_DIR=""
FORMULA_PATH=""
if [[ "$UPDATE_TAP" -eq 1 ]]; then
  TAP_DIR="$(brew --repo "$TAP_NAME")"
  FORMULA_PATH="${TAP_DIR}/Formula/tg.rb"
  [[ -f "$FORMULA_PATH" ]] || die "formula not found: ${FORMULA_PATH}"
  [[ -z "$(git -C "$TAP_DIR" status --porcelain)" ]] || die "tap working tree is dirty: ${TAP_DIR}"

  log "updating tap checkout ${TAP_NAME}"
  git -C "$TAP_DIR" pull --ff-only
  [[ -z "$(git -C "$TAP_DIR" status --porcelain)" ]] || die "tap working tree is dirty after pull: ${TAP_DIR}"
fi

log "fetching origin/main and tags"
git fetch origin main --tags

HEAD_SHA="$(git rev-parse HEAD)"
ORIGIN_MAIN_SHA="$(git rev-parse origin/main)"
if [[ "$HEAD_SHA" != "$ORIGIN_MAIN_SHA" ]]; then
  if git merge-base --is-ancestor origin/main HEAD; then
    log "current HEAD is ahead of origin/main"
  else
    die "current HEAD is not origin/main and cannot fast-forward it"
  fi
fi

CURRENT_VERSION="$(package_version)"
CURRENT_TAG="v${CURRENT_VERSION}"
CURRENT_TAG_SHA="$(tag_commit "$CURRENT_TAG")"
VERSION="$CURRENT_VERSION"

if [[ -n "$VERSION_OVERRIDE" ]]; then
  VERSION="$VERSION_OVERRIDE"
elif [[ -n "$CURRENT_TAG_SHA" && "$CURRENT_TAG_SHA" != "$HEAD_SHA" ]]; then
  VERSION="$(bump_version "$CURRENT_VERSION" "$BUMP_KIND")"
fi

TAG="v${VERSION}"
if [[ "$VERSION" != "$CURRENT_VERSION" ]]; then
  TAG_SHA="$(tag_commit "$TAG")"
  [[ -z "$TAG_SHA" ]] || die "tag ${TAG} already exists at ${TAG_SHA}; choose a different version"

  log "bumping Cargo version ${CURRENT_VERSION} -> ${VERSION}"
  set_package_version "$VERSION"
else
  log "using Cargo version ${VERSION}"
fi

TAG_SHA="$(tag_commit "$TAG")"
if [[ -n "$TAG_SHA" && "$TAG_SHA" != "$HEAD_SHA" ]]; then
  die "tag ${TAG} points to ${TAG_SHA}, not HEAD ${HEAD_SHA}; choose a different version"
fi

if [[ "$RUN_CHECKS" -eq 1 ]]; then
  log "running make check"
  make check
fi

if ! git diff --quiet -- Cargo.toml Cargo.lock; then
  log "committing version bump"
  git diff --check -- Cargo.toml Cargo.lock
  git add Cargo.toml Cargo.lock
  git commit -m "chore: bump tg version to ${VERSION}"
  HEAD_SHA="$(git rev-parse HEAD)"
fi

if [[ "$HEAD_SHA" != "$(git rev-parse origin/main)" ]]; then
  log "pushing current HEAD to origin/main"
  git push origin HEAD:main
fi

ASSET_NAME="tg-${TAG}-darwin-arm64.tar.gz"
ASSET_URL="https://github.com/${REPO_SLUG}/releases/download/${TAG}/${ASSET_NAME}"

log "preparing ${TAG}"

if [[ -n "$(local_tag_commit "$TAG")" ]]; then
  log "local tag ${TAG} already exists"
else
  log "creating tag ${TAG}"
  git tag -a "$TAG" -m "$TAG"
fi

REMOTE_TAG_SHA="$(remote_tag_commit "$TAG")"
if [[ -n "$REMOTE_TAG_SHA" ]]; then
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
  log "updating tap ${TAP_NAME}"
  python3 - "$FORMULA_PATH" "$VERSION" "$ASSET_URL" "$ASSET_SHA" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
version = sys.argv[2]
asset_url = sys.argv[3]
sha = sys.argv[4]

text = path.read_text()

def keep_line(pattern):
    match = re.search(pattern, text, re.MULTILINE)
    if not match:
        raise SystemExit(f"formula line not found: {pattern}")
    return match.group(0)

desc = keep_line(r'^  desc .*$')
homepage = keep_line(r'^  homepage .*$')
license_line = keep_line(r'^  license .*$')
native_decoder_dep = '  depends_on "rust-" + "si" + "lk"\n'

path.write_text(f'''class Tg < Formula
{desc}
{homepage}
  url "{asset_url}"
  version "{version}"
  sha256 "{sha}"
{license_line}

  depends_on arch: :arm64
{native_decoder_dep}
  def install
    bin.install "tg"
    generate_completions_from_executable(bin/"tg", "completions")
  end

  test do
    system bin/"tg", "--version"
  end
end
''')
PY

  if git -C "$TAP_DIR" diff --quiet -- Formula/tg.rb; then
    log "tap already points to ${VERSION}"
  else
    git -C "$TAP_DIR" diff --check -- Formula/tg.rb
    git -C "$TAP_DIR" add Formula/tg.rb
    git -C "$TAP_DIR" commit -m "tg ${VERSION}"
    git -C "$TAP_DIR" push origin main
  fi
fi

if [[ "$BREW_VERIFY" -eq 1 ]]; then
  log "verifying Homebrew install"
  brew update
  brew upgrade "$FORMULA_REF" || brew reinstall "$FORMULA_REF"
  tg --version
  brew test "$FORMULA_REF"
fi

log "release ${TAG} complete"
