#[cfg(test)]
mod test {
    use {
        anyhow::{anyhow, Result},
        async_trait::async_trait,
        bytes::Bytes,
        futures::{
            channel::{mpsc, oneshot},
            future::{self, Either},
            sink::SinkExt,
        },
        indexmap::IndexMap,
        isyswasfa_host::{IsyswasfaCtx, IsyswasfaView},
        isyswasfa_http::{
            wasi::http::types::{Method, Scheme},
            Fields, FieldsReceiver, Request, WasiHttpState, WasiHttpView,
        },
        std::{
            env,
            io::Write,
            path::Path,
            pin::pin,
            sync::{Arc, Mutex, MutexGuard},
            time::Duration,
        },
        tokio::{fs, process::Command},
        wasmtime::{
            component::{Component, Linker, ResourceTable},
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

    mod service {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "service",
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

    async fn build_component(src_path: &str, name: &str) -> Result<Vec<u8>> {
        _ = fs::create_dir(Path::new(src_path).join("target")).await;

        let adapter_path = if let Ok(path) = env::var("ISYSWASFA_TEST_ADAPTER") {
            path
        } else {
            let adapter_url = "https://github.com/bytecodealliance/wasmtime/releases\
                               /download/v17.0.0/wasi_snapshot_preview1.reactor.wasm";

            let adapter_path = &format!("{src_path}/target/wasi_snapshot_preview1.reactor.wasm");

            if !fs::try_exists(adapter_path).await? {
                fs::write(
                    adapter_path,
                    reqwest::get(adapter_url).await?.bytes().await?,
                )
                .await?;
            }
            adapter_path.to_owned()
        };

        if Command::new("cargo")
            .current_dir(src_path)
            .args(["build", "--target", "wasm32-wasi"])
            .status()
            .await?
            .success()
        {
            Ok(ComponentEncoder::default()
                .validate(true)
                .module(
                    &fs::read(format!("{src_path}/target/wasm32-wasi/debug/{name}.wasm")).await?,
                )?
                .adapter("wasi_snapshot_preview1", &fs::read(adapter_path).await?)?
                .encode()?)
        } else {
            Err(anyhow!("cargo build failed"))
        }
    }

    #[derive(Clone)]
    struct SharedTable(Arc<Mutex<ResourceTable>>);

    impl WasiHttpState for SharedTable {
        fn shared_table(&self) -> MutexGuard<ResourceTable> {
            self.0.lock().unwrap()
        }
    }

    struct Ctx {
        wasi: WasiCtx,
        isyswasfa: IsyswasfaCtx,
        shared_table: SharedTable,
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
            self.shared_table.0.lock().unwrap()
        }
    }

    impl IsyswasfaView for Ctx {
        type State = SharedTable;

        fn isyswasfa(&mut self) -> &mut IsyswasfaCtx {
            &mut self.isyswasfa
        }
        fn state(&self) -> Self::State {
            self.shared_table.clone()
        }
    }

    #[async_trait]
    impl round_trip::component::test::baz::Host for Ctx {
        async fn foo(_state: SharedTable, s: String) -> wasmtime::Result<String> {
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

        let component_bytes = build_component("round-trip", "round_trip").await?;

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
                shared_table: SharedTable(Arc::new(Mutex::new(ResourceTable::new()))),
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
        service_test(&build_component("service", "service").await?, false).await
    }

    #[tokio::test]
    async fn middleware() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let service = build_component("service", "service").await?;
        let service_file = dir.path().join("service.wasm");
        fs::write(&service_file, &service).await?;

        let middleware = build_component("middleware", "middleware").await?;
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
        use {
            flate2::{
                write::{DeflateDecoder, DeflateEncoder},
                Compression,
            },
            isyswasfa_host::ReceiverStream,
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
                shared_table: SharedTable(Arc::new(Mutex::new(ResourceTable::new()))),
            },
        );

        let (service, instance) =
            service::Service::instantiate_async(&mut store, &component, &linker).await?;

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
                    .clone()
                    .into_iter()
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
            body: Some(InputStream::Host(Box::new(ReceiverStream::new(
                request_body_rx,
            )))),
            trailers: Some(FieldsReceiver(request_trailers_rx)),
            options: None,
        })?;

        let response = service
            .wasi_http_handler()
            .call_handle(&mut store, request)
            .await??;

        let mut response = store.data_mut().shared_table().delete(response)?;

        assert!(response.status_code == 200);

        let InputStream::Host(mut response_rx) = response.body.take().unwrap() else {
            unreachable!();
        };

        let response_body = async move {
            let mut buffer = Vec::new();

            loop {
                match response_rx.read(1024) {
                    Ok(bytes) if bytes.is_empty() => response_rx.ready().await,
                    Ok(bytes) => buffer.extend_from_slice(&bytes),
                    Err(StreamError::Closed) => break Ok::<_, anyhow::Error>(buffer),
                    Err(e) => break Err(anyhow!("error reading response body: {e:?}")),
                }
            }
        };
        let response_body = pin!(response_body);

        // TODO: move the following poll_loop/select logic into a reusable function:

        let response_body = {
            let poll_loop = isyswasfa_host::poll_loop(&mut store);
            let poll_loop = pin!(poll_loop);

            match future::select(poll_loop, response_body).await {
                Either::Left((Ok(()), body)) => body.await,
                Either::Left((Err(e), _)) => return Err(e),
                Either::Right((body, _)) => body,
            }?
        };

        let response_trailers = response.trailers.take().unwrap();

        let response_trailers = {
            let poll_loop = isyswasfa_host::poll_loop(&mut store);
            let poll_loop = pin!(poll_loop);

            match future::select(poll_loop, response_trailers.0).await {
                Either::Left((Ok(()), trailers)) => trailers.await,
                Either::Left((Err(e), _)) => return Err(e),
                Either::Right((trailers, _)) => trailers,
            }?
        };

        assert!(headers.iter().all(|(k0, v0)| response
            .headers
            .0
            .iter()
            .any(|(k1, v1)| k0 == k1 && v0 == v1)));

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

        assert!(trailers.iter().all(|(k0, v0)| response_trailers
            .0
            .iter()
            .any(|(k1, v1)| k0 == k1 && v0 == v1)));

        Ok(())
    }
}
