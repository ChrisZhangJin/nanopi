# Plan: Centralize version into a `VERSION` file

## Context

`nanopi -V` currently prints `nanopi 0.9.3` (from a stale symlink to an old binary). The actual current version is `0.10.0`. The user wants a single source of truth for the version — a `VERSION` file at the repo root — so that:
- `nanopi -V` always prints the correct version (embedded at compile time).
- `make` and CI read from the same file.
- `make bump VERSION=x.y.z` is the one-stop update command.

## Files to change

| File | Action |
|---|---|
| `VERSION` (new) | One line: `0.10.0` |
| `src/main.rs` (~line 1-28) | Add `fn nanopi_version()` using `include_str!("../VERSION").trim()`, use it in `#[command(version = nanopi_version())]` |
| `Makefile` (line 5) | Read from `VERSION` with fallback: `$(shell cat VERSION 2>/dev/null \|\| grep '^version' Cargo.toml …)` |
| `.github/workflows/release.yml` (~line 100) | Add a step that validates `VERSION` matches the git tag before building |
| `Makefile` (new target) | `bump:` target — updates both `VERSION` and `Cargo.toml`; prints usage if `VERSION` not supplied |

## Detailed changes

### 1. Create `VERSION` (new file)

```
0.10.0
```

Plain text, one line.

### 2. `src/main.rs` — embed VERSION at compile time

Add before the `Args` struct (~line 26):

```rust
/// Version string from the repo-root `VERSION` file, baked in at compile time.
fn nanopi_version() -> &'static str {
    include_str!("../VERSION").trim()
}
```

Change line 28:

```rust
// Before:
#[command(name = "nanopi", version, about = "minimal Pi port in Rust")]

// After:
#[command(name = "nanopi", version = nanopi_version(), about = "minimal Pi port in Rust")]
```

This replaces clap's default `CARGO_PKG_VERSION` with the content of the `VERSION` file. `include_str!` is a compile-time macro; `.trim()` strips the trailing newline; the returned `&'static str` has the same lifetime as the embedded data.

### 3. `Makefile` line 5 — read VERSION with fallback

```makefile
# Before:
VERSION  := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)

# After:
VERSION  := $(shell cat VERSION 2>/dev/null || grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
```

If `VERSION` exists and is readable, use it; otherwise fall back to grepping `Cargo.toml`.

### 4. `release.yml` — validate VERSION ↔ git tag

Add after the `checkout` step (~line 84) in the `build` job:

```yaml
      - name: validate VERSION matches tag
        if: github.event_name == 'push'
        shell: bash
        run: |
          set -eux
          TAG="${{ github.ref_name }}"
          FILE_V="$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)"
          EXPECTED="v${FILE_V}"
          if [ "$TAG" != "$EXPECTED" ]; then
            echo "::error::VERSION file ($FILE_V) does not match git tag ($TAG). Update VERSION and re-tag."
            exit 1
          fi
```

This only runs on tag-push (not `workflow_dispatch`) so manual back-fill still works. It catches mismatches early — before building all four platform binaries.

### 5. New `bump` target in Makefile

Append at the end of the Makefile:

```makefile
# Usage: make bump VERSION=x.y.z
# Updates VERSION file and Cargo.toml, then reminds you to commit + tag.
bump:
ifndef VERSION
	@echo "Usage: make bump VERSION=x.y.z"
	@echo "  Updates VERSION file and Cargo.toml to the specified version."
	@exit 1
endif
	@echo -n "$(VERSION)" > VERSION
	@sed -i 's/^version = .*/version = "$(VERSION)"/' Cargo.toml
	@echo "Bumped to $(VERSION) — VERSION file and Cargo.toml updated."
	@echo "Next steps:"
	@echo "  git commit -am 'chore: bump to v$(VERSION)'"
	@echo "  git tag v$(VERSION)"
	@echo "  git push && git push --tags"
```

## Verification

1. `cargo build --release` and `./target/release/nanopi -V` → prints `nanopi 0.10.0`
2. `make` (clean build) → still works, version embedded correctly in UPX-packed binary
3. `make bump VERSION=0.11.0` → updates VERSION and Cargo.toml; prints next-steps reminder
4. `make bump` (no VERSION) → prints usage and exits non-zero
5. Push a tag `v0.10.0` → release.yml validates VERSION matches before building
