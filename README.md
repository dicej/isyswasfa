## isyswasfa: I sync, you sync, we all sync for async

An experimental polyfill for composable concurrency based on the [WebAssembly Component Model](https://github.com/WebAssembly/component-model) and [WASI](https://github.com/WebAssembly/WASI) 0.2.x

### Background

As of this writing, the Component Model does not support concurrent, composable execution.  Although WASI 0.2.x includes support for asynchronous I/O via the `wasi:io/poll` interface, it does not compose well: only one component in a composition can block at a time.  A major goal for WASI 0.3.0 is to provide built-in support for "composable async" in the Component Model, thereby resolving the tension between composition and concurrency.

### So what is this?

A pile of hacks -- but a _useful_ pile of hacks.  The goals are:

- To provide early, real-world implementation feedback to the Component Model "async" design process
- To give developers a tool for "polyfilling" composable concurrency on top of WASI 0.2.x, ideally in such a way that upgrading application code to 0.3.0 requires little or no effort

In short, it's an experiment to see how close we can get to the 0.3.0 developer experience with minimal changes to existing tools.

### Features

- Modified guest WIT bindings generators and support libraries for first-class, composable async/await in Rust and Python
- Support for concurrent `await`ing of any `wasi:io/poll.pollable` (e.g. files, sockets, timers, HTTP bodies)
  - No need for `wasi:io/poll.poll` anymore -- just use `await`!
- `spawn` function allows guests to spawn tasks which may outlive the current function call from the host or composed component
  - For example, you can spawn a task to stream an HTTP response body and return the response object to the caller before the stream has finished.
- Async-friendly composition using `wasm-compose`
- Asynchronous cancellation of host and guest tasks (currently untested)
- Modified Rust host-side binding generator and support library for hosting components, including a working implementation of `wasi:http@0.3.0-draft`
- A CLI tool supporting a `serve` subcommand for running `isyswasfa`-flavored `wasi:http@0.3.0-draft` components

### Planned features

- Guest support for other languages supporting stackless coroutines
  - E.g. JavaScript and .NET
  - Eventually, the Component Model will also support composable concurrency for stackful coroutines (e.g. Goroutines, Java fibers, etc.), but those are out of scope for this polyfill.
- Host-side code generation for bridging async and sync components using backpressure to serialize async->sync calls without blocking the caller

### Examples

The [test/rust-cases](./test/rust-cases) and [test/python-cases](./test/python-cases) directory contains a few guest programs and corresponding host code for executing them:

- **round-trip** ([Rust version](./test/rust-cases/round-trip/src/lib.rs), [Python version](./test/python-cases/round-trip/app.py)): a simple example of an exported async function calling an imported async function
- **service** ([Rust version](./test/rust-cases/service/src/lib.rs), [Python version](./test/python-cases/service/app.py)) and **middleware** ([Rust version](./test/rust-cases/middleware/src/lib.rs)): a pair of components which are composed to demonstrate cross-component asynchronous I/O, with the middleware providing transparent `deflate` encoding and decoding support to the service.  These use `wasi:http@0.3.0-draft`, which includes a single `request` type and a single `response` type; unlike `wasi:http@0.2.0`, there is no need for incoming and outgoing variations of those types.
- **hash-all** ([Rust version](./test/rust-cases/hash-all/src/lib.rs), [Python version](./test/python-cases/hash-all/app.py)): A `wasi:http@0.3.0-draft` component, capable of sending multiple concurrent outgoing requests, hashing the response bodies without buffering, and streaming the hashes back to the client.
- **echo** ([Rust version](./test/rust-cases/echo/src/lib.rs), [Python version](./test/python-cases/echo/app.py)): A `wasi:http@0.3.0-draft` component, capable of either echoing the request body back to the client without buffering, or else piping the request body to an outgoing request and then streaming the response body back to the client.

#### Building and running the examples

To build the CLI from source using this Git repository, [install Rust](https://rustup.rs/) and run:

```shell
cargo build --release --manifest-path cli/Cargo.toml
```

Then you can run the command using e.g.:

```shell
./target/release/isyswasfa --help
```

To build the Rust guest examples, you'll need to make sure you have the `wasm32-wasi` Rust target installed, along with a recent version of `wasm-tools`:

```shell
rustup target add wasm32-wasi
cargo install wasm-tools
```

We can build the Rust `hash-all` example using:

```shell
cargo build --release --target wasm32-wasi --manifest-path test/rust-cases/hash-all/Cargo.toml
curl -LO https://github.com/bytecodealliance/wasmtime/releases/download/v18.0.2/wasi_snapshot_preview1.reactor.wasm
wasm-tools component new --adapt wasi_snapshot_preview1.reactor.wasm test/rust-cases/target/wasm32-wasi/debug/hash_all.wasm -o hash-all.wasm
```

And finally we can run it:

```shell
./target/release/isyswasfa serve hash-all.wasm
```

While that's running, we can send it a request from another terminal:

```shell
curl -i \
    -H 'url: https://webassembly.github.io/spec/core/' \
    -H 'url: https://www.w3.org/groups/wg/wasm/' \
    -H 'url: https://bytecodealliance.org/' \
    http://127.0.0.1:8080/
```

To build the Python examples, you'll need to build this project's temporary fork of `componentize-py`, which requires a `wasi-sockets`-enabled build of `wasi-sdk` (replace `linux` with `macos` or `mingw` (Windows) as appropriate):

```shell
curl -LO https://github.com/dicej/wasi-sdk/releases/download/wasi-sockets-alpha-2/wasi-sdk-20.26g68203b20b82e-linux.tar.gz
tar xf tar xf wasi-sdk-20.26g68203b20b82e-linux.tar.gz
sudo mv wasi-sdk-20.26g68203b20b82e /opt/wasi-sdk
export WASI_SDK_PATH=/opt/wasi-sdk
```

Then run:

```shell
cargo build --release --manifest-path componentize-py/Cargo.toml
```

And finally build and run the Python `hash-all` example:

```shell
./componentize-py/target/release/componentize-py --isyswasfa=-echo -d wit -w proxy componentize -p test/python-cases/hash-all app -o hash-all.wasm
./target/release/isyswasfa serve hash-all.wasm
```

#### Composing Rust and Python components

In addition to the tools we built above, we can build `wasm-compose` and use it to compose the Python `hash-all` component with the Rust `middleware` component.  Here, we use a lightly-patched version which supports exposing exports from multiple subcomponents:

```shell
cargo build --release --manifest-path wasm-tools/Cargo.toml
```

Then we build the `middleware` component (reusing the `wasi_snapshot_preview1.reactor.wasm` file we downloaded above):

```shell
cargo build --release --target wasm32-wasi --manifest-path test/rust-cases/middleware/Cargo.toml
wasm-tools component new --adapt wasi_snapshot_preview1.reactor.wasm test/rust-cases/target/wasm32-wasi/debug/middleware.wasm -o middleware.wasm
```

And compose it with the `hash-all` component we built above, then run the result:

```shell
./wasm-tools/target/release/wasm-tools compose middleware.wasm -d hash-all.wasm -o composed.wasm
./target/release/isyswasfa serve composed.wasm
```

This time, we'll send a request with a `accept-encoding: deflate` header, which tells the `middleware` component to compress the response body:

```shell
curl -i --compressed \
    -H 'accept-encoding: deflate' \
    -H 'url: https://webassembly.github.io/spec/core/' \
    -H 'url: https://www.w3.org/groups/wg/wasm/' \
    -H 'url: https://bytecodealliance.org/' \
    http://127.0.0.1:8080/
```

Viola!

### How it works

I've lightly modified the `wit-bindgen` and `wasmtime-wit-bindgen` Rust code generators to support an `isyswasfa` configuration option.  When that option is enabled, the code generators "asyncify" a subset of imported and exported functions by splitting each one into two functions: one for initiating a task, and other for retrieving the result when the task has completed.  For example:

- `foo: func(s: string) -> string` becomes:
  - `foo-isyswasfa-start: func(s: string) -> result<string, pending>`, where `pending` is a resource handle representing an asynchronous task, returned if a result is not immediately available, and
  - `foo-isyswasfa-result: func(r: ready) -> string`, where `ready` is a resource handle representing the completion of an asynchronous task.
  
These two functions become part of the new component type seen by the host or composition tool, while the user-visible generated code presents only a single `async` function to the application developer.

In addition, the guest binding generator exports a function named `isyswasfa-poll$SUFFIX`, where `$SUFFIX` represents a unique string that must differ from any other component the current component might be composed with.  In the case of composition, each subcomponent will export its own such function.  The host will use these functions to send and receive events to and from the component, keeping track of which subcomponents are waiting for which tasks, where to route cancellation requests and confirmations, etc.

(TODO: add a step-by-step example with a diagram)

Although WASI 0.2 imports are *not* transformed as described above, the `isyswasfa-host` and `isyswasfa-guest` support libraries have special support for `wasi:io/poll.pollable` handles such that they can be concurrently `await`ed by the guest and multiplexed by the host, allowing e.g. `monotonic_clock::subscribe_duration(ns).await` to do just what you'd expect.
