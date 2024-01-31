mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "wasi-http-handler",
        isyswasfa: "-wasi-http",
        exports: {
            "component:test/incoming-handler": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        exports::component::test::incoming_handler::Guest,
        wasi::http::types::{
            Fields, IncomingBody, IncomingRequest, OutgoingBody, OutgoingResponse,
        },
    },
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(request: IncomingRequest) -> OutgoingResponse {
        let response = OutgoingResponse::new(Fields::new());
        let response_body = response.body().unwrap();

        isyswasfa_guest::spawn(async move {
            let request_body = request.consume().unwrap();
            isyswasfa_guest::copy(
                &request_body.stream().unwrap(),
                &response_body.write().unwrap(),
            )
            .await
            .unwrap();
            IncomingBody::finish(request_body);
            OutgoingBody::finish(response_body, None).unwrap();
        });

        response
    }
}
