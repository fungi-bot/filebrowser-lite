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

wasmtime run \
  -Scli -Stcp -Sinherit-network \
  --dir data \
  ./filebrowser-lite-wasi/target/wasm32-wasip2/release/filebrowser-lite-wasi.wasm \
  --listen 127.0.0.1:8082
```

## Implementation Notes

- Lite-mode uploads send file bytes as the raw HTTP request body; multipart
  parsing and TUS uploads are not included.
- Request paths are normalized and reject `..` traversal.
- One long-running component process owns the Tokio runtime and HTTP listener,
  so all requests share the same process-level state.
- The guest can only access directories mounted with `wasmtime run --dir`.
- Rebuild the frontend before recompiling the component when frontend assets
  change.
- The repository's `.cargo/config.toml` enables Tokio's unstable WASIp2 network
  support during native and component builds.
