mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "proxy",
        isyswasfa: "-hash-all",
        exports: {
            "wasi:http/handler": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        exports::wasi::http::handler::Guest,
        isyswasfa::io::pipe,
        wasi::http::{
            handler,
            types::{Body, ErrorCode, Fields, Method, Request, Response, Scheme},
        },
    },
    url::Url,
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    /// Return a response which either echoes the request body and trailers or forwards the request body to an
    /// external URL and forwards response to the client.
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let headers = request.headers().entries();
        let method = request.method();
        let filtered_headers = Fields::from_list(
            &headers
                .iter()
                .filter(|(k, _)| k == "content-type")
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect::<Vec<_>>(),
        )
        .unwrap();

        match (&method, request.path_with_query().as_deref()) {
            (Method::Post, Some("/echo")) => {
                // Echo the request body without buffering it.

                Ok(Response::new(filtered_headers, Request::consume(request)))
            }

            (Method::Post, Some("/double-echo")) => {
                // Pipe the request body to an outgoing request and stream the response back to the client.

                if let Some(url) = headers.iter().find_map(|(k, v)| {
                    (k == "url")
                        .then_some(v)
                        .and_then(|v| std::str::from_utf8(v).ok())
                        .and_then(|v| Url::parse(v).ok())
                }) {
                    let request = Request::new(filtered_headers, Request::consume(request), None);

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

            _ => Ok(respond(405)),
        }
    }
}

fn respond(status: u16) -> Response {
    let response = Response::new(Fields::new(), Body::new(pipe::make_pipe().1, None));
    response.set_status_code(status).unwrap();
    response
}
