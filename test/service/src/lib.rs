mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "service",
        isyswasfa: "-service",
        exports: {
            "component:test/incoming-handler": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        exports::component::test::incoming_handler::Guest,
        wasi::{
            http::types::{Fields, IncomingBody, IncomingRequest, OutgoingBody, OutgoingResponse},
            io::streams::{InputStream, OutputStream, StreamError},
        },
    },
};

const MAX_READ: u64 = 64 * 1024;

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(request: IncomingRequest) -> OutgoingResponse {
        let response = OutgoingResponse::new(Fields::new());
        let response_body = response.body().unwrap();

        isyswasfa_guest::spawn(async move {
            let request_body = request.consume().unwrap();
            copy(
                request_body.stream().unwrap(),
                response_body.write().unwrap(),
            )
            .await;
            IncomingBody::finish(request_body);
            OutgoingBody::finish(response_body, None).unwrap();
        });

        response
    }
}

async fn copy(rx: InputStream, tx: OutputStream) {
    loop {
        match rx.read(MAX_READ) {
            Ok(chunk) if chunk.is_empty() => rx.subscribe().await,
            Ok(chunk) => {
                let mut offset = 0;
                while offset < chunk.len() {
                    let count = usize::try_from(tx.check_write().unwrap())
                        .unwrap()
                        .min(chunk.len() - offset);

                    if count > 0 {
                        tx.write(&chunk[offset..][..count]).unwrap();
                        offset += count
                    } else {
                        tx.subscribe().await
                    }
                }
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(error)) => {
                panic!("{}", error.to_debug_string())
            }
        }
    }
}
