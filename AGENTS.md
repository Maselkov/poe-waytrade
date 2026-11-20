# Notes for agents

## Run the CI checks before you commit

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs `lint`, `test`
and `layer` in parallel on every pull request and push to main, and `package`
only on pushes to main. Run the `lint` and `test`
commands before every commit that touches `daemon/`:

```bash
cd daemon
cargo fmt --check                                   # rustfmt.toml: max_width = 120
cargo clippy --locked --all-targets -- -D warnings  # any warning fails CI
cargo test --locked
```

If `cargo fmt --check` complains, run `cargo fmt` and commit the result. Don't
hand-wrap lines to 120 columns; let rustfmt do it.

CI uses Arch's `rust` package, which is the latest stable toolchain. A newer
clippy can add lints, so keep the local toolchain current
(`rustup update stable`), or a clean local run can still fail in CI.

The daemon's tests for poe.ninja and trade-site replies run against real
replies saved in `daemon/fixtures/`. When you change how those replies are
read, or suspect the APIs changed, re-capture them and re-run the tests (a
dozen requests, no account needed):

```bash
python3 daemon/fixtures/refresh.py
```

When you touch the launcher (`packaging/waytrade`), run the rest of `test`:

```bash
cargo build --locked --manifest-path daemon/Cargo.toml
.github/scripts/test-launcher.sh daemon/target/debug/poe-waytrade-daemon
```

When you touch the layer (`src/`, `CMakeLists.txt`, `assets/`), run what the
`layer` job runs. This needs GCC 15 for `#embed`:

```bash
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build build
.github/scripts/check-exports.sh build/libVkLayer_poe_waytrade.so
```

The `package` job (`cd packaging/arch && makepkg`) takes about 4 minutes in
CI and doesn't run on pull requests, so a broken package shows up only after
the merge. Run it locally when you change `packaging/`.

## Versions and releases

`pkgver` in `packaging/arch/PKGBUILD` is the release version, and
`daemon/Cargo.toml`'s `version` must equal it (the `lint` job checks). To bump,
change both, run `cargo update -p poe-waytrade-daemon --offline` in `daemon/`,
and commit. To release, tag that commit `v<pkgver>` and push the tag:
`release.yml` builds it and makes the GitHub release.

The binaries report the exact build from `git describe` (`daemon/build.rs`,
`cmake/version.cmake`): `0.21.0` on the tag, `0.21.0-5-gcf90910` after it,
`-dirty` with uncommitted changes. The daemon prints it with `--version` and at
start; the layer logs it when the game creates its Vulkan instance.
