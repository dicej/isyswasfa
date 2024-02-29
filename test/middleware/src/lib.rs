mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "middleware",
        isyswasfa: "-middleware",
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
        wasi::http::{
            handler,
            types::{ErrorCode, Headers, Request, Response},
        },
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
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let mut accept_deflated = false;
        let mut content_deflated = false;
        let mut headers = request.headers().entries();
        headers.retain(|(k, v)| match (k.as_str(), v.as_slice()) {
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

        let rx = request.body().unwrap();
        let rx = if content_deflated {
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
        } else {
            rx
        };

        let my_request = Request::new(Headers::from_list(&headers).unwrap(), rx, None, None);
        my_request.set_method(&request.method()).unwrap();
        my_request.set_scheme(request.scheme().as_ref()).unwrap();
        my_request
            .set_path_with_query(request.path_with_query().as_deref())
            .unwrap();
        my_request
            .set_authority(request.authority().as_deref())
            .unwrap();

        let response = handler::handle(request).await?;

        let mut headers = response.headers().entries();
        let rx = response.body().unwrap();
        let rx = if accept_deflated {
            headers.push(("content-encoding".into(), b"deflate".into()));

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
        } else {
            rx
        };

        let my_response = Response::new(Headers::from_list(&headers).unwrap(), rx, None);
        my_response.set_status_code(response.status_code()).unwrap();

        Ok(my_response)
    }
}
