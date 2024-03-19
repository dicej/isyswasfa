## Building and running tests

### Prerequisites

- Unix-like environment
- Rust stable 1.71 or later *and* nightly 2023-07-27 or later, including the `wasm32-wasi` and `wasm32-unknown-unknown` targets
    - See https://github.com/bytecodealliance/componentize-py/blob/main/CONTRIBUTING.md for details

First, make sure you have all the submodules cloned:

```shell
git submodule update --init --recursive
```

Next, grab a `wasi-sockets`-enabled build of `wasi-sdk` (replace `linux` with `macos` or `mingw` (Windows) as appropriate):

```shell
curl -LO https://github.com/dicej/wasi-sdk/releases/download/wasi-sockets-alpha-2/wasi-sdk-20.26g68203b20b82e-linux.tar.gz
tar xf tar xf wasi-sdk-20.26g68203b20b82e-linux.tar.gz
sudo mv wasi-sdk-20.26g68203b20b82e /opt/wasi-sdk
export WASI_SDK_PATH=/opt/wasi-sdk
```

Finally, build and run the tests:

```shell
cargo test --release
```
