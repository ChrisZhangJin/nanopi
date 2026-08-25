# Local musl+UPX release build — design

## Goal

`make` builds a fully-static, stripped, UPX-packed musl binary at
`dist/nanopi-v<version>-linux-x86_64-musl`, mirroring the asset the GitHub
`release` workflow produces for the `linux-x86_64-musl` matrix entry
(see `.github/workflows/release.yml:62-66, 100-136`).

Out of scope: other platforms, debug builds, Windows packaging, asset upload.

## Targets

| Target      | Effect                                                                 |
| ----------- | ---------------------------------------------------------------------- |
| `make`      | Build → strip → UPX-pack → print final size. Default target.            |
| `make check` | Verify the four prerequisites; fail early with a clear install command. |
| `make clean` | `cargo clean` + remove `dist/`.                                        |

## Phases (mirror release.yml steps 91–136 for the musl matrix)

1. **ensure-target** — `rustup target list --installed | grep -q
   x86_64-unknown-linux-musl`; else `rustup target add
   x86_64-unknown-linux-musl`. Mirrors `dtolnay/rust-toolchain@stable`'s
   target install in release.yml:89.
2. **ensure-tools** — Skip if `which upx && which musl-gcc` both succeed.
   Otherwise detect distro via `ID=` from `/etc/os-release`:
   - `ubuntu` / `debian` → `sudo apt-get install -y musl-tools upx-ucl`
     (mirrors release.yml:93, 105).
   - else → fail with a one-line message naming the equivalent packages.
3. **build** — `cargo build --release --target x86_64-unknown-linux-musl`.
   Matches release.yml:101.
4. **pack** — `cp` to `dist/nanopi-v$(VERSION)-linux-x86_64-musl`,
   `strip`, `upx --best --lzma || true`. The `|| true` matches
   release.yml:136's reasoning: a rare UPX failure should not fail the
   whole release; the unpacked binary is still usable.

## Version

Parsed once at top:

```make
VERSION := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
```

Matches the CI asset naming `nanopi-${TAG}-${NAME}` from
release.yml:122 with `TAG=VERSION` and `NAME=linux-x86_64-musl`.

## Error handling

- `make check` is the explicit gate; running the default target invokes
  `ensure-target` and `ensure-tools` first so a missing prerequisite
  produces an early, actionable error instead of a cryptic linker or UPX
  failure later.
- UPX failure is non-fatal (`|| true`), consistent with CI.
- `make clean` is unconditional — no `.PHONY` ordering games.

## Testing

- Manual: `make check` on this dev machine (Debian/Ubuntu) — both
  `upx` and `musl-gcc` are already present, so `ensure-tools` should
  be a no-op.
- Manual: `make` — produces `dist/nanopi-v0.9.2-linux-x86_64-musl`
  (or current version); `ls -lh` confirms size matches CI's ~60% shrink
  target.
- Manual: `./dist/nanopi-v<ver>-linux-x86_64-musl --version` runs
  from any cwd, proving static linking (no glibc dep) and that
  UPX-decompressed startup works.