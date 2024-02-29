mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "service",
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
        isyswasfa::io::pipe,
        wasi::http::types::{ErrorCode, Headers, Request, Response},
    },
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let (response_tx, response_rx) = pipe::make_pipe();
        let request_rx = request.body().unwrap();

        isyswasfa_guest::spawn(async move {
            isyswasfa_guest::copy(&request_rx, &response_tx)
                .await
                .unwrap();
        });

        Ok(Response::new(Headers::new(), response_rx, None))
    }
}
