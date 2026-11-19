# Notes for agents

## Run the CI checks before you commit

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) has three jobs that
run in parallel: `lint`, `test` and `package`. Run the `lint` and `test`
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

When you touch the launcher (`packaging/waytrade`), run the rest of `test`:

```bash
cargo build --locked --manifest-path daemon/Cargo.toml
.github/scripts/test-launcher.sh daemon/target/debug/poe-waytrade-daemon
```

When you touch the layer (`src/`, `CMakeLists.txt`, `assets/`). This needs
GCC 15 for `#embed`:

```bash
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build build
.github/scripts/check-exports.sh build/libVkLayer_poe_waytrade.so
```

The `package` job (`cd packaging/arch && makepkg`) takes about 6 minutes in
CI. Run it locally only when you change `packaging/`.
