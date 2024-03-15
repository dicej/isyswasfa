#![deny(warnings)]

wasmtime::component::bindgen!({
    path: "../wit",
    interfaces: "
      import wasi:http/types@0.3.0-draft;
      import wasi:http/handler@0.3.0-draft;
    ",
    isyswasfa: true,
    with: {
        "wasi:io/error": wasmtime_wasi::preview2::bindings::wasi::io::error,
        "wasi:io/streams": wasmtime_wasi::preview2::bindings::wasi::io::streams,
        "wasi:http/types/body": Body,
        "wasi:http/types/request": Request,
        "wasi:http/types/request-options": RequestOptions,
        "wasi:http/types/response": Response,
        "wasi:http/types/fields": Fields,
        "wasi:http/types/isyswasfa-receiver-own-trailers": FieldsReceiver,
        "wasi:http/types/isyswasfa-sender-own-trailers": FieldsSender,
    }
});

use {
    anyhow::anyhow,
    futures::channel::oneshot,
    isyswasfa_host::IsyswasfaView,
    std::{fmt, future::Future, mem},
    wasi::http::types::{ErrorCode, HeaderError, Method, RequestOptionsError, Scheme},
    wasmtime::component::{Linker, Resource, ResourceTable},
    wasmtime_wasi::preview2::{bindings::wasi::io::error::Error, InputStream},
};

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Scheme::Http => "http",
                Scheme::Https => "https",
                Scheme::Other(s) => s,
            }
        )
    }
}

pub trait WasiHttpView: IsyswasfaView {
    fn table(&mut self) -> &mut ResourceTable;

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
    >;
}

pub struct Body {
    pub stream: Option<InputStream>,
    pub trailers: Option<FieldsReceiver>,
}

#[derive(Clone)]
pub struct Fields(pub Vec<(String, Vec<u8>)>);

pub struct FieldsSender(pub oneshot::Sender<Fields>);

pub struct FieldsReceiver(pub oneshot::Receiver<Fields>);

#[derive(Default, Copy, Clone)]
pub struct RequestOptions {
    pub connect_timeout: Option<u64>,
    pub first_byte_timeout: Option<u64>,
    pub between_bytes_timeout: Option<u64>,
}

pub struct Request {
    pub method: Method,
    pub scheme: Option<Scheme>,
    pub path_with_query: Option<String>,
    pub authority: Option<String>,
    pub headers: Fields,
    pub body: Body,
    pub options: Option<RequestOptions>,
}

pub struct Response {
    pub status_code: u16,
    pub headers: Fields,
    pub body: Body,
}

impl<T: WasiHttpView> wasi::http::types::HostIsyswasfaSenderOwnTrailers for T {
    fn send(
        &mut self,
        this: Resource<FieldsSender>,
        fields: Resource<Fields>,
    ) -> wasmtime::Result<()> {
        let sender = self.table().delete(this)?;
        let fields = self.table().delete(fields)?;
        _ = sender.0.send(fields);
        Ok(())
    }

