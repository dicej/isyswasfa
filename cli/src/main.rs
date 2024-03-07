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

use {
    anyhow::{anyhow, bail, Error, Result},
    async_trait::async_trait,
    bytes::Bytes,
    clap::Parser,
    futures::{
        channel::{mpsc, oneshot},
        future::FutureExt,
        sink::SinkExt,
        stream::{FuturesUnordered, StreamExt, TryStreamExt},
    },
    http_body_util::{combinators::BoxBody, BodyExt, StreamBody},
    hyper::{body::Frame, server::conn::http1, service},
    hyper_util::rt::tokio::TokioIo,
    isyswasfa_host::{InputStream, IsyswasfaCtx, IsyswasfaView, ReceiverStream},
    isyswasfa_http::{
        wasi::http::types::{ErrorCode, Method, Scheme},
        Body, Fields, FieldsReceiver, Request, Response, WasiHttpState, WasiHttpView,
    },
    std::{
        net::{IpAddr, Ipv4Addr},
        ops::Deref,
        path::PathBuf,
        sync::{Arc, Mutex, MutexGuard},
    },
    tokio::{fs, net::TcpListener},
    wasmtime::{
        component::{Component, InstancePre, Linker, Resource, ResourceTable},
        Config, Engine, Store,
    },
    wasmtime_wasi::preview2::{command, StreamError, WasiCtx, WasiCtxBuilder, WasiView},
};

const MAX_READ_SIZE: usize = 64 * 1024;

/// A utility to experiment with WASI 0.3.0-style composable concurrency
#[derive(clap::Parser, Debug)]
#[command(author, version, about)]
struct Options {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Run an HTTP server, passing requests to the specified component.
    Serve(Serve),
}

#[derive(clap::Args, Debug)]
struct Serve {
    /// The component used to handle incoming HTTP requests.
    ///
    /// The component must export `wasi:http/handler@0.3.0-draft`.
    component: PathBuf,
}

#[derive(Clone)]
struct HttpState {
    shared_table: Arc<Mutex<ResourceTable>>,
    client: Arc<reqwest::Client>,
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
        let mut request = self.shared_table().delete(request)?;

        if request.options.is_some() {
            bail!("todo: handle outgoing request options");
        }

        if request.body.trailers.is_some() {
            bail!("todo: handle outgoing request trailers");
        }

        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Head => reqwest::Method::HEAD,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
            Method::Connect => reqwest::Method::CONNECT,
            Method::Options => reqwest::Method::OPTIONS,
            Method::Trace => reqwest::Method::TRACE,
            Method::Patch => reqwest::Method::PATCH,
            Method::Other(s) => match reqwest::Method::from_bytes(s.as_bytes()) {
                Ok(method) => method,
                Err(e) => {
                    // TODO: map errors more precisely
                    return Ok(Err(ErrorCode::InternalError(Some(format!("{e:?}")))));
                }
            },
        };
        let scheme = request.scheme.unwrap_or(Scheme::Http);
        let authority = if let Some(authority) = request.authority {
            authority
        } else {
            match scheme {
                Scheme::Http => ":80",
                Scheme::Https => ":443",
                _ => bail!("unable to determine authority for {scheme:?}"),
            }
            .into()
        };
        let path = request.path_with_query.unwrap_or_else(|| "/".into());

        let InputStream::Host(mut request_rx) = request.body.stream.take().unwrap() else {
            todo!("handle non-`InputStream::Host` case");
        };

        let (mut request_body_tx, request_body_rx) = mpsc::channel(1);

        tokio::spawn(
            async move {
                loop {
                    match request_rx.read(MAX_READ_SIZE) {
                        Ok(bytes) if bytes.is_empty() => request_rx.ready().await,
                        Ok(bytes) => request_body_tx.send(bytes).await?,
                        Err(StreamError::Closed) => break Ok(()),
                        Err(e) => break Err(anyhow!("error reading response body: {e:?}")),
                    }
                }
            }
            .map(|result| {
                if let Err(e) = result {
                    eprintln!("error streaming request body: {e:?}");
                }
            }),
        );

        let response = self
            .client
            .request(method, format!("{scheme}://{authority}{path}"))
            .body(reqwest::Body::wrap_stream(
                request_body_rx.map(Ok::<_, Error>),
            ))
            .send()
            .await
            .map_err(|e| {
                // TODO: map errors more precisely
                ErrorCode::InternalError(Some(format!("{e:?}")))
            })?;

        let status_code = response.status().as_u16();
        let headers = Fields(
            response
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().into(), v.as_bytes().into()))
                .collect(),
        );

        let (mut response_body_tx, response_body_rx) = mpsc::channel(1);

        tokio::spawn(
            async move {
                let mut stream = response.bytes_stream();

                while let Some(chunk) = stream.try_next().await? {
                    response_body_tx.send(chunk).await?;
                }

                Ok::<_, Error>(())
            }
            .map(|result| {
                if let Err(e) = result {
                    eprintln!("error streaming response body: {e:?}");
                }
            }),
        );

        Ok(Ok(self.shared_table().push(Response {
            status_code,
            headers,
            body: Body {
                stream: Some(InputStream::Host(Box::new(ReceiverStream::new(
                    response_body_rx,
                )))),
                // TODO: handle response trailers
                trailers: None,
            },
        })?))
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

