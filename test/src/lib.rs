#![deny(warnings)]

#[cfg(test)]
mod test {
    use {
        anyhow::{anyhow, bail, Error, Result},
        bytes::Bytes,
        futures::{
            channel::{mpsc, oneshot},
            future::{BoxFuture, FutureExt},
            sink::SinkExt,
            stream::{FuturesUnordered, TryStreamExt},
        },
        indexmap::IndexMap,
        isyswasfa_host::{IsyswasfaCtx, IsyswasfaView, ReceiverStream},
        isyswasfa_http::{
            wasi::http::types::{ErrorCode, Method, Scheme},
            Body, Fields, FieldsReceiver, Request, Response, WasiHttpView,
        },
        sha2::{Digest, Sha256},
        std::{
            collections::HashMap,
            future::Future,
            io::Write,
            iter,
            path::Path,
            str,
            sync::{Arc, Once},
            time::Duration,
        },
        tempfile::NamedTempFile,
        tokio::{fs, process::Command, sync::OnceCell},
        wasmtime::{
            component::{Component, Linker, Resource, ResourceTable},
            Config, Engine, Store,
        },
        wasmtime_wasi::preview2::{
            command, InputStream, StreamError, WasiCtx, WasiCtxBuilder, WasiView,
        },
        wit_component::ComponentEncoder,
    };

