# Local musl + UPX release build.
# Mirrors the linux-x86_64-musl matrix in .github/workflows/release.yml.
# Produces dist/nanopi-v<version>-linux-x86_64-musl.

VERSION  := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
TARGET   := x86_64-unknown-linux-musl
NAME     := nanopi-v$(VERSION)-linux-x86_64-musl
BIN_SRC  := target/$(TARGET)/release/nanopi
BIN_OUT  := dist/$(NAME)

# Empty when already root (containers, CI). Prefixing apt-get with a
# literal `sudo` breaks every root environment with "sudo: not found".
SUDO := $(shell [ "$$(id -u)" = 0 ] || command -v sudo 2>/dev/null)

.PHONY: all check clean ensure-target ensure-tools build pack

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

clean:
	cargo clean
	rm -rf dist