async fn handle_request(
    engine: &Engine,
    pre: &InstancePre<Ctx>,
    component_bytes: &[u8],
    request: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<BoxBody<Bytes, Error>>> {
    let mut store = Store::new(
        engine,
        Ctx {
            wasi: WasiCtxBuilder::new().inherit_stdio().build(),
            isyswasfa: IsyswasfaCtx::new(),
            http_state: HttpState {
                shared_table: Arc::new(Mutex::new(ResourceTable::new())),
                client: Arc::new(reqwest::Client::new()),
            },
        },
    );

    let (service, instance) = Proxy::instantiate_pre(&mut store, pre).await?;

    isyswasfa_host::load_poll_funcs(&mut store, component_bytes, &instance)?;

    let (mut request_body_tx, request_body_rx) = mpsc::channel(1);

    let (request_trailers_tx, request_trailers_rx) = oneshot::channel();

    let wasi_request = store.data_mut().shared_table().push(Request {
        method: match request.method() {
            &http::Method::GET => Method::Get,
            &http::Method::POST => Method::Post,
            &http::Method::PUT => Method::Put,
            &http::Method::DELETE => Method::Delete,
            &http::Method::PATCH => Method::Patch,
            &http::Method::HEAD => Method::Head,
            &http::Method::OPTIONS => Method::Options,
            request => Method::Other(request.as_str().into()),
        },
        scheme: request.uri().scheme().map(|scheme| match scheme.as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            _ => Scheme::Other(scheme.as_str().into()),
        }),
        path_with_query: request.uri().path_and_query().map(|p| p.as_str().into()),
        authority: request.uri().authority().map(|a| a.as_str().into()),
        headers: Fields(
            request
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().into(), v.as_bytes().into()))
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

    let pipe_request_body = async move {
        let mut body = request.into_body();

        let mut request_trailers_tx = Some(request_trailers_tx);
        while let Some(frame) = body.frame().await {
            match frame?.into_data() {
                Ok(chunk) => request_body_tx.send(chunk).await?,
                Err(frame) => match frame.into_trailers() {
                    Ok(trailers) => drop(
                        request_trailers_tx
                            .take()
                            .ok_or_else(|| anyhow!("more than one set of trailers received"))?
                            .send(Fields(
                                trailers
                                    .iter()
                                    .map(|(k, v)| (k.as_str().into(), v.as_bytes().into()))
                                    .collect(),
                            )),
                    ),
                    Err(_) => unreachable!(),
                },
            }
        }

        Ok::<_, Error>(None)
    };

    let call_handle = async move {
        let response = service
            .wasi_http_handler()
            .call_handle(&mut store, wasi_request)
            .await??;

        Ok::<_, Error>(Some((response, store)))
    };

    let mut futures = FuturesUnordered::new();
    futures.push(pipe_request_body.boxed());
    futures.push(call_handle.boxed());

    while let Some(event) = futures.try_next().await? {
        if let Some((response, mut store)) = event {
            let response = store.data_mut().shared_table().delete(response)?;

            let mut body = response.body;

            let InputStream::Host(mut response_rx) = body.stream.take().unwrap() else {
                unreachable!();
            };

            let (mut response_body_tx, response_body_rx) = mpsc::channel(1);

            futures.push(
                async move {
                    isyswasfa_host::poll_loop_until(&mut store, async move {
                        loop {
                            match response_rx.read(MAX_READ_SIZE) {
                                Ok(bytes) if bytes.is_empty() => response_rx.ready().await,
                                Ok(bytes) => response_body_tx.send(Ok(Frame::data(bytes))).await?,
                                Err(StreamError::Closed) => break Ok(()),
                                Err(e) => break Err(anyhow!("error reading response body: {e:?}")),
                            }
                        }?;

                        if let Some(trailers) = body.trailers.take() {
                            if let Ok(trailers) = trailers.0.await {
                                response_body_tx
                                    .send(Ok(Frame::trailers(
                                        trailers
                                            .0
                                            .into_iter()
                                            .map(|(k, v)| Ok((k.try_into()?, v.try_into()?)))
                                            .collect::<Result<_>>()?,
                                    )))
                                    .await?;
                            }
                        }

                        Ok::<_, Error>(())
                    })
                    .await??;

                    Ok(None)
                }
                .boxed(),
            );

            tokio::spawn(
                async move {
                    while let Some(event) = futures.try_next().await? {
                        assert!(event.is_none());
                    }

                    Ok::<_, Error>(())
                }
                .map(|result| {
                    if let Err(e) = result {
                        eprintln!("error sending response body: {e:?}");
                    }
                }),
            );

            let mut builder = hyper::Response::builder().status(response.status_code);
            for (k, v) in response.headers.0 {
                builder = builder.header(k, v);
            }
            return Ok(builder.body(BoxBody::new(StreamBody::new(response_body_rx)))?);
        }
    }

    unreachable!()
}

#[tokio::main]
async fn main() -> Result<()> {
    let Options {
        command: Command::Serve(Serve { component }),
    } = Options::parse();

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);

    let engine = Engine::new(&config)?;

    let component_bytes = fs::read(component).await?;

    let component = Component::new(&engine, &component_bytes)?;

    let mut linker = Linker::new(&engine);

    command::add_to_linker(&mut linker)?;
    isyswasfa_host::add_to_linker(&mut linker)?;
    isyswasfa_http::add_to_linker(&mut linker)?;

    let pre = linker.instantiate_pre(&component)?;

    let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080)).await?;

    let state = Arc::new((engine, pre, component_bytes));

    eprintln!("Serving HTTP on http://{}/", listener.local_addr()?);

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::task::spawn(async move {
            if let Err(e) = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(
                    TokioIo::new(stream),
                    service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        let state = state.clone();
                        async move {
                            let (engine, pre, component_bytes) = state.deref();
                            handle_request(engine, pre, component_bytes, request).await
                        }
                    }),
                )
                .await
            {
                eprintln!("error: {e:?}");
            }
        });
    }
}