    mod round_trip {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "round-trip",
            isyswasfa: true,
            with: {
                "wasi:clocks/monotonic-clock": wasmtime_wasi::preview2::bindings::wasi::clocks::monotonic_clock,
            }
        });
    }

    mod proxy {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "proxy",
            isyswasfa: true,
            with: {
                "wasi:clocks/monotonic-clock": wasmtime_wasi::preview2::bindings::wasi::clocks::monotonic_clock,
                "wasi:io/error": wasmtime_wasi::preview2::bindings::wasi::io::error,
                "wasi:io/streams": wasmtime_wasi::preview2::bindings::wasi::io::streams,
                "isyswasfa:io/pipe": isyswasfa_host::isyswasfa_pipe_interface,
                "wasi:http/types": isyswasfa_http::wasi::http::types,
            }
        });
    }

    fn init_logger() {
        static ONCE: Once = Once::new();
        ONCE.call_once(pretty_env_logger::init);
    }

    async fn build_rust_component(name: &str) -> Result<Vec<u8>> {
        init_logger();

        static BUILD: OnceCell<()> = OnceCell::const_new();

        BUILD
            .get_or_init(|| async {
                assert!(
                    Command::new("cargo")
                        .current_dir("rust-cases")
                        .args([
                            "build",
                            "--release",
                            "--workspace",
                            "--target",
                            "wasm32-wasi"
                        ])
                        .status()
                        .await
                        .unwrap()
                        .success(),
                    "cargo build failed"
                );
            })
            .await;

        const ADAPTER_PATH: &str = "rust-cases/target/wasi_snapshot_preview1.reactor.wasm";

        static ADAPTER: OnceCell<()> = OnceCell::const_new();

        ADAPTER
            .get_or_init(|| async {
                let adapter_url = "https://github.com/bytecodealliance/wasmtime/releases\
                                   /download/v18.0.0/wasi_snapshot_preview1.reactor.wasm";

                if !fs::try_exists(ADAPTER_PATH).await.unwrap() {
                    fs::write(
                        ADAPTER_PATH,
                        reqwest::get(adapter_url)
                            .await
                            .unwrap()
                            .bytes()
                            .await
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                }
            })
            .await;

        ComponentEncoder::default()
            .validate(true)
            .module(&fs::read(format!("rust-cases/target/wasm32-wasi/release/{name}.wasm")).await?)?
            .adapter("wasi_snapshot_preview1", &fs::read(ADAPTER_PATH).await?)?
            .encode()
    }

    async fn build_python_component(
        world: &str,
        name: &str,
        isyswasfa_suffix: &str,
    ) -> Result<Vec<u8>> {
        init_logger();

        let tmp = NamedTempFile::new()?;
        componentize_py::componentize(
            Some(Path::new("../wit")),
            Some(world),
            &[&format!("python-cases/{name}")],
            &[],
            "app",
            tmp.path(),
            None,
            Some(isyswasfa_suffix),
            false,
        )
        .await?;
        Ok(fs::read(tmp.path()).await?)
    }

    type RequestSender =
        Arc<dyn Fn(Request) -> BoxFuture<'static, Result<Response, ErrorCode>> + Send + Sync>;

    struct Ctx {
        wasi: WasiCtx,
        isyswasfa: IsyswasfaCtx<Ctx>,
        send_request: Option<RequestSender>,
    }

    impl WasiView for Ctx {
        fn table(&mut self) -> &mut ResourceTable {
            self.isyswasfa.table()
        }
        fn ctx(&mut self) -> &mut WasiCtx {
            &mut self.wasi
        }
    }

    impl WasiHttpView for Ctx {
        fn table(&mut self) -> &mut ResourceTable {
            self.isyswasfa.table()
        }
        fn send_request(
            &mut self,
            request: Resource<Request>,
        ) -> wasmtime::Result<
            impl Future<
                    Output = impl FnOnce(
                        &mut Self,
                    )
                        -> wasmtime::Result<Result<Resource<Response>, ErrorCode>>
                                 + 'static,
                > + Send
                + 'static,
        > {
            if let Some(send_request) = self.send_request.clone() {
                let request = WasiHttpView::table(self).delete(request)?;
                Ok(async move {
                    let response = send_request(request).await;
                    move |self_: &mut Self| {
                        Ok(match response {
                            Ok(response) => Ok(WasiHttpView::table(self_).push(response)?),
                            Err(e) => Err(e),
                        })
                    }
                })
            } else {
                bail!("no outbound request handler available")
            }
        }
    }

    impl IsyswasfaView for Ctx {
        fn isyswasfa(&mut self) -> &mut IsyswasfaCtx<Ctx> {
            &mut self.isyswasfa
        }
    }

    impl round_trip::component::test::baz::Host for Ctx {
        fn foo(
            &mut self,
            s: String,
        ) -> wasmtime::Result<
            impl Future<Output = impl FnOnce(&mut Self) -> wasmtime::Result<String> + 'static>
                + Send
                + 'static,
        > {
            Ok(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                move |_: &mut Self| Ok(format!("{s} - entered host - exited host"))
            })
        }
    }

    #[tokio::test]
    async fn round_trip_rust() -> Result<()> {
        round_trip(&build_rust_component("round_trip").await?).await
    }

    #[tokio::test]
    async fn round_trip_python() -> Result<()> {
        round_trip(&build_python_component("round-trip", "round-trip", "-round-trip").await?).await
    }

    async fn round_trip(component_bytes: &[u8]) -> Result<()> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;

        round_trip::RoundTrip::add_to_linker(&mut linker, |ctx| ctx)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                send_request: None,
            },
        );

        let (round_trip, instance) =
            round_trip::RoundTrip::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, component_bytes, &instance)?;

        let value = round_trip
            .component_test_baz()
            .call_foo(&mut store, "hello, world!")
            .await?;

        assert_eq!(
            "hello, world! - entered guest - entered host - exited host - exited guest",
            &value
        );

        Ok(())
    }

    #[tokio::test]
    async fn service_rust() -> Result<()> {
        service_test(&build_rust_component("service").await?, "/service", false).await
    }

    #[tokio::test]
    async fn service_python() -> Result<()> {
        service_test(
            &build_python_component("proxy", "service", "-service").await?,
            "/service",
            false,
        )
        .await
    }

    #[tokio::test]
    async fn middleware_rust() -> Result<()> {
        middleware(&build_rust_component("service").await?).await
    }

    #[tokio::test]
    async fn middleware_python() -> Result<()> {
        middleware(&build_python_component("proxy", "service", "-service").await?).await
    }

    async fn middleware(service: &[u8]) -> Result<()> {
        use wasm_compose::{
            composer::ComponentComposer,
            config::{Config, Instantiation, InstantiationArg},
        };

        let dir = tempfile::tempdir()?;

        let service_file = dir.path().join("service.wasm");
        fs::write(&service_file, &service).await?;

        let middleware = build_rust_component("middleware").await?;
        let middleware_file = dir.path().join("middleware.wasm");
        fs::write(&middleware_file, &middleware).await?;

        let composed = &ComponentComposer::new(
            &middleware_file,
            &Config {
                dir: dir.path().to_owned(),
                definitions: Vec::new(),
                search_paths: Vec::new(),
                skip_validation: false,
                import_components: false,
                disallow_imports: false,
                dependencies: IndexMap::new(),
                instantiations: [(
                    "root".to_owned(),
                    Instantiation {
                        dependency: None,
                        arguments: [(
                            "wasi:http/handler@0.3.0-draft".to_owned(),
                            InstantiationArg {
                                instance: "service".into(),
                                export: Some("wasi:http/handler@0.3.0-draft".into()),
                            },
                        )]
                        .into_iter()
                        .collect(),
                    },
                )]
                .into_iter()
                .collect(),
            },
        )
        .compose()?;

        service_test(composed, "/middleware", true).await
    }

    async fn service_test(component_bytes: &[u8], uri: &str, use_compression: bool) -> Result<()> {
        use flate2::{
            write::{DeflateDecoder, DeflateEncoder},
            Compression,
        };

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;
        isyswasfa_http::add_to_linker(&mut linker)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                send_request: None,
            },
        );

        let (proxy, instance) =
            proxy::Proxy::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, component_bytes, &instance)?;

        let headers = vec![("foo".into(), b"bar".into())];

        let body = b"And the mome raths outgrabe";

        let request_body_rx = {
            let (mut request_body_tx, request_body_rx) = mpsc::channel(1);

            request_body_tx
                .send(if use_compression {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
                    encoder.write_all(body)?;
                    encoder.finish()?.into()
                } else {
                    Bytes::from_static(body)
                })
                .await?;

            request_body_rx
        };

        let (request_trailers_tx, request_trailers_rx) = oneshot::channel();

        let trailers = vec![("fizz".into(), b"buzz".into())];

        _ = request_trailers_tx.send(Fields(trailers.clone()));

        let request = WasiHttpView::table(store.data_mut()).push(Request {
            method: Method::Post,
            scheme: Some(Scheme::Http),
            path_with_query: Some(uri.into()),
            authority: Some("localhost".into()),
            headers: Fields(
                headers
                    .iter()
                    .cloned()
                    .chain(if use_compression {
                        vec![
                            ("content-encoding".into(), b"deflate".into()),
                            ("accept-encoding".into(), b"deflate".into()),
                        ]
                    } else {
                        Vec::new()
                    })
                    .collect(),
            ),
            body: Body {
                stream: Some(InputStream::Host(Box::new(ReceiverStream::new(
                    request_body_rx,
                )))),
                trailers: Some(FieldsReceiver(request_trailers_rx)),
            },
            options: None,
        })?;

        let response = proxy
            .wasi_http_handler()
            .call_handle(&mut store, request)
            .await??;

        let mut response = WasiHttpView::table(store.data_mut()).delete(response)?;

        assert!(response.status_code == 200);

        assert!(headers.iter().all(|(k0, v0)| response
            .headers
            .0
            .iter()
            .any(|(k1, v1)| k0 == k1 && v0 == v1)));

        let InputStream::Host(mut response_rx) = response.body.stream.take().unwrap() else {
            unreachable!();
        };

        let response_body = isyswasfa_host::poll_loop_until(&mut store, async move {
            let mut buffer = Vec::new();

            loop {
                match response_rx.read(1024) {
                    Ok(bytes) if bytes.is_empty() => response_rx.ready().await,
                    Ok(bytes) => buffer.extend_from_slice(&bytes),
                    Err(StreamError::Closed) => break Ok::<_, anyhow::Error>(buffer),
                    Err(e) => break Err(anyhow!("error reading response body: {e:?}")),
                }
            }
        })
        .await??;

        let response_body = if use_compression {
            assert!(response.headers.0.iter().any(|(k, v)| matches!(
                (k.as_str(), v.as_slice()),
                ("content-encoding", b"deflate")
            )));

            let mut decoder = DeflateDecoder::new(Vec::new());
            decoder.write_all(&response_body)?;
            decoder.finish()?
        } else {
            response_body
        };

        assert_eq!(body as &[_], &response_body);

        let response_trailers =
            isyswasfa_host::poll_loop_until(&mut store, response.body.trailers.take().unwrap().0)
                .await??;

        assert!(trailers.iter().all(|(k0, v0)| response_trailers
            .0
            .iter()
            .any(|(k1, v1)| k0 == k1 && v0 == v1)));

        Ok(())
    }

    #[tokio::test]
    async fn hash_all_rust() -> Result<()> {
        hash_all(&build_rust_component("hash_all").await?).await
    }

    #[tokio::test]
    async fn hash_all_python() -> Result<()> {
        hash_all(&build_python_component("proxy", "hash-all", "-hash-all").await?).await
    }

    async fn hash_all(component_bytes: &[u8]) -> Result<()> {
        let bodies = Arc::new(
            [
                ("/a", "’Twas brillig, and the slithy toves"),
                ("/b", "Did gyre and gimble in the wabe:"),
                ("/c", "All mimsy were the borogoves,"),
                ("/d", "And the mome raths outgrabe."),
            ]
            .into_iter()
            .collect::<HashMap<_, _>>(),
        );

        let send_request = Arc::new({
            let bodies = bodies.clone();

            move |request: Request| {
                let bodies = bodies.clone();

                async move {
                    let (status_code, rx) = if let (Method::Get, Some(body)) = (
                        request.method,
                        request.path_with_query.and_then(|p| bodies.get(p.as_str())),
                    ) {
                        let (mut tx, rx) = mpsc::channel(1);
                        tx.send(Bytes::copy_from_slice(body.as_bytes()))
                            .await
                            .unwrap();
                        (200, rx)
                    } else {
                        (405, mpsc::channel(1).1)
                    };

                    Ok(Response {
                        status_code,
                        headers: Fields(Vec::new()),
                        body: Body {
                            stream: Some(InputStream::Host(Box::new(ReceiverStream::new(rx)))),
                            trailers: None,
                        },
                    })
                }
                .boxed()
            }
        });

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;
        isyswasfa_http::add_to_linker(&mut linker)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                send_request: Some(send_request),
            },
        );

        let (proxy, instance) =
            proxy::Proxy::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, component_bytes, &instance)?;

        let request = WasiHttpView::table(store.data_mut()).push(Request {
            method: Method::Get,
            scheme: Some(Scheme::Http),
            path_with_query: Some("/hash-all".into()),
            authority: Some("localhost".into()),
            headers: Fields(
                bodies
                    .keys()
                    .map(|key| ("url".into(), format!("http://localhost{key}").into_bytes()))
                    .collect(),
            ),
            body: Body {
                stream: Some(InputStream::Host(Box::new(ReceiverStream::new(
                    mpsc::channel(1).1,
                )))),
                trailers: None,
            },
            options: None,
        })?;

        let response = proxy
            .wasi_http_handler()
            .call_handle(&mut store, request)
            .await??;

        let mut response = WasiHttpView::table(store.data_mut()).delete(response)?;

        assert!(response.status_code == 200);

        let InputStream::Host(mut response_rx) = response.body.stream.take().unwrap() else {
            unreachable!();
        };

        let response_body = isyswasfa_host::poll_loop_until(&mut store, async move {
            let mut buffer = Vec::new();

            loop {
                match response_rx.read(1024) {
                    Ok(bytes) if bytes.is_empty() => response_rx.ready().await,
                    Ok(bytes) => buffer.extend_from_slice(&bytes),
                    Err(StreamError::Closed) => break Ok::<_, anyhow::Error>(buffer),
                    Err(e) => break Err(anyhow!("error reading response body: {e:?}")),
                }
            }
        })
        .await??;

        let body = str::from_utf8(&response_body)?;
        for line in body.lines() {
            let (url, hash) = line
                .split_once(": ")
                .ok_or_else(|| anyhow!("expected string of form `<url>: <sha-256>`; got {line}"))?;

            let prefix = "http://localhost";
            let path = url
                .strip_prefix(prefix)
                .ok_or_else(|| anyhow!("expected string with prefix {prefix}; got {url}"))?;

            let mut hasher = Sha256::new();
            hasher.update(
                bodies
                    .get(path)
                    .ok_or_else(|| anyhow!("unexpected path: {path}"))?,
            );

            assert_eq!(hash, hex::encode(hasher.finalize()));
        }

        Ok(())
    }

    #[tokio::test]
    async fn echo_rust() -> Result<()> {
        echo_test(&build_rust_component("echo").await?, "/echo", None).await
    }

    #[tokio::test]
    async fn echo_python() -> Result<()> {
        echo_test(
            &build_python_component("proxy", "echo", "-echo").await?,
            "/echo",
            None,
        )
        .await
    }

    #[tokio::test]
    async fn double_echo_rust() -> Result<()> {
        double_echo(&build_rust_component("echo").await?, "/double-echo").await
    }

    #[tokio::test]
    async fn double_echo_python() -> Result<()> {
        double_echo(
            &build_python_component("proxy", "echo", "-echo").await?,
            "/double-echo",
        )
        .await
    }

    async fn double_echo(component_bytes: &[u8], uri: &str) -> Result<()> {
        let send_request = Arc::new({
            move |request: Request| {
                async move {
                    let (status_code, body) = if let (Method::Post, Some("/echo")) =
                        (request.method, request.path_with_query.as_deref())
                    {
                        (200, request.body)
                    } else {
                        (
                            405,
                            Body {
                                stream: Some(InputStream::Host(Box::new(ReceiverStream::new(
                                    mpsc::channel(1).1,
                                )))),
                                trailers: None,
                            },
                        )
                    };

                    Ok(Response {
                        status_code,
                        headers: Fields(
                            request
                                .headers
                                .0
                                .into_iter()
                                .filter(|(k, _)| k == "content-type")
                                .collect(),
                        ),
                        body,
                    })
                }
                .boxed()
            }
        });

        echo_test(component_bytes, uri, Some(("/echo", send_request))).await
    }

    async fn echo_test(
        component_bytes: &[u8],
        uri: &str,
        double: Option<(&str, RequestSender)>,
    ) -> Result<()> {
        let body = &{
            // A sorta-random-ish megabyte
            let mut n = 0_u8;
            iter::repeat_with(move || {
                n = n.wrapping_add(251);
                n
            })
            .take(1024 * 1024)
            .collect::<Vec<_>>()
        };

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;
        isyswasfa_http::add_to_linker(&mut linker)?;

        let url_header = double.as_ref().map(|(s, _)| *s);
        let send_request = double.map(|(_, v)| v);

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                send_request,
            },
        );

        let (proxy, instance) =
            proxy::Proxy::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, component_bytes, &instance)?;

        let (mut request_body_tx, request_body_rx) = mpsc::channel(1);

        enum Event {
            RequestBody,
            Call {
                response: Resource<Response>,
                store: Store<Ctx>,
            },
            ResponseBody(Vec<u8>),
        }

        let pipe_request_body = async move {
            let chunks = body.chunks(16 * 1024).map(Bytes::copy_from_slice);

            for chunk in chunks {
                request_body_tx.send(chunk).await?;
            }

            Ok::<_, Error>(Event::RequestBody)
        };

        let request = WasiHttpView::table(store.data_mut()).push(Request {
            method: Method::Post,
            scheme: Some(Scheme::Http),
            path_with_query: Some(uri.into()),
            authority: Some("localhost".into()),
            headers: Fields(
                url_header
                    .into_iter()
                    .map(|key| ("url".into(), format!("http://localhost{key}").into_bytes()))
                    .chain(iter::once((
                        "content-type".into(),
                        b"application/octet-stream".into(),
                    )))
                    .collect(),
            ),
            body: Body {
                stream: Some(InputStream::Host(Box::new(ReceiverStream::new(
                    request_body_rx,
                )))),
                trailers: None,
            },
            options: None,
        })?;

        let call_handle = async move {
            let response = proxy
                .wasi_http_handler()
                .call_handle(&mut store, request)
                .await??;

            Ok::<_, Error>(Event::Call { response, store })
        };

        let mut futures = FuturesUnordered::new();
        futures.push(pipe_request_body.boxed());
        futures.push(call_handle.boxed());

        while let Some(event) = futures.try_next().await? {
            match event {
                Event::RequestBody => {}
                Event::Call {
                    response,
                    mut store,
                } => {
                    let mut response = WasiHttpView::table(store.data_mut()).delete(response)?;

                    assert!(response.status_code == 200);

                    assert!(response.headers.0.iter().any(|(k, v)| matches!(
                        (k.as_str(), v.as_slice()),
                        ("content-type", b"application/octet-stream")
                    )));

                    let InputStream::Host(mut response_rx) = response.body.stream.take().unwrap()
                    else {
                        unreachable!();
                    };

                    futures.push(
                        async move {
                            isyswasfa_host::poll_loop_until(&mut store, async move {
                                let mut buffer = Vec::new();

                                loop {
                                    match response_rx.read(1024) {
                                        Ok(bytes) if bytes.is_empty() => response_rx.ready().await,
                                        Ok(bytes) => buffer.extend_from_slice(&bytes),
                                        Err(StreamError::Closed) => {
                                            break Ok::<_, Error>(Event::ResponseBody(buffer))
                                        }
                                        Err(e) => {
                                            break Err(anyhow!(
                                                "error reading response body: {e:?}"
                                            ))
                                        }
                                    }
                                }
                            })
                            .await?
                        }
                        .boxed(),
                    );
                }
                Event::ResponseBody(response_body) => {
                    if body != &response_body {
                        panic!(
                            "body content mismatch (expected length {}; actual length {})",
                            body.len(),
                            response_body.len()
                        );
                    }

                    break;
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn router() -> Result<()> {
        use wasm_compose::{
            composer::ComponentComposer,
            config::{Config, Instantiation, InstantiationArg},
        };

        let composed = &{
            let dir = tempfile::tempdir()?;

            let echo = build_rust_component("echo").await?;
            let echo_file = dir.path().join("echo.wasm");
            fs::write(&echo_file, &echo).await?;

            let hash_all = build_rust_component("hash_all").await?;
            let hash_all_file = dir.path().join("hash-all.wasm");
            fs::write(&hash_all_file, &hash_all).await?;

            let service = build_rust_component("service").await?;
            let service_file = dir.path().join("service.wasm");
            fs::write(&service_file, &service).await?;

            let middleware = build_rust_component("middleware").await?;
            let middleware_file = dir.path().join("middleware.wasm");
            fs::write(&middleware_file, &middleware).await?;

            let router = build_rust_component("router").await?;
            let router_file = dir.path().join("router.wasm");
            fs::write(&router_file, &router).await?;

            ComponentComposer::new(
                &router_file,
                &Config {
                    dir: dir.path().to_owned(),
                    definitions: Vec::new(),
                    search_paths: Vec::new(),
                    skip_validation: false,
                    import_components: false,
                    disallow_imports: false,
                    dependencies: IndexMap::new(),
                    instantiations: [
                        (
                            "root".to_owned(),
                            Instantiation {
                                dependency: None,
                                arguments: [
                                    (
                                        "echo".to_owned(),
                                        InstantiationArg {
                                            instance: "echo".into(),
                                            export: Some("wasi:http/handler@0.3.0-draft".into()),
                                        },
                                    ),
                                    (
                                        "hash-all".to_owned(),
                                        InstantiationArg {
                                            instance: "hash-all".into(),
                                            export: Some("wasi:http/handler@0.3.0-draft".into()),
                                        },
                                    ),
                                    (
                                        "service".to_owned(),
                                        InstantiationArg {
                                            instance: "service".into(),
                                            export: Some("wasi:http/handler@0.3.0-draft".into()),
                                        },
                                    ),
                                    (
                                        "middleware".to_owned(),
                                        InstantiationArg {
                                            instance: "middleware".into(),
                                            export: Some("wasi:http/handler@0.3.0-draft".into()),
                                        },
                                    ),
                                ]
                                .into_iter()
                                .collect(),
                            },
                        ),
                        (
                            "middleware".to_owned(),
                            Instantiation {
                                dependency: None,
                                arguments: [(
                                    "wasi:http/handler@0.3.0-draft".to_owned(),
                                    InstantiationArg {
                                        instance: "service".into(),
                                        export: Some("wasi:http/handler@0.3.0-draft".into()),
                                    },
                                )]
                                .into_iter()
                                .collect(),
                            },
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )
            .compose()?
        };

        fs::write("/tmp/composed.wasm", composed).await?;

        echo_test(composed, "/echo", None).await?;
        double_echo(composed, "/double-echo").await?;
        double_echo(composed, "/proxy").await?;
        hash_all(composed).await?;
        service_test(composed, "/service", false).await?;
        middleware(composed).await
    }
}
