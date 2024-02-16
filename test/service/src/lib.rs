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
        wasi::http::types::{self, ErrorCode, IsyswasfaSenderOwnTrailers, Request, Response},
    },
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    /// Return a response which echoes the request headers, body, and trailers.
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        // First, extract the parts of the request.
        let response_headers = request.headers().clone();
        let request_body_rx = request.body().unwrap();

        // Next, spawn a task to pipe the request body and trailers into the response we'll create below.  This
        // will run concurrently with the caller's code (i.e. it won't necessarily complete before we return a
        // value).
        let (response_body_tx, response_body_rx) = pipe::make_pipe();
        let (response_trailers_tx, response_trailers_rx) = types::isyswasfa_pipe_own_trailers();

        isyswasfa_guest::spawn(async move {
            isyswasfa_guest::copy(&request_body_rx, &response_body_tx)
                .await
                .unwrap();

            drop(request_body_rx);

            if let Some(trailers) = Request::finish(request).await.unwrap() {
                IsyswasfaSenderOwnTrailers::send(response_trailers_tx, trailers);
            }
        });

        // While the above task is running, synthesize a response from the parts collected above and return it.
        Ok(Response::new(
            response_headers,
            response_body_rx,
            Some(response_trailers_rx),
        ))
    }
}
