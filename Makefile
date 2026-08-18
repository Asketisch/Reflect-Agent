# ── Reflect Makefile ───────────────────────────────────────────────
.PHONY: build run test test-fast test-changed check check-fast watch clean gc gc-deep dmg dist dist-dry help

BINARY_NAME := reflect
INSTALL_DIR := /usr/local/bin

## build: 以 release 模式编译(等价于 scripts/build.sh --dry)
build:
	cargo build --release -p reflect-cli

## run: 构建并直接启动 TUI
run: build
	./target/release/$(BINARY_NAME)

## install: 构建 + 安装到 /usr/local/bin
install:
	scripts/build.sh

## dmg: 构建 macOS DMG 安装包(dist/reflect-<ver>-<triple>.dmg)
dmg:
	scripts/build-dmg.sh

## dist: 构建并打包当前平台分发包到 dist/(DMG / tar.gz / zip)
dist:
	scripts/build.sh --dist

## dist-dry: 预览 dist 打包命令(不实际执行)
dist-dry:
	scripts/build.sh --dist --dry-run

## test: 跑全部测试(cargo test,全量兼容)
test:
	cargo test --workspace

## test-fast: 用 nextest 跑全部测试(进程级并行,更快;需 cargo-nextest)
test-fast:
	cargo nextest run --workspace

## test-changed: 只跑自上次提交以来受变更影响的测试(开发循环用)
test-changed:
	cargo nextest run --changed-since HEAD

## check: 全 workspace 编译检查(类型检查,不链接)
check:
	cargo check --workspace

## check-fast: 快速类型检查单个 crate(示例:make check-fast C=reflect-tui)
check-fast:
	cargo check -p $(C)

## watch: 文件变更自动 check(需先 cargo install cargo-watch)
watch:
	cargo watch -x check

## clean: 移除构建产物(全清,等同于 `cargo clean`)
clean:
	cargo clean

## gc: 清掉 debug 里 cargo 不会自动 GC 的旧中间产物
## (incremental/.fingerprint 是 30+ GB 的元凶). 主二进制和 .rlib 保留,
## 下次 cargo build 只需补全缺失依赖。日常开发循环使用。
gc:
	rm -rf target/debug/incremental target/debug/.fingerprint
	rm -rf target/debug/examples
	@echo "✅ removed target/debug/{incremental,.fingerprint,examples}"

## gc-deep: 同 gc,再清空 deps 里所有 *.o/*.d/旧 .rlib。下次构建接近冷构建,
## 但能多释放 ~10-15 GB。仅当磁盘告警时使用。
gc-deep: gc
	rm -rf target/debug/deps
	@echo "✅ removed target/debug/deps (下次构建接近冷构建)"

## help: 显示本帮助信息
help:
	@echo "可用目标:"
	@echo "  build         - 以 release 模式编译"
	@echo "  run           - 构建并启动 TUI"
	@echo "  install       - 构建 + 安装到 $(INSTALL_DIR)"
	@echo "  dmg           - 构建 macOS DMG 安装包(dist/)"
	@echo "  dist          - 构建并打包当前平台分发包(DMG/tar.gz/zip)到 dist/"
	@echo "  dist-dry      - 预览 dist 打包命令(不实际执行)"
	@echo "  test          - cargo test(全量兼容)"
	@echo "  test-fast     - nextest 加速跑全部测试(需 cargo-nextest)"
	@echo "  test-changed  - nextest 只跑变更影响测试(开发循环)"
	@echo "  check         - cargo check 全 workspace"
	@echo "  check-fast    - cargo check 单 crate:make check-fast C=<crate>"
	@echo "  watch         - 文件变更自动 check(需 cargo-watch)"
	@echo "  clean         - 移除全部构建产物(cargo clean)"
	@echo "  gc            - 清 incremental/.fingerprint/examples(-30GB),保留 .rlib"
	@echo "  gc-deep       - 同 gc,再清 deps(-15GB,下次近冷构建)"
