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
    flate2::{
        write::{DeflateDecoder, DeflateEncoder},
        Compression,
    },
    std::io::Write,
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(mut request: Request) -> Response {
        let mut accept_deflated = false;
        let mut content_deflated = false;
        request
            .headers
            .retain(|(k, v)| match (k.as_str(), v.as_slice()) {
                ("accept-encoding", b"deflate") => {
                    accept_deflated = true;
                    false
                }
                ("content-encoding", b"deflate") => {
                    content_deflated = true;
                    false
                }
                _ => true,
            });

        if content_deflated {
            request.body = request.body.map(|rx| {
                let (pipe_tx, pipe_rx) = pipe::make_pipe();

                isyswasfa_guest::spawn(async move {
                    let mut decoder = DeflateDecoder::new(Vec::new());

                    while let Some(chunk) = isyswasfa_guest::read(&rx, 64 * 1024).await.unwrap() {
                        decoder.write_all(&chunk).unwrap();
                        isyswasfa_guest::write_all(&pipe_tx, decoder.get_ref())
                            .await
                            .unwrap();
                        decoder.get_mut().clear()
                    }

                    isyswasfa_guest::write_all(&pipe_tx, &decoder.finish().unwrap())
                        .await
                        .unwrap();
                });

                pipe_rx
            });
        }

        let mut response = http_handler::handle(request).await;

        if accept_deflated {
            response
                .headers
                .push(("content-encoding".into(), b"deflate".into()));

            response.body = response.body.map(|rx| {
                let (pipe_tx, pipe_rx) = pipe::make_pipe();

                isyswasfa_guest::spawn(async move {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());

                    while let Some(chunk) = isyswasfa_guest::read(&rx, 64 * 1024).await.unwrap() {
                        encoder.write_all(&chunk).unwrap();
                        isyswasfa_guest::write_all(&pipe_tx, encoder.get_ref())
                            .await
                            .unwrap();
                        encoder.get_mut().clear()
                    }

                    isyswasfa_guest::write_all(&pipe_tx, &encoder.finish().unwrap())
                        .await
                        .unwrap();
                });

                pipe_rx
            });
        }

        response
    }
}
