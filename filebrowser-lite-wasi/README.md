# WASI backend

Minimal Rust/WASI backend for File Browser Lite. See the
[main README](../README.md) for the project overview and supported features.

## Build From Source

Build the frontend before compiling the WASI component:

```bash
cd frontend
corepack pnpm install --frozen-lockfile
FILEBROWSER_LITE_WASI=1 corepack pnpm exec vite build

cd ../filebrowser-lite-wasi
cargo build --target=wasm32-wasip2 --release
```

To force a fresh WASI rebuild, run `cargo clean` before `cargo build`.

## Run Locally

Run it from the repository root:

```bash
mkdir -p data

wasmtime serve \
  --addr=127.0.0.1:8082 \
  -Scli \
  --dir data \
  ./filebrowser-lite-wasi/target/wasm32-wasip2/release/filebrowser-lite-wasi.wasm
```

## Implementation Notes

- Lite-mode uploads send file bytes as the raw HTTP request body; multipart
  parsing and TUS uploads are not included.
- Request paths are normalized and reject `..` traversal.
- The guest can only access directories mounted with `wasmtime serve --dir`.
- Rebuild the frontend before recompiling the component when frontend assets
  change.
- The current `wstd` stack requires `-Scli` to link the expected `wasi:cli/*`
  imports.
