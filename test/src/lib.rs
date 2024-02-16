#[cfg(test)]
mod test {
    use {
        anyhow::{anyhow, Result},
        async_trait::async_trait,
        bytes::Bytes,
        futures::{
            channel::mpsc,
            future::{self, Either},
            sink::SinkExt,
        },
        http_body_util::{combinators::BoxBody, BodyExt, Full},
        hyper::Request,
        indexmap::IndexMap,
        isyswasfa_host::{IsyswasfaCtx, IsyswasfaView},
        std::{env, io::Write, path::Path, pin::pin, time::Duration},
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

    mod wasi_http_handler {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "wasi:http/proxy@0.3.0-draft-2024-02-14",
            isyswasfa: true,
            with: {
                "wasi:clocks/monotonic-clock": wasmtime_wasi::preview2::bindings::wasi::clocks::monotonic_clock,
                "wasi:io/error": wasmtime_wasi::preview2::bindings::wasi::io::error,
                "wasi:io/streams": wasmtime_wasi::preview2::bindings::wasi::io::streams,
            }
        });
    }

    mod service {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "service",
            isyswasfa: true,
            with: {
                "wasi:io/error": wasmtime_wasi::preview2::bindings::wasi::io::error,
                "wasi:io/streams": wasmtime_wasi::preview2::bindings::wasi::io::streams,
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

    struct Ctx {
        wasi: WasiCtx,
        isyswasfa: IsyswasfaCtx,
    }

    impl WasiView for Ctx {
        fn table(&mut self) -> &mut ResourceTable {
            self.isyswasfa.table()
        }
        fn ctx(&mut self) -> &mut WasiCtx {
            &mut self.wasi
        }
    }

    impl IsyswasfaView for Ctx {
        type State = ();

        fn isyswasfa(&mut self) -> &mut IsyswasfaCtx {
            &mut self.isyswasfa
        }
        fn state(&self) -> Self::State {}
    }

    #[async_trait]
    impl round_trip::component::test::baz::Host for Ctx {
        async fn foo(_state: (), s: String) -> wasmtime::Result<String> {
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
    async fn wasi_http_handler() -> Result<()> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component_bytes = build_component("wasi-http-handler", "wasi_http_handler").await?;

        let component = Component::new(&engine, &component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
            },
        );

        let (handler, instance) =
            wasi_http_handler::Proxy::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, &component_bytes, &instance)?;

        let body = b"All mimsy were the borogoves";

        let request = store
            .data_mut()
            .new_incoming_request(Request::post("/foo").body(BoxBody::new(
                Full::new(Bytes::from_static(body)).map_err(|_| unreachable!()),
            ))?)?;

        let response = handler
            .wasi_http_handler()
            .call_handle(&mut store, request)
            .await?;

        let response_body = {
            let response = IsyswasfaView::table(store.data_mut()).get_mut(&response)?;
            assert!(response.status.is_success());
            response.body.take().unwrap()
        };

        let poll_loop = isyswasfa_host::poll_loop(&mut store);
        let poll_loop = pin!(poll_loop);

        let response_body = match future::select(poll_loop, response_body.collect()).await {
            Either::Left((Ok(()), collect)) => collect.await,
            Either::Left((Err(e), _)) => return Err(e),
            Either::Right((body, _)) => body,
        }?;

        assert_eq!(body as &[_], &response_body.to_bytes());

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
            service::wasi::http::types::Request,
        };

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component = Component::new(&engine, component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
            },
        );

        let (service, instance) =
            service::Service::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, component_bytes, &instance)?;

        let body = b"And the mome raths outgrabe";

        let request_rx = {
            let (mut request_tx, request_rx) = mpsc::channel(1);

            request_tx
                .send(if use_compression {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
                    encoder.write_all(body)?;
                    encoder.finish()?.into()
                } else {
                    Bytes::from_static(body)
                })
                .await?;

            request_rx
        };

        let request_body = Some(
            WasiView::table(store.data_mut())
                .push(InputStream::Host(Box::new(ReceiverStream::new(request_rx))))?,
        );

        let response = service
            .wasi_http_handler()
            .call_handle(
                &mut store,
                &Request {
                    method: "POST".into(),
                    uri: "/foo".into(),
                    headers: if use_compression {
                        vec![
                            ("content-encoding".into(), b"deflate".into()),
                            ("accept-encoding".into(), b"deflate".into()),
                        ]
                    } else {
                        Vec::new()
                    },
                    body: request_body,
                },
            )
            .await?;

        assert!(response.status == 200);

        let InputStream::Host(mut response_rx) =
            WasiView::table(store.data_mut()).delete(response.body.unwrap())?
        else {
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

        let poll_loop = isyswasfa_host::poll_loop(&mut store);
        let poll_loop = pin!(poll_loop);

        let response_body = match future::select(poll_loop, response_body).await {
            Either::Left((Ok(()), body)) => body.await,
            Either::Left((Err(e), _)) => return Err(e),
            Either::Right((body, _)) => body,
        }?;

        let response_body = if use_compression {
            assert!(response.headers.iter().any(|(k, v)| matches!(
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

        Ok(())
    }
}
