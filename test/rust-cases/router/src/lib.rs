#![deny(warnings)]

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "router",
        isyswasfa: "-router"
    });

    use super::Component;
    impl Guest for Component {}
    export!(Component);
}

use {
    async_trait::async_trait,
    bindings::{
        echo,
        exports::wasi::http::handler::Guest,
        hash_all,
        isyswasfa::io::pipe,
        middleware, service,
        wasi::http::{
            handler,
            types::{Body, ErrorCode, Fields, Request, Response, Scheme},
        },
    },
    url::Url,
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        match request.path_with_query().as_deref() {
            Some("/echo" | "/double-echo") => echo::handle(request).await,
            Some("/proxy") => {
                let method = request.method();
                let (headers, body) = Request::into_parts(request);
                let urls = headers.delete(&"url".into()).unwrap();
                if let Some(url) = urls
                    .first()
                    .and_then(|v| std::str::from_utf8(v).ok())
                    .and_then(|v| Url::parse(v).ok())
                {
                    let request = Request::new(headers, body, None);
                    request.set_method(&method).unwrap();
                    request.set_path_with_query(Some(url.path())).unwrap();
                    request
                        .set_scheme(Some(&match url.scheme() {
                            "http" => Scheme::Http,
                            "https" => Scheme::Https,
                            scheme => Scheme::Other(scheme.into()),
                        }))
                        .unwrap();
                    request.set_authority(Some(url.authority())).unwrap();

                    handler::handle(request).await
                } else {
                    Ok(respond(400))
                }
            }
            Some("/hash-all") => hash_all::handle(request).await,
            Some("/service") => service::handle(request).await,
            Some("/middleware") => middleware::handle(request).await,
            _ => Ok(respond(405)),
        }
    }
}

fn respond(status: u16) -> Response {
    let response = Response::new(Fields::new(), Body::new(pipe::make_pipe().1, None));
    response.set_status_code(status).unwrap();
    response
}
