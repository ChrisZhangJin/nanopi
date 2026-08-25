# Local musl + UPX release build.
# Mirrors the linux-x86_64-musl matrix in .github/workflows/release.yml.
# Produces dist/nanopi-v<version>-linux-x86_64-musl.

VERSION  := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
TARGET   := x86_64-unknown-linux-musl
NAME     := nanopi-v$(VERSION)-linux-x86_64-musl
BIN_SRC  := target/$(TARGET)/release/nanopi
BIN_OUT  := dist/$(NAME)

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

ensure-tools:
	@if which upx >/dev/null 2>&1 && which musl-gcc >/dev/null 2>&1; then \
		echo "upx + musl-tools already installed"; \
	else \
		command -v apt-get >/dev/null 2>&1 || { \
			echo "upx or musl-gcc missing and apt-get not available"; \
			echo "install musl-tools and upx manually, then re-run"; \
			exit 1; \
		}; \
		. /etc/os-release 2>/dev/null; \
		case "$$ID" in \
			ubuntu|debian) \
				echo "installing musl-tools + upx-ucl (apt)"; \
				sudo apt-get update && sudo apt-get install -y musl-tools upx-ucl \
			;; \
			*) \
				echo "unsupported distro: $$ID"; \
				echo "install musl-tools and upx manually, then re-run"; \
				exit 1 \
			;; \
		esac; \
	fi

build: ensure-target
	cargo build --release --target $(TARGET)

pack: build
	@mkdir -p dist
	cp $(BIN_SRC) $(BIN_OUT)
	strip $(BIN_OUT) || true
	# UPX-pack: --best --lzma gives ~60% shrink at ~100 ms startup cost,
	# which is unnoticeable for TUI use. || true so a UPX failure (rare
	# on some platforms) doesn't fail the whole build; the unpacked
	# binary is still uploaded.
	upx --best --lzma $(BIN_OUT) || true
	@ls -lh $(BIN_OUT)

clean:
	cargo clean
	rm -rf dist