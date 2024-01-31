mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "service",
        isyswasfa: "-service",
        exports: {
            "component:test/http-handler": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        exports::component::test::http_handler::{Guest, Request, Response},
        isyswasfa::io::pipe,
    },
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(request: Request) -> Response {
        Response {
            status: 200,
            headers: Vec::new(),
            body: request.body.map(|request_rx| {
                let (response_tx, response_rx) = pipe::make_pipe();

                isyswasfa_guest::spawn(async move {
                    isyswasfa_guest::copy(&request_rx, &response_tx)
                        .await
                        .unwrap();
                });

                response_rx
            }),
        }
    }
}
