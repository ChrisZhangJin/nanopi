# Local musl + UPX release build.
# Mirrors the linux-x86_64-musl matrix in .github/workflows/release.yml.
# Produces dist/nanopi-v<version>-linux-x86_64-musl.

VERSION  := $(shell cat VERSION 2>/dev/null || grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
TARGET   := x86_64-unknown-linux-musl
NAME     := nanopi-v$(VERSION)-linux-x86_64-musl
BIN_SRC  := target/$(TARGET)/release/nanopi
BIN_OUT  := dist/$(NAME)

# Empty when already root (containers, CI). Prefixing apt-get with a
# literal `sudo` breaks every root environment with "sudo: not found".
SUDO := $(shell [ "$$(id -u)" = 0 ] || command -v sudo 2>/dev/null)

# WASM extension support is a build-time opt-in: the shipped release
# binary carries no runtime, so `[[extensions]]` entries are ignored
# with a warning. These targets are the other build.
WASM_NAME    := $(NAME)-wasm
WASM_BIN_OUT := dist/$(WASM_NAME)
WASM_DBG_BIN := target/debug/nanopi
PLUGIN_SRC   := examples/wasm-plugin
PLUGIN_WASM  := $(PLUGIN_SRC)/target/wasm32-wasip1/release/nanopi_example_plugin.wasm
PLUGIN_OUT   := dist/nanopi-example-plugin.component.wasm

.PHONY: all check clean ensure-target ensure-tools build pack \
        wasm wasm-debug build-wasm plugin test-wasm \
        ensure-wasm-tools ensure-musl-cc

all: pack

check: ensure-target ensure-tools
	@echo "all prerequisites present"

ensure-target:
	@command -v rustup >/dev/null 2>&1 || { \
		echo "rustup not found on PATH; install rustup (https://rustup.rs) or add ~/.cargo/bin to PATH"; \
		exit 1; \
	}
	@if ! rustup target list --installed | grep -q $(TARGET); then \
		echo "installing rustup target $(TARGET)"; \
		rustup target add $(TARGET); \
	fi

# Only upx. `musl-tools` used to be required here, but Rust's musl
# rust-std links self-contained — a clean build of this project (ring's
# C included) succeeds with musl-gcc absent, so demanding it just forced
# a pointless apt install.
#
# Never fatal: `pack` still produces a working, merely-unpacked binary
# without upx. It says so loudly instead.
ensure-tools:
	@if command -v upx >/dev/null 2>&1; then \
		echo "upx present"; \
	elif command -v apt-get >/dev/null 2>&1 && . /etc/os-release 2>/dev/null && \
	     case "$$ID$$ID_LIKE" in *debian*|*ubuntu*) true;; *) false;; esac; then \
		echo "installing upx-ucl (apt)"; \
		$(SUDO) apt-get update && $(SUDO) apt-get install -y --no-install-recommends upx-ucl || \
			echo "WARNING: upx install failed — continuing unpacked"; \
	else \
		echo "WARNING: upx not found and cannot auto-install on this system."; \
		echo "         Install it manually for a packed binary; the build still works."; \
	fi

build: ensure-target
	cargo build --release --target $(TARGET)

# ensure-tools belongs here, not only on `check`: packing is what needs
# upx, and for a long time nothing in the default `make` path ran
# ensure-tools at all, so upx was never installed and the `|| true`
# below swallowed its absence. `make` exited 0 having shipped a 4.4 MB
# binary where a 1.6 MB one was intended.
pack: build ensure-tools
	@mkdir -p dist
	cp $(BIN_SRC) $(BIN_OUT)
	strip $(BIN_OUT) || true
	@# UPX-pack: --best --lzma shrinks ~2.7x at ~100 ms startup cost,
	@# which is unnoticeable for TUI use. A upx failure must not fail the
	@# whole build (the unpacked binary is still usable) but it must not
	@# pass unnoticed either — that is the bug this warning exists for.
	@if command -v upx >/dev/null 2>&1; then \
		upx --best --lzma $(BIN_OUT) || \
			echo "WARNING: upx failed; $(BIN_OUT) is left UNPACKED"; \
	else \
		echo "WARNING: upx unavailable; $(BIN_OUT) is UNPACKED (~2.7x larger)"; \
	fi
	@ls -lh $(BIN_OUT)

# ─── WASM extensions ────────────────────────────────────────────────

# A host binary that can load plugins: same musl + UPX pipeline as
# `pack` above, just with the runtime compiled in. Output lands beside
# the stock artifact as dist/nanopi-v<version>-linux-x86_64-musl-wasm.
#
# RELEASE, and not for size reasons. wasmtime embeds cranelift, a JIT
# compiler, and an unoptimized build is ~10x slower at the one thing it
# does here — measured startup-through-plugin-load on the example
# plugin: 962 ms debug vs 138 ms release. That cost is paid again on
# every `/new`, `/resume`, and `/fork`, which is slow enough to read as
# a bug in the plugin system rather than as a debug build. Use
# `make wasm-debug` when you actually need a backtrace.
#
# What the runtime actually costs, measured on this project:
#
#                          size      startup+plugin load
#   musl, no wasm          4.1 MB    —
#   musl + wasm            7.2 MB    138 ms
#   musl + wasm + UPX      2.5 MB    329 ms
#
# So UPX is a bigger win here than on the stock binary (66% off) and a
# bigger cost too: ~190 ms, not the ~100 ms quoted above `pack`. That
# is decompression, paid once per process launch — unlike the cranelift
# cost, it is NOT paid again on `/new`.
#
# NOTE: this and `build` write the same path,
# target/$(TARGET)/release/nanopi, with different feature sets. Running
# `make` after `make wasm` (or the reverse) therefore triggers a full
# rebuild rather than reusing the artifact. Both copy into dist/ under
# distinct names, so the outputs never clobber each other.
wasm: build-wasm ensure-tools
	@mkdir -p dist
	cp $(BIN_SRC) $(WASM_BIN_OUT)
	strip $(WASM_BIN_OUT) || true
	@if command -v upx >/dev/null 2>&1; then \
		upx --best --lzma $(WASM_BIN_OUT) || \
			echo "WARNING: upx failed; $(WASM_BIN_OUT) is left UNPACKED"; \
	else \
		echo "WARNING: upx unavailable; $(WASM_BIN_OUT) is UNPACKED (~3x larger)"; \
	fi
	@ls -lh $(WASM_BIN_OUT)
	@echo "point it at a plugin with an [[extensions]] entry in config.toml"

