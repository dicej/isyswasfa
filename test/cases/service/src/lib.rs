mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "proxy",
        isyswasfa: "-service",
        exports: {
            "wasi:http/handler": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        exports::wasi::http::handler::Guest,
        wasi::http::types::{ErrorCode, Request, Response},
    },
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    /// Return a response which echoes the request headers, body, and trailers.
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let headers = request.headers().clone();
        let (_, body) = Request::into_parts(request);

        Ok(Response::new(headers, body))
    }
}
