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
    isyswasfa_guest::streams_interface::InputStream,
    std::io::Write,
};

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    async fn handle(request: Request) -> Response {
        let accept_deflated = accept_deflated(&request.headers);

        let mut response = http_handler::handle(Request {
            body: request.body.map(|rx| {
                if true || is_deflated(&request.headers) {
                    deflate_decode(rx)
                } else {
                    rx
                }
            }),
            ..request
        })
        .await;

        if accept_deflated {
            response
                .headers
                .push(("content-encoding".into(), b"deflate".into()));
        }

        Response {
            body: response.body.map(|rx| {
                if accept_deflated {
                    deflate_encode(rx)
                } else {
                    rx
                }
            }),
            ..response
        }
    }
}

fn accept_deflated(headers: &[(String, Vec<u8>)]) -> bool {
    headers
        .iter()
        .any(|(k, v)| matches!((k.as_str(), v.as_slice()), ("accept-encoding", b"deflate")))
}

fn is_deflated(headers: &[(String, Vec<u8>)]) -> bool {
    headers
        .iter()
        .any(|(k, v)| matches!((k.as_str(), v.as_slice()), ("content-encoding", b"deflate")))
}

fn deflate_decode(rx: InputStream) -> InputStream {
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
}

fn deflate_encode(rx: InputStream) -> InputStream {
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
}
