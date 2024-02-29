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
        service::wasi::http::types::{ErrorCode, HeaderError, Method, RequestOptionsError, Scheme},
        std::{
            env,
            io::Write,
            path::Path,
            pin::pin,
            sync::{Arc, Mutex},
            time::Duration,
        },
        tokio::{fs, process::Command},
        wasmtime::{
            component::{Component, Linker, Resource, ResourceTable},
            Config, Engine, Store,
        },
        wasmtime_wasi::preview2::{
            bindings::wasi::io::error::Error, command, InputStream, StreamError, WasiCtx,
            WasiCtxBuilder, WasiView,
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
                "wasi:http/types/request": super::Request,
                "wasi:http/types/request-options": super::RequestOptions,
                "wasi:http/types/response": super::Response,
                "wasi:http/types/fields": super::Fields,
                "wasi:http/types/isyswasfa-receiver-own-trailers": super::FieldsReceiver,
                "wasi:http/types/isyswasfa-sender-own-trailers": super::FieldsSender,
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
        shared_table: Arc<Mutex<ResourceTable>>,
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
        type State = Arc<Mutex<ResourceTable>>;

        fn isyswasfa(&mut self) -> &mut IsyswasfaCtx {
            &mut self.isyswasfa
        }
        fn state(&self) -> Self::State {
            self.shared_table.clone()
        }
    }

    pub struct Fields(Vec<(String, Vec<u8>)>);

    pub struct FieldsSender(oneshot::Sender<Resource<Fields>>);

    pub struct FieldsReceiver(oneshot::Receiver<Resource<Fields>>);

    #[derive(Default)]
    pub struct RequestOptions {
        connect_timeout: Option<u64>,
        first_byte_timeout: Option<u64>,
        between_bytes_timeout: Option<u64>,
    }

    pub struct Request {
        method: Method,
        scheme: Option<Scheme>,
        path_with_query: Option<String>,
        authority: Option<String>,
        headers: Fields,
        body: Option<InputStream>,
        trailers: Option<FieldsReceiver>,
        options: Option<RequestOptions>,
    }

    pub struct Response {
        status_code: u16,
        headers: Fields,
        body: Option<InputStream>,
        trailers: Option<FieldsReceiver>,
    }

    #[async_trait]
    impl service::wasi::http::types::HostIsyswasfaSenderOwnTrailers for Ctx {
        fn drop(&mut self, this: Resource<FieldsSender>) -> wasmtime::Result<()> {
            self.table().delete(this)?;
            Ok(())
        }
    }

    #[async_trait]
    impl service::wasi::http::types::HostIsyswasfaReceiverOwnTrailers for Ctx {
        fn drop(&mut self, this: Resource<FieldsReceiver>) -> wasmtime::Result<()> {
            self.table().delete(this)?;
            Ok(())
        }
    }

    impl service::wasi::http::types::HostFields for Ctx {
        fn new(&mut self) -> wasmtime::Result<Resource<Fields>> {
            Ok(self.table().push(Fields(Vec::new()))?)
        }

        fn from_list(
            &mut self,
            list: Vec<(String, Vec<u8>)>,
        ) -> wasmtime::Result<Result<Resource<Fields>, HeaderError>> {
            Ok(Ok(self.table().push(Fields(list))?))
        }

        fn get(&mut self, this: Resource<Fields>, key: String) -> wasmtime::Result<Vec<Vec<u8>>> {
            Ok(self
                .table()
                .get(&this)?
                .0
                .iter()
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .collect())
        }

        fn has(&mut self, this: Resource<Fields>, key: String) -> wasmtime::Result<bool> {
            Ok(self.table().get(&this)?.0.iter().any(|(k, _)| *k == key))
        }

        fn set(
            &mut self,
            this: Resource<Fields>,
            key: String,
            values: Vec<Vec<u8>>,
        ) -> wasmtime::Result<Result<(), HeaderError>> {
            let fields = self.table().get_mut(&this)?;
            fields.0.retain(|(k, _)| *k != key);
            fields
                .0
                .extend(values.into_iter().map(|v| (key.clone(), v)));
            Ok(Ok(()))
        }

        fn delete(
            &mut self,
            this: Resource<Fields>,
            key: String,
        ) -> wasmtime::Result<Result<(), HeaderError>> {
            self.table().get_mut(&this)?.0.retain(|(k, _)| *k != key);
            Ok(Ok(()))
        }

        fn append(
            &mut self,
            this: Resource<Fields>,
            key: String,
            value: Vec<u8>,
        ) -> wasmtime::Result<Result<(), HeaderError>> {
            self.table().get_mut(&this)?.0.push((key, value));
            Ok(Ok(()))
        }

        fn entries(&mut self, this: Resource<Fields>) -> wasmtime::Result<Vec<(String, Vec<u8>)>> {
            Ok(self.table().get(&this)?.0.clone())
        }

        fn clone(&mut self, this: Resource<Fields>) -> wasmtime::Result<Resource<Fields>> {
            let entries = self.table().get(&this)?.0.clone();
            Ok(self.table().push(Fields(entries))?)
        }

        fn drop(&mut self, this: Resource<Fields>) -> wasmtime::Result<()> {
            self.table().delete(this)?;
            Ok(())
        }
    }

    #[async_trait]
    impl service::wasi::http::types::HostRequest for Ctx {
        fn new(
            &mut self,
            headers: Resource<Fields>,
            body: Resource<InputStream>,
            trailers: Option<Resource<FieldsReceiver>>,
            options: Option<Resource<RequestOptions>>,
        ) -> wasmtime::Result<Resource<Request>> {
            let headers = self.table().delete(headers)?;
            let body = self.table().delete(body)?;
            let trailers = if let Some(trailers) = trailers {
                Some(self.table().delete(trailers)?)
            } else {
                None
            };
            let options = if let Some(options) = options {
                Some(self.table().delete(options)?)
            } else {
                None
            };

            Ok(self.shared_table.lock().unwrap().push(Request {
                method: Method::Get,
                scheme: None,
                path_with_query: None,
                authority: None,
                headers,
                body: Some(body),
                trailers,
                options,
            })?)
        }

        fn method(&mut self, this: Resource<Request>) -> wasmtime::Result<Method> {
            Ok(self.shared_table.lock().unwrap().get(&this)?.method.clone())
        }

        fn set_method(
            &mut self,
            this: Resource<Request>,
            method: Method,
        ) -> wasmtime::Result<Result<(), ()>> {
            self.shared_table.lock().unwrap().get_mut(&this)?.method = method;
            Ok(Ok(()))
        }

        fn scheme(&mut self, this: Resource<Request>) -> wasmtime::Result<Option<Scheme>> {
            Ok(self.shared_table.lock().unwrap().get(&this)?.scheme.clone())
        }

        fn set_scheme(
            &mut self,
            this: Resource<Request>,
            scheme: Option<Scheme>,
        ) -> wasmtime::Result<Result<(), ()>> {
            self.shared_table.lock().unwrap().get_mut(&this)?.scheme = scheme;
            Ok(Ok(()))
        }

        fn path_with_query(&mut self, this: Resource<Request>) -> wasmtime::Result<Option<String>> {
            Ok(self
                .shared_table
                .lock()
                .unwrap()
                .get(&this)?
                .path_with_query
                .clone())
        }

        fn set_path_with_query(
            &mut self,
            this: Resource<Request>,
            path_with_query: Option<String>,
        ) -> wasmtime::Result<Result<(), ()>> {
            self.shared_table
                .lock()
                .unwrap()
                .get_mut(&this)?
                .path_with_query = path_with_query;
            Ok(Ok(()))
        }

        fn authority(&mut self, this: Resource<Request>) -> wasmtime::Result<Option<String>> {
            Ok(self
                .shared_table
                .lock()
                .unwrap()
                .get(&this)?
                .authority
                .clone())
        }

        fn set_authority(
            &mut self,
            this: Resource<Request>,
            authority: Option<String>,
        ) -> wasmtime::Result<Result<(), ()>> {
            self.shared_table.lock().unwrap().get_mut(&this)?.authority = authority;
            Ok(Ok(()))
        }

        fn options(
            &mut self,
            _this: Resource<Request>,
        ) -> wasmtime::Result<Option<Resource<RequestOptions>>> {
            Err(anyhow!("todo: implement wasi:http/types#request.options"))
        }

        fn headers(&mut self, _this: Resource<Request>) -> wasmtime::Result<Resource<Fields>> {
            Err(anyhow!("todo: implement wasi:http/types#request.headers"))
        }

        fn body(
            &mut self,
            this: Resource<Request>,
        ) -> wasmtime::Result<Result<Resource<InputStream>, ()>> {
            let body = self
                .shared_table
                .lock()
                .unwrap()
                .get_mut(&this)?
                .body
                .take()
                .ok_or_else(|| {
                    anyhow!("todo: allow wasi:http/types#request.body to be called multiple times")
                })?;

            Ok(Ok(self.table().push(body)?))
        }

        async fn finish(
            table: Arc<Mutex<ResourceTable>>,
            this: Resource<Request>,
        ) -> wasmtime::Result<Result<Option<Resource<Fields>>, ErrorCode>> {
            let trailers = table.lock().unwrap().delete(this)?.trailers;
            Ok(Ok(if let Some(trailers) = trailers {
                Some(trailers.0.await?)
            } else {
                None
            }))
        }

        fn drop(&mut self, this: Resource<Request>) -> wasmtime::Result<()> {
            self.shared_table.lock().unwrap().delete(this)?;
            Ok(())
        }
    }

    #[async_trait]
    impl service::wasi::http::types::HostResponse for Ctx {
        fn new(
            &mut self,
            headers: Resource<Fields>,
            body: Resource<InputStream>,
            trailers: Option<Resource<FieldsReceiver>>,
        ) -> wasmtime::Result<Resource<Response>> {
            let headers = self.table().delete(headers)?;
            let body = self.table().delete(body)?;
            let trailers = if let Some(trailers) = trailers {
                Some(self.table().delete(trailers)?)
            } else {
                None
            };

            Ok(self.shared_table.lock().unwrap().push(Response {
                status_code: 200,
                headers,
                body: Some(body),
                trailers,
            })?)
        }

        fn status_code(&mut self, this: Resource<Response>) -> wasmtime::Result<u16> {
            Ok(self.shared_table.lock().unwrap().get(&this)?.status_code)
        }

        fn set_status_code(
            &mut self,
            this: Resource<Response>,
            status_code: u16,
        ) -> wasmtime::Result<Result<(), ()>> {
            self.shared_table
                .lock()
                .unwrap()
                .get_mut(&this)?
                .status_code = status_code;
            Ok(Ok(()))
        }

        fn headers(&mut self, _this: Resource<Response>) -> wasmtime::Result<Resource<Fields>> {
            Err(anyhow!("todo: implement wasi:http/types#response.headers"))
        }

        fn body(
            &mut self,
            this: Resource<Response>,
        ) -> wasmtime::Result<Result<Resource<InputStream>, ()>> {
            let body = self
                .shared_table
                .lock()
                .unwrap()
                .get_mut(&this)?
                .body
                .take()
                .ok_or_else(|| {
                    anyhow!("todo: allow wasi:http/types#response.body to be called multiple times")
                })?;

            Ok(Ok(self.table().push(body)?))
        }

        async fn finish(
            table: Arc<Mutex<ResourceTable>>,
            this: Resource<Response>,
        ) -> wasmtime::Result<Result<Option<Resource<Fields>>, ErrorCode>> {
            let trailers = table.lock().unwrap().delete(this)?.trailers;
            Ok(Ok(if let Some(trailers) = trailers {
                Some(trailers.0.await?)
            } else {
                None
            }))
        }

        fn drop(&mut self, this: Resource<Response>) -> wasmtime::Result<()> {
            self.shared_table.lock().unwrap().delete(this)?;
            Ok(())
        }
    }

    impl service::wasi::http::types::HostRequestOptions for Ctx {
        fn new(&mut self) -> wasmtime::Result<Resource<RequestOptions>> {
            Ok(self
                .shared_table
                .lock()
                .unwrap()
                .push(RequestOptions::default())?)
        }

        fn connect_timeout(
            &mut self,
            this: Resource<RequestOptions>,
        ) -> wasmtime::Result<Option<u64>> {
            Ok(self.table().get(&this)?.connect_timeout)
        }

        fn set_connect_timeout(
            &mut self,
            this: Resource<RequestOptions>,
            connect_timeout: Option<u64>,
        ) -> wasmtime::Result<Result<(), RequestOptionsError>> {
            self.table().get_mut(&this)?.connect_timeout = connect_timeout;
            Ok(Ok(()))
        }

        fn first_byte_timeout(
            &mut self,
            this: Resource<RequestOptions>,
        ) -> wasmtime::Result<Option<u64>> {
            Ok(self.table().get(&this)?.first_byte_timeout)
        }

        fn set_first_byte_timeout(
            &mut self,
            this: Resource<RequestOptions>,
            first_byte_timeout: Option<u64>,
        ) -> wasmtime::Result<Result<(), RequestOptionsError>> {
            self.table().get_mut(&this)?.first_byte_timeout = first_byte_timeout;
            Ok(Ok(()))
        }

        fn between_bytes_timeout(
            &mut self,
            this: Resource<RequestOptions>,
        ) -> wasmtime::Result<Option<u64>> {
            Ok(self.table().get(&this)?.between_bytes_timeout)
        }

        fn set_between_bytes_timeout(
            &mut self,
            this: Resource<RequestOptions>,
            between_bytes_timeout: Option<u64>,
        ) -> wasmtime::Result<Result<(), RequestOptionsError>> {
            self.table().get_mut(&this)?.between_bytes_timeout = between_bytes_timeout;
            Ok(Ok(()))
        }

        fn drop(&mut self, this: Resource<RequestOptions>) -> wasmtime::Result<()> {
            self.table().delete(this)?;
            Ok(())
        }
    }

    impl service::wasi::http::types::Host for Ctx {
        fn http_error_code(
            &mut self,
            _error: Resource<Error>,
        ) -> wasmtime::Result<Option<ErrorCode>> {
            Err(anyhow!("todo: implement wasi:http/types#http-error-code"))
        }
    }

    #[async_trait]
    impl round_trip::component::test::baz::Host for Ctx {
        async fn foo(_state: Arc<Mutex<ResourceTable>>, s: String) -> wasmtime::Result<String> {
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
                shared_table: Arc::new(Mutex::new(ResourceTable::new())),
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
        service::Service::add_to_linker(&mut linker, |ctx| ctx)?;

        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                isyswasfa: IsyswasfaCtx::new(),
                shared_table: Arc::new(Mutex::new(ResourceTable::new())),
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

        let request = store
            .data_mut()
            .shared_table
            .lock()
            .unwrap()
            .push(Request {
                method: Method::Post,
                scheme: Some(Scheme::Http),
                path_with_query: Some("/foo".into()),
                authority: Some("localhost".into()),
                headers: Fields(if use_compression {
                    vec![
                        ("content-encoding".into(), b"deflate".into()),
                        ("accept-encoding".into(), b"deflate".into()),
                    ]
                } else {
                    Vec::new()
                }),
                body: Some(InputStream::Host(Box::new(ReceiverStream::new(request_rx)))),
                trailers: None,
                options: None,
            })?;

        let response = service
            .wasi_http_handler()
            .call_handle(&mut store, request)
            .await??;

        let mut response = store
            .data_mut()
            .shared_table
            .lock()
            .unwrap()
            .delete(response)?;

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

        let poll_loop = isyswasfa_host::poll_loop(&mut store);
        let poll_loop = pin!(poll_loop);

        let response_body = match future::select(poll_loop, response_body).await {
            Either::Left((Ok(()), body)) => body.await,
            Either::Left((Err(e), _)) => return Err(e),
            Either::Right((body, _)) => body,
        }?;

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

        Ok(())
    }
}
