#!/usr/bin/env bash
# Build tg and run every test, e2e included, for Linux inside the release container
# image (ubuntu:22.04), then check the binary does not link SQLite or OpenSSL
# dynamically. Run on a host with docker, e.g. scripts/remote.sh scripts/linux-check.sh
set -euo pipefail
arch="${1:-$(uname -m)}"
case "$arch" in arm64|aarch64) platform=linux/arm64 ;; *) platform=linux/amd64 ;; esac
go_version="$(sed -n 's/^go //p' go.mod)"
docker run --rm --platform "$platform" -v "$PWD":/src -w /src \
  -v tg-linux-gocache:/root/.cache/go-build -v tg-linux-gomod:/root/go/pkg/mod \
  ubuntu:22.04 bash -ec "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq --no-install-recommends build-essential libssl-dev ca-certificates curl >/dev/null
    goarch=\$(dpkg --print-architecture)
    curl -fsSL https://go.dev/dl/go${go_version}.linux-\$goarch.tar.gz | tar -C /usr/local -xz
    export PATH=/usr/local/go/bin:\$PATH
    go build -buildvcs=false -o /tmp/tg ./cmd/tg
    TG_BIN=/tmp/tg go test -count=1 ./...
    ! ldd /tmp/tg | grep -E 'lib(sqlite3|sqlcipher|crypto|ssl)'
    echo linux build ok
  "
