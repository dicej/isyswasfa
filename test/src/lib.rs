#[cfg(test)]
mod test {
    use {
        anyhow::{anyhow, Result},
        async_trait::async_trait,
        isyswasfa_host::{IsyswasfaCtx, IsyswasfaView},
        std::{env, time::Duration},
        tokio::{fs, process::Command},
        wasmtime::{
            component::{Component, Linker, ResourceTable},
            Config, Engine, Store,
        },
        wasmtime_wasi::preview2::{command, WasiCtx, WasiCtxBuilder, WasiView},
        wit_component::ComponentEncoder,
    };

    wasmtime::component::bindgen!({
        path: "guest/wit",
        isyswasfa: true,
    });

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

    #[tokio::test]
    async fn round_trip() -> Result<()> {
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
        impl component::guest::baz::Host for Ctx {
            async fn foo(_state: (), s: String) -> wasmtime::Result<String> {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(format!("{s} - entered host - exited host"))
            }
        }

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = Engine::new(&config)?;

        let component_bytes = build_component("guest", "guest").await?;

        let component = Component::new(&engine, &component_bytes)?;

        let mut linker = Linker::new(&engine);

        command::add_to_linker(&mut linker)?;
        isyswasfa_host::add_to_linker(&mut linker)?;

        Bar::add_to_linker(&mut linker, |ctx| ctx)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
            },
        );

        let (command, instance) = Bar::instantiate_async(&mut store, &component, &linker).await?;

        isyswasfa_host::load_poll_funcs(&mut store, &component_bytes, &instance)?;

        let value = command
            .component_guest_baz()
            .call_foo(&mut store, "hello, world!")
            .await?;

        assert_eq!(
            "hello, world! - entered guest - entered host - exited host - exited guest",
            &value
        );

        Ok(())
    }
}
