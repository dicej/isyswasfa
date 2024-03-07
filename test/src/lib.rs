#![deny(warnings)]

#[cfg(test)]
mod test {
    use {
        anyhow::{anyhow, bail, Result},
        async_trait::async_trait,
        bytes::Bytes,
        futures::{
            channel::{mpsc, oneshot},
            future::{BoxFuture, FutureExt},
            sink::SinkExt,
        },
        indexmap::IndexMap,
        isyswasfa_host::{IsyswasfaCtx, IsyswasfaView, ReceiverStream},
        isyswasfa_http::{
            wasi::http::types::{ErrorCode, Method, Scheme},
            Body, Fields, FieldsReceiver, Request, Response, WasiHttpState, WasiHttpView,
        },
        sha2::{Digest, Sha256},
        std::{
            collections::HashMap,
            io::Write,
            str,
            sync::{Arc, Mutex, MutexGuard},
            time::Duration,
        },
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

    async fn build_component(name: &str) -> Result<Vec<u8>> {
        static BUILD: OnceCell<()> = OnceCell::const_new();

        BUILD
            .get_or_init(|| async {
                assert!(
                    Command::new("cargo")
                        .current_dir("cases")
                        .args(["build", "--workspace", "--target", "wasm32-wasi"])
                        .status()
                        .await
                        .unwrap()
                        .success(),
                    "cargo build failed"
                );
            })
            .await;

        const ADAPTER_PATH: &str = "cases/target/wasi_snapshot_preview1.reactor.wasm";

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
            .module(&fs::read(format!("cases/target/wasm32-wasi/debug/{name}.wasm")).await?)?
            .adapter("wasi_snapshot_preview1", &fs::read(ADAPTER_PATH).await?)?
            .encode()
    }

    type RequestSender = Arc<
        dyn Fn(
                &HttpState,
                Resource<Request>,
            )
                -> BoxFuture<'static, wasmtime::Result<Result<Resource<Response>, ErrorCode>>>
            + Send
            + Sync,
    >;

    #[derive(Clone)]
    struct HttpState {
        shared_table: Arc<Mutex<ResourceTable>>,
        send_request: Option<RequestSender>,
    }

    #[async_trait]
    impl WasiHttpState for HttpState {
        fn shared_table(&self) -> MutexGuard<ResourceTable> {
            self.shared_table.lock().unwrap()
        }

        async fn handle_request(
            &self,
            request: Resource<Request>,
        ) -> wasmtime::Result<Result<Resource<Response>, ErrorCode>> {
            if let Some(send_request) = self.send_request.clone() {
                send_request(self, request).await
            } else {
                bail!("no outbound request handler available")
            }
        }
    }

    struct Ctx {
        wasi: WasiCtx,
        isyswasfa: IsyswasfaCtx,
        http_state: HttpState,
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
        fn shared_table(&self) -> MutexGuard<ResourceTable> {
            self.http_state.shared_table.lock().unwrap()
        }
    }

    impl IsyswasfaView for Ctx {
        type State = HttpState;

        fn isyswasfa(&mut self) -> &mut IsyswasfaCtx {
            &mut self.isyswasfa
        }
        fn state(&self) -> Self::State {
            self.http_state.clone()
        }
    }

    #[async_trait]
    impl round_trip::component::test::baz::Host for Ctx {
        async fn foo(_state: HttpState, s: String) -> wasmtime::Result<String> {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(format!("{s} - entered host - exited host"))
        }
    }

    #[tokio::test]
    async fn round_trip() -> Result<()> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component_bytes = build_component("round_trip").await?;

        let component = Component::new(&engine, &component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;

        round_trip::RoundTrip::add_to_linker(&mut linker, |ctx| ctx)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                http_state: HttpState {
                    shared_table: Arc::new(Mutex::new(ResourceTable::new())),
                    send_request: None,
                },
            },
        );

        let (round_trip, instance) =
            round_trip::RoundTrip::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, &component_bytes, &instance)?;

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
    async fn service() -> Result<()> {
        service_test(&build_component("service").await?, false).await
    }

    #[tokio::test]
    async fn middleware() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let service = build_component("service").await?;
        let service_file = dir.path().join("service.wasm");
        fs::write(&service_file, &service).await?;

        let middleware = build_component("middleware").await?;
        let middleware_file = dir.path().join("middleware.wasm");
        fs::write(&middleware_file, &middleware).await?;

        use wasm_compose::{
            composer::ComponentComposer,
            config::{Config, Instantiation, InstantiationArg},
        };

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
                            "wasi:http/handler@0.3.0-draft-2024-02-14".to_owned(),
                            InstantiationArg {
                                instance: "service".into(),
                                export: Some("wasi:http/handler@0.3.0-draft-2024-02-14".into()),
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

        service_test(composed, true).await
    }

    async fn service_test(component_bytes: &[u8], use_compression: bool) -> Result<()> {
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
                http_state: HttpState {
                    shared_table: Arc::new(Mutex::new(ResourceTable::new())),
                    send_request: None,
                },
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

        let request = store.data_mut().shared_table().push(Request {
            method: Method::Post,
            scheme: Some(Scheme::Http),
            path_with_query: Some("/foo".into()),
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

        let mut response = store.data_mut().shared_table().delete(response)?;

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
    async fn hash_all() -> Result<()> {
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

            move |state: &HttpState, request: Resource<Request>| {
                let table = state.shared_table.clone();
                let bodies = bodies.clone();

                async move {
                    let request = table.lock().unwrap().delete(request)?;

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

                    Ok(Ok(table.lock().unwrap().push(Response {
                        status_code,
                        headers: Fields(Vec::new()),
                        body: Body {
                            stream: Some(InputStream::Host(Box::new(ReceiverStream::new(rx)))),
                            trailers: None,
                        },
                    })?))
                }
                .boxed()
            }
        });

        let component_bytes = build_component("hash_all").await?;

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, &component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;
        isyswasfa_http::add_to_linker(&mut linker)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                http_state: HttpState {
                    shared_table: Arc::new(Mutex::new(ResourceTable::new())),
                    send_request: Some(send_request),
                },
            },
        );

        let (proxy, instance) =
            proxy::Proxy::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, &component_bytes, &instance)?;

        let request = store.data_mut().shared_table().push(Request {
            method: Method::Get,
            scheme: Some(Scheme::Http),
            path_with_query: Some("/".into()),
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

        let mut response = store.data_mut().shared_table().delete(response)?;

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

            use base64::Engine;
            assert_eq!(
                hash,
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(hasher.finalize())
            );
        }

        Ok(())
    }
}