build-wasm: ensure-target ensure-musl-cc
	cargo build --release --target $(TARGET) --features wasm

# musl-tools IS required for this build, unlike the stock one. The
# comment above `ensure-tools` explains why it was dropped there: Rust's
# musl rust-std links self-contained. wasmtime breaks that assumption —
# it carries a cc-rs build step, and without a musl C compiler the build
# dies at
#   error occurred in cc-rs: failed to find tool "x86_64-linux-musl-gcc"
# which names a tool, not a cause, and sends you looking in the wrong
# place. Fail early with the fix instead.
ensure-musl-cc:
	@if command -v x86_64-linux-musl-gcc >/dev/null 2>&1 || command -v musl-gcc >/dev/null 2>&1; then \
		echo "musl C compiler present"; \
	elif command -v apt-get >/dev/null 2>&1 && . /etc/os-release 2>/dev/null && \
	     case "$$ID$$ID_LIKE" in *debian*|*ubuntu*) true;; *) false;; esac; then \
		echo "installing musl-tools (apt) — wasmtime's cc-rs step needs it"; \
		$(SUDO) apt-get update && $(SUDO) apt-get install -y --no-install-recommends musl-tools; \
	else \
		echo "ERROR: musl-tools not found and cannot auto-install here."; \
		echo "       wasmtime's cc-rs build step needs x86_64-linux-musl-gcc."; \
		exit 1; \
	fi

# Same binary, unoptimized, on the native target — musl is pointless
# for a debug build you are not shipping. ~10x slower to load a plugin
# and ~30x larger (233 MB, mostly debuginfo); take it only when you
# need a backtrace or a debugger.
wasm-debug:
	cargo build --features wasm
	@ls -lh $(WASM_DBG_BIN) | awk '{print "built " $$9 " (" $$5 ") — unoptimized, expect slow plugin loads"}'

# The example plugin, as a loadable component. Three steps, and the
# middle one is the one everybody forgets: `cargo build` alone emits a
# core MODULE, which the host rejects — `component embed` bakes in the
# WIT world and `component new` wraps it as a component.
#
# --world extension-commands, NOT extension: this example registers
# slash commands. `wit/` declares two worlds, so --world is no longer
# optional. A tool-only plugin should target `extension`.
plugin: ensure-wasm-tools
	cargo build --manifest-path $(PLUGIN_SRC)/Cargo.toml \
		--target wasm32-wasip1 --release
	@mkdir -p dist
	wasm-tools component embed wit/ $(PLUGIN_WASM) \
		-o dist/embedded.wasm --world extension-commands
	wasm-tools component new dist/embedded.wasm -o $(PLUGIN_OUT)
	@rm -f dist/embedded.wasm
	@echo
	@echo "built $(PLUGIN_OUT) — exports:"
	@wasm-tools component wit $(PLUGIN_OUT) | grep -E '^\s+export' || true

ensure-wasm-tools:
	@command -v wasm-tools >/dev/null 2>&1 || { \
		echo "wasm-tools not found: cargo install wasm-tools --locked"; \
		exit 1; \
	}
	@# wasm32-wasip1 may be installed without rustup knowing (a manual
	@# std drop-in), so check the toolchain dir rather than trusting
	@# `rustup target list --installed`, which would report it missing
	@# and send you to install something you already have.
	@ls -d $$(rustc --print sysroot)/lib/rustlib/wasm32-wasip1 >/dev/null 2>&1 || { \
		echo "wasm32-wasip1 std not found: rustup target add wasm32-wasip1"; \
		exit 1; \
	}

# --test-threads=1 is NOT optional. Several tests mutate process-global
# env (NANOPI_HOME) under a lock that a panicking test poisons, so a
# parallel run fails a varying subset — roughly 1 run in 3, on an
# unmodified tree. Serial is deterministic.
test-wasm:
	cargo test --features wasm -- --test-threads=1

# Usage: make bump VERSION=x.y.z
# Updates the VERSION file and the Cargo.toml `version` line.
bump:
ifndef VERSION
	@echo "Usage: make bump VERSION=x.y.z"
	@exit 1
endif
	@echo -n "$(VERSION)" > VERSION
	@sed -i 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	@echo "Updated VERSION and Cargo.toml to $(VERSION)."
	@echo "Next: git commit -am 'chore: bump to v$(VERSION)' && git tag v$(VERSION) && git push && git push --tags"

clean:
	cargo clean
	rm -rf dist
