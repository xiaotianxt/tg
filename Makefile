.PHONY: all build clean

all: build scanner

scanner: vendor/find_all_keys_macos.c
	cc -O2 -o scanner_macos vendor/find_all_keys_macos.c -framework Foundation

build: scanner
	cargo build --release

clean:
	rm -f scanner_macos
	cargo clean
