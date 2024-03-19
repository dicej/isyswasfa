#![deny(warnings)]

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "proxy",
        isyswasfa: "-service"
    });

    use super::Component;
    impl Guest for Component {}
    export!(Component);
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
        let (headers, body) = Request::into_parts(request);

        Ok(Response::new(headers, body))
    }
}
