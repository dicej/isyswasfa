## isyswasfa: I sync, you sync, we all sync for async

An experimental polyfill for composable concurrency based on the [WebAssembly Component Model](https://github.com/WebAssembly/component-model) and [WASI](https://github.com/WebAssembly/WASI) Preview 2

### Background

As of this writing, the Component Model does not support concurrent, composable execution.  Although WASI Preview 2 includes support for asynchronous I/O via the `wasi:io/poll` interface, it does not compose well: only one component in a composition can block at a time.  A major goal for WASI Preview 3 is to provide built-in support for "composable async" in the Component Model, thereby resolving the tension between composition and concurrency.

### So what is this?

A pile of hacks -- but a _useful_ pile of hacks.  The goals are:

- To provide early, real-world implementation feedback to the Component Model "async" design process
- To give developers a tool for "polyfilling" composable concurrency on top of WASI Preview 2, ideally in such a way that upgrading application code to Preview 3 requires little or no effort

In short, it's an experiment to see how close we can get to the Preview 3 developer experience with minimal changes to existing tools.

### Features

- Modified guest and host WIT bindings generators and support libraries for first-class, composable async/await in Rust
- Support for concurrent `await`ing of any `wasi:io/poll.pollable` (e.g. files, sockets, timers, HTTP bodies)
  - No need for `wasi:io/poll.poll` anymore -- just use `await`!
- `spawn` function allows guests to spawn tasks which may outlive the current function call from the host or composed component
  - For example, you can spawn a task to stream an HTTP response body and return the response object to the caller before the stream has finished.
- Async-friendly composition using `wasm-compose`
- Asynchronous cancellation of host and guest tasks (currently untested)

### Planned features

- Guest support for other languages supporting stackless coroutines
  - E.g. Python, JavaScript, and .NET
  - Eventually, the Component Model will also support composable concurrency for stackful coroutines (e.g. Goroutines, Java fibers, etc.), but those are out of scope for this polyfill.
- Host-side code generation for bridging async and sync components using backpressure to serialize async->sync calls without blocking the caller

### Examples

The [test](./test) directory contains a few guest programs and corresponding host code for executing them:

- [round-trip](./test/round-trip/src/lib.rs): a simple example of an exported async function calling an imported async function
- [wasi-http-handler](./test/wasi-http-handler/src/lib.rs): an example of asynchronously handling a `wasi:http/types.incoming-request`, spawning a task to stream the request body back to the client, and returning a `wasi:http/types.outgoing-request` without waiting for the stream task to complete.  Note that this does away with the `wasi:http/types.response-outparam` type, which is no longer needed with first-class async support.
- [service](./test/service/src/lib.rs) and [middleware](./test/middleware/src/lib.rs): a pair of components which are composed to demonstrate asynchronous I/O across composed components, with the middleware providing transparent `deflate` encoding and decoding support to the service.  These use a (highly simplified) version of what we expect `wasi-http` 0.3.0 will provide: a single `request` type and a single `response` type, with no need for incoming and outgoing variations.

### How it works

I've lightly modified the `wit-bindgen` and `wasmtime-wit-bindgen` Rust code generators to support an `isyswasfa` configuration option.  When that option is enabled, the code generators "asyncify" each function in non-WASI imports and exports by splitting it into two functions: one for initiating a task, and other for retrieving the result when the task has completed.  For example:

- `foo: func(s: string) -> string` becomes:
  - `foo-isyswasfa: func(s: string) -> result<string, pending>`, where `pending` is a resource handle representing an asynchronous task, returned if a result is not immediately available, and
  - `foo-isyswasfa-result: func(r: ready) -> string`, where `ready` is a resource handle representing the completion of an asynchronous task.
  
These two functions become part of the new component type seen by the host or composition tool, while the user-visible generated code presents only a single `async` function to the application developer.

In addition, the guest binding generator exports a function named `isyswasfa-poll$SUFFIX`, where `$SUFFIX` represents a unique string that must differ from any other component the current component might be composed with.  In the case of composition, each subcomponent will export its own such function.  The host will use these functions to send and receive events to and from the component, keeping track of which subcomponents are waiting for which tasks, where to route cancellation requests and confirmations, etc.

(TODO: add a step-by-step example with a diagram)

Although WASI imports are *not* transformed as described above, the `isyswasfa-host` and `isyswasfa-guest` support libraries have special support for `wasi:io/poll.pollable` handles such that they can be concurrently `await`ed by the guest and multiplexed by the host, allowing e.g. `monotonic_clock::subscribe_duration(ns).await` to do just what you'd expect.