## Building and running tests

### Prerequisites

- Unix-like environment
- Rust

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