    fn drop(&mut self, this: Resource<FieldsSender>) -> wasmtime::Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl<T: WasiHttpView> wasi::http::types::HostIsyswasfaReceiverOwnTrailers for T {
    fn receive(
        &mut self,
        this: Resource<FieldsReceiver>,
    ) -> wasmtime::Result<
        impl Future<
                Output = impl FnOnce(&mut Self) -> wasmtime::Result<Option<Resource<Fields>>> + 'static,
            > + Send
            + 'static,
    > {
        let receiver = self.table().delete(this)?;
        Ok(async move {
            let fields = receiver.0.await;
            move |self_: &mut Self| {
                Ok(if let Ok(fields) = fields {
                    Some(self_.table().push(fields)?)
                } else {
                    None
                })
            }
        })
    }

    fn drop(&mut self, this: Resource<FieldsReceiver>) -> wasmtime::Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl<T: WasiHttpView> wasi::http::types::HostFields for T {
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
    ) -> wasmtime::Result<Result<Vec<Vec<u8>>, HeaderError>> {
        let fields = self.table().get_mut(&this)?;
        let (matched, unmatched) = mem::take(&mut fields.0)
            .into_iter()
            .partition(|(k, _)| *k == key);
        fields.0 = unmatched;
        Ok(Ok(matched.into_iter().map(|(_, v)| v).collect()))
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

impl<T: WasiHttpView> wasi::http::types::HostBody for T {
    fn new(
        &mut self,
        stream: Resource<InputStream>,
        trailers: Option<Resource<FieldsReceiver>>,
    ) -> wasmtime::Result<Resource<Body>> {
        let stream = self.table().delete(stream)?;
        let trailers = if let Some(trailers) = trailers {
            Some(self.table().delete(trailers)?)
        } else {
            None
        };

        Ok(self.table().push(Body {
            stream: Some(stream),
            trailers,
        })?)
    }

    fn stream(
        &mut self,
        this: Resource<Body>,
    ) -> wasmtime::Result<Result<Resource<InputStream>, ()>> {
        // TODO: This should return a child handle
        let stream = self.table().get_mut(&this)?.stream.take().ok_or_else(|| {
            anyhow!("todo: allow wasi:http/types#body.stream to be called multiple times")
        })?;

        Ok(Ok(self.table().push(stream)?))
    }

    fn finish(
        &mut self,
        this: Resource<Body>,
    ) -> wasmtime::Result<
        impl Future<
                Output = impl FnOnce(
                    &mut Self,
                )
                    -> wasmtime::Result<Result<Option<Resource<Fields>>, ErrorCode>>
                             + 'static,
            > + Send
            + 'static,
    > {
        let trailers = self.table().delete(this)?.trailers;
        Ok(async move {
            let trailers = if let Some(trailers) = trailers {
                if let Ok(trailers) = trailers.0.await {
                    Some(trailers)
                } else {
                    None
                }
            } else {
                None
            };

            move |self_: &mut Self| {
                Ok(Ok(if let Some(trailers) = trailers {
                    Some(self_.table().push(trailers)?)
                } else {
                    None
                }))
            }
        })
    }

    fn drop(&mut self, this: Resource<Body>) -> wasmtime::Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl<T: WasiHttpView> wasi::http::types::HostRequest for T {
    fn new(
        &mut self,
        headers: Resource<Fields>,
        body: Resource<Body>,
        options: Option<Resource<RequestOptions>>,
    ) -> wasmtime::Result<Resource<Request>> {
        let headers = self.table().delete(headers)?;
        let body = self.table().delete(body)?;
        let options = if let Some(options) = options {
            Some(self.table().delete(options)?)
        } else {
            None
        };

        Ok(self.table().push(Request {
            method: Method::Get,
            scheme: None,
            path_with_query: None,
            authority: None,
            headers,
            body,
            options,
        })?)
    }

    fn method(&mut self, this: Resource<Request>) -> wasmtime::Result<Method> {
        Ok(self.table().get(&this)?.method.clone())
    }

    fn set_method(
        &mut self,
        this: Resource<Request>,
        method: Method,
    ) -> wasmtime::Result<Result<(), ()>> {
        self.table().get_mut(&this)?.method = method;
        Ok(Ok(()))
    }

    fn scheme(&mut self, this: Resource<Request>) -> wasmtime::Result<Option<Scheme>> {
        Ok(self.table().get(&this)?.scheme.clone())
    }

    fn set_scheme(
        &mut self,
        this: Resource<Request>,
        scheme: Option<Scheme>,
    ) -> wasmtime::Result<Result<(), ()>> {
        self.table().get_mut(&this)?.scheme = scheme;
        Ok(Ok(()))
    }

    fn path_with_query(&mut self, this: Resource<Request>) -> wasmtime::Result<Option<String>> {
        Ok(self.table().get(&this)?.path_with_query.clone())
    }

    fn set_path_with_query(
        &mut self,
        this: Resource<Request>,
        path_with_query: Option<String>,
    ) -> wasmtime::Result<Result<(), ()>> {
        self.table().get_mut(&this)?.path_with_query = path_with_query;
        Ok(Ok(()))
    }

    fn authority(&mut self, this: Resource<Request>) -> wasmtime::Result<Option<String>> {
        Ok(self.table().get(&this)?.authority.clone())
    }

    fn set_authority(
        &mut self,
        this: Resource<Request>,
        authority: Option<String>,
    ) -> wasmtime::Result<Result<(), ()>> {
        self.table().get_mut(&this)?.authority = authority;
        Ok(Ok(()))
    }

    fn options(
        &mut self,
        this: Resource<Request>,
    ) -> wasmtime::Result<Option<Resource<RequestOptions>>> {
        // TODO: This should return an immutable child handle
        let options = self.table().get(&this)?.options;
        Ok(if let Some(options) = options {
            Some(self.table().push(options)?)
        } else {
            None
        })
    }

    fn headers(&mut self, this: Resource<Request>) -> wasmtime::Result<Resource<Fields>> {
        // TODO: This should return an immutable child handle
        let headers = self.table().get(&this)?.headers.clone();
        Ok(self.table().push(headers)?)
    }

    fn body(&mut self, _this: Resource<Request>) -> wasmtime::Result<Resource<Body>> {
        Err(anyhow!("todo: implement wasi:http/types#request.body"))
    }

    fn into_parts(
        &mut self,
        this: Resource<Request>,
    ) -> wasmtime::Result<(Resource<Fields>, Resource<Body>)> {
        let request = self.table().delete(this)?;
        let headers = self.table().push(request.headers)?;
        let body = self.table().push(request.body)?;
        Ok((headers, body))
    }

    fn drop(&mut self, this: Resource<Request>) -> wasmtime::Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl<T: WasiHttpView> wasi::http::types::HostResponse for T {
    fn new(
        &mut self,
        headers: Resource<Fields>,
        body: Resource<Body>,
    ) -> wasmtime::Result<Resource<Response>> {
        let headers = self.table().delete(headers)?;
        let body = self.table().delete(body)?;

        Ok(self.table().push(Response {
            status_code: 200,
            headers,
            body,
        })?)
    }

    fn status_code(&mut self, this: Resource<Response>) -> wasmtime::Result<u16> {
        Ok(self.table().get(&this)?.status_code)
    }

    fn set_status_code(
        &mut self,
        this: Resource<Response>,
        status_code: u16,
    ) -> wasmtime::Result<Result<(), ()>> {
        self.table().get_mut(&this)?.status_code = status_code;
        Ok(Ok(()))
    }

    fn headers(&mut self, this: Resource<Response>) -> wasmtime::Result<Resource<Fields>> {
        // TODO: This should return an immutable child handle
        let headers = self.table().get(&this)?.headers.clone();
        Ok(self.table().push(headers)?)
    }

    fn body(&mut self, _this: Resource<Response>) -> wasmtime::Result<Resource<Body>> {
        Err(anyhow!("todo: implement wasi:http/types#response.body"))
    }

    fn into_parts(
        &mut self,
        this: Resource<Response>,
    ) -> wasmtime::Result<(Resource<Fields>, Resource<Body>)> {
        let response = self.table().delete(this)?;
        let headers = self.table().push(response.headers)?;
        let body = self.table().push(response.body)?;
        Ok((headers, body))
    }

    fn drop(&mut self, this: Resource<Response>) -> wasmtime::Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl<T: WasiHttpView> wasi::http::types::HostRequestOptions for T {
    fn new(&mut self) -> wasmtime::Result<Resource<RequestOptions>> {
        Ok(self.table().push(RequestOptions::default())?)
    }

    fn connect_timeout(&mut self, this: Resource<RequestOptions>) -> wasmtime::Result<Option<u64>> {
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

impl<T: WasiHttpView> wasi::http::types::Host for T {
    fn isyswasfa_pipe_own_trailers(
        &mut self,
    ) -> wasmtime::Result<(Resource<FieldsSender>, Resource<FieldsReceiver>)> {
        let (tx, rx) = oneshot::channel();
        let tx = self.table().push(FieldsSender(tx))?;
        let rx = self.table().push(FieldsReceiver(rx))?;
        Ok((tx, rx))
    }

    fn http_error_code(&mut self, _error: Resource<Error>) -> wasmtime::Result<Option<ErrorCode>> {
        Err(anyhow!("todo: implement wasi:http/types#http-error-code"))
    }
}

impl<T: WasiHttpView> wasi::http::handler::Host for T {
    fn handle(
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
        self.send_request(request)
    }
}

pub fn add_to_linker<T: WasiHttpView>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
    wasi::http::types::add_to_linker(linker, |ctx| ctx)?;
    wasi::http::handler::add_to_linker(linker, |ctx| ctx)
}
