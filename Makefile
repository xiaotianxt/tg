.PHONY: all build check e2e linux-check remote-check install install-local

TG ?= $(CURDIR)/target/tg

all: check

build:
	go build -trimpath -o "$(TG)" ./cmd/tg

# Unit tests plus the black-box contracts in e2e/ against the built binary.
check: build
	test -z "$$(gofmt -l cmd internal e2e)"
	go vet ./...
	TG_BIN="$(TG)" go test -count=1 ./...

e2e: build
	TG_BIN="$(TG)" go test -count=1 ./e2e/...

# Linux cgo build in the release container image (needs docker).
linux-check:
	scripts/linux-check.sh

# Run check on the build host (default: ssh mini) instead of this machine.
remote-check:
	scripts/remote.sh make check

install: build
	sudo cp "$(TG)" /usr/local/bin/tg

install-local: build
	mkdir -p ~/.local/bin
	cp "$(TG)" ~/.local/bin/tg
