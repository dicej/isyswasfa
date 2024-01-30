mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "middleware",
        isyswasfa: "-middleware",
        exports: {
            "component:test/http-handler": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        component::test::http_handler,
        exports::component::test::http_handler::{Guest, Request, Response},
        isyswasfa::io::pipe,
    },
    isyswasfa_guest::streams_interface::InputStream,
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(
        request: Request,
        request_body: Option<InputStream>,
    ) -> (Response, Option<InputStream>) {
        let pipe = |rx| {
            let (pipe_tx, pipe_rx) = pipe::make_pipe();

            isyswasfa_guest::spawn(async move {
                isyswasfa_guest::copy(rx, pipe_tx).await.unwrap();
            });

            pipe_rx
        };

        let (response, response_body) =
            http_handler::handle(&request, request_body.map(pipe)).await;

        (response, response_body.map(pipe))
    }
}
