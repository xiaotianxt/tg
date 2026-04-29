.PHONY: all build scanner install install-local clean

all: build scanner

scanner: vendor/find_all_keys_macos.c
	cc -O2 -o scanner_macos vendor/find_all_keys_macos.c -framework Foundation

build: scanner
	cargo build --release

install: build
	sudo cp scanner_macos /usr/local/bin/
	sudo cp target/release/tg /usr/local/bin/
	@echo ""
	@echo "安装完成！现在可以直接使用以下命令："
	@echo "  sudo tg keys      # 提取密钥"
	@echo "  tg decrypt        # 解密"
	@echo "  tg sessions       # 查看会话"

install-local: build
	mkdir -p ~/.local/bin
	cp scanner_macos ~/.local/bin/
	cp target/release/tg ~/.local/bin/
	@echo ""
	@echo "已安装到 ~/.local/bin，请确保该目录在 PATH 中。"
	@echo "  sudo tg keys      # 提取密钥"
	@echo "  tg decrypt        # 解密"
	@echo "  tg sessions       # 查看会话"

clean:
	rm -f scanner_macos
	cargo clean
	sudo rm -f /usr/local/bin/scanner_macos 2>/dev/null || true
	sudo rm -f /usr/local/bin/tg 2>/dev/null || true
	rm -f ~/.local/bin/scanner_macos 2>/dev/null || true
	rm -f ~/.local/bin/tg 2>/dev/null || true
