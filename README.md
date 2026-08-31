<p align="center">
  <img src="branding/banner.png" width="550" alt="File Browser Lite"/>
</p>

# File Browser Lite

File Browser Lite is a WASI Preview 2 fork of
[File Browser](https://github.com/filebrowser/filebrowser).

It combines a minimal Rust/WASI backend with an adapted File Browser frontend
and packages them into a single `.wasm` component.

## Status

File Browser Lite is experimental and currently intended for local,
trusted-network, and embedded use.

Authentication is not currently included. Do not expose it directly to an
untrusted network.

## Quick Start

Requires a recent version of [Wasmtime](https://wasmtime.dev/) with WASIp2 TCP
support.

Download the latest release:

```bash
curl -L \
  https://github.com/enbop/filebrowser-lite/releases/latest/download/filebrowser-lite-wasi.wasm \
  -o filebrowser-lite-wasi.wasm
```

Create a directory and serve it:

```bash
mkdir -p data

wasmtime run \
  -Scli -Stcp -Sinherit-network \
  --dir data \
  ./filebrowser-lite-wasi.wasm \
  --listen 127.0.0.1:8082
```

Open <http://localhost:8082/>.

Only the directory mounted with `--dir` is exposed to the WASI component.

## Supported Features

- Directory listing
- File metadata
- File download and upload
- Create directories
- Rename and copy files
- Delete files and directories
- Overwrite existing file content

The current WASI build does not include authentication, users, shares, search,
previews, or TUS uploads.

## Development

See [`filebrowser-lite-wasi/README.md`](filebrowser-lite-wasi/README.md) for
source builds and implementation notes.

## License

Licensed under the [Apache License 2.0](LICENSE).
