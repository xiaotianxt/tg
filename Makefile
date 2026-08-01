.PHONY: all fmt clippy test check build install install-local clean

all: check
	$(MAKE) build

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features

check:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --all-features

build:
	cargo build --release

install: build
	sudo cp target/release/tg /usr/local/bin/
	@echo ""
	@echo "安装完成！现在可以直接使用以下命令："
	@echo "  sudo tg keys      # 提取密钥"
	@echo "  tg decrypt        # 解密"
	@echo "  tg sessions       # 查看会话"

install-local: build
	mkdir -p ~/.local/bin
	cp target/release/tg ~/.local/bin/
	@echo ""
	@echo "已安装到 ~/.local/bin，请确保该目录在 PATH 中。"
	@echo "  sudo tg keys      # 提取密钥"
	@echo "  tg decrypt        # 解密"
	@echo "  tg sessions       # 查看会话"

clean:
	cargo clean
	sudo rm -f /usr/local/bin/tg 2>/dev/null || true
	rm -f ~/.local/bin/tg 2>/dev/null || true
