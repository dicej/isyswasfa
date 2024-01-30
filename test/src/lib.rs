#[cfg(test)]
mod test {
    use {
        anyhow::{anyhow, Result},
        async_trait::async_trait,
        bytes::Bytes,
        futures::future::{self, Either},
        http_body_util::{combinators::BoxBody, BodyExt, Full},
        hyper::Request,
        isyswasfa_host::{IsyswasfaCtx, IsyswasfaView},
        std::{env, pin::pin, time::Duration},
        tokio::{fs, process::Command},
        wasmtime::{
            component::{Component, Linker, ResourceTable},
            Config, Engine, Store,
        },
        wasmtime_wasi::preview2::{command, WasiCtx, WasiCtxBuilder, WasiView},
        wasmtime_wasi_http::{proxy, WasiHttpCtx, WasiHttpView},
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
                "wasi:http/types": wasmtime_wasi_http::bindings::wasi::http::types,
            }
        });
    }

    async fn build_component(src_path: &str, name: &str) -> Result<Vec<u8>> {
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
        wasi_http: WasiHttpCtx,
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

    impl WasiHttpView for Ctx {
        fn table(&mut self) -> &mut ResourceTable {
            self.isyswasfa.table()
        }
        fn ctx(&mut self) -> &mut WasiHttpCtx {
            &mut self.wasi_http
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
                wasi_http: WasiHttpCtx,
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
    async fn service() -> Result<()> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component_bytes = build_component("service", "service").await?;

        let component = Component::new(&engine, &component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        proxy::add_only_http_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                wasi_http: WasiHttpCtx,
                isyswasfa: IsyswasfaCtx::new(),
            },
        );

        let (service, instance) =
            service::Service::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, &component_bytes, &instance)?;

        let body = b"All mimsy were the borogoves";

        let request = store
            .data_mut()
            .new_incoming_request(Request::post("/foo").body(BoxBody::new(
                Full::new(Bytes::from_static(body)).map_err(|_| unreachable!()),
            ))?)?;

        let response = service
            .component_test_incoming_handler()
            .call_handle(&mut store, request)
            .await?;

        let response_body = {
            let response = WasiHttpView::table(store.data_mut()).get_mut(&response)?;
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
}
