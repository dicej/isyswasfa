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
            types::{self, ErrorCode, Headers, IsyswasfaSenderOwnTrailers, Request, Response},
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
    /// Forward the specified request to the imported `wasi:http/handler`, transparently decoding the request body
    /// if it is `deflate`d and then encoding the response body if the client has provided an `accept-encoding:
    /// deflate` header.
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        // First, extract the parts of the request and check for (and remove) headers pertaining to body encodings.
        let method = request.method();
        let scheme = request.scheme();
        let path_with_query = request.path_with_query();
        let authority = request.authority();
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
        let body_rx = request.body().unwrap();

        // Next, spawn a task to pipe (and optionally decode) the original request body and trailers into a new
        // request we'll create below.  This will run concurrently with any code in the imported
        // `wasi:http/handler`.
        let (trailers_tx, trailers_rx) = types::isyswasfa_pipe_own_trailers();
        let (pipe_tx, pipe_rx) = pipe::make_pipe();

        isyswasfa_guest::spawn(async move {
            if content_deflated {
                let mut decoder = DeflateDecoder::new(Vec::new());

                while let Some(chunk) = isyswasfa_guest::read(&body_rx, 64 * 1024).await.unwrap() {
                    decoder.write_all(&chunk).unwrap();
                    isyswasfa_guest::write_all(&pipe_tx, decoder.get_ref())
                        .await
                        .unwrap();
                    decoder.get_mut().clear()
                }

                isyswasfa_guest::write_all(&pipe_tx, &decoder.finish().unwrap())
                    .await
                    .unwrap();
            } else {
                isyswasfa_guest::copy(&body_rx, &pipe_tx).await.unwrap();
            }

            drop(body_rx);

            if let Some(trailers) = Request::finish(request).await.unwrap() {
                IsyswasfaSenderOwnTrailers::send(trailers_tx, trailers);
            }
        });

        // While the above task is running, synthesize a request from the parts collected above and pass it to the
        // imported `wasi:http/handler`.
        let my_request = Request::new(
            Headers::from_list(&headers).unwrap(),
            pipe_rx,
            Some(trailers_rx),
            None,
        );
        my_request.set_method(&method).unwrap();
        my_request.set_scheme(scheme.as_ref()).unwrap();
        my_request
            .set_path_with_query(path_with_query.as_deref())
            .unwrap();
        my_request.set_authority(authority.as_deref()).unwrap();

        let response = handler::handle(my_request).await?;

        // Now that we have the response, extract the parts, adding an extra header if we'll be encoding the body.
        let status_code = response.status_code();
        let mut headers = response.headers().entries();
        if accept_deflated {
            headers.push(("content-encoding".into(), b"deflate".into()));
        }
        let body_rx = response.body().unwrap();

        // Spawn another task; this one is to pipe (and optionally encode) the original response body and trailers
        // into a new response we'll create below.  This will run concurrently with the caller's code (i.e. it
        // won't necessarily complete before we return a value).
        let (trailers_tx, trailers_rx) = types::isyswasfa_pipe_own_trailers();
        let (pipe_tx, pipe_rx) = pipe::make_pipe();

        isyswasfa_guest::spawn(async move {
            if accept_deflated {
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());

                while let Some(chunk) = isyswasfa_guest::read(&body_rx, 64 * 1024).await.unwrap() {
                    encoder.write_all(&chunk).unwrap();
                    isyswasfa_guest::write_all(&pipe_tx, encoder.get_ref())
                        .await
                        .unwrap();
                    encoder.get_mut().clear()
                }

                isyswasfa_guest::write_all(&pipe_tx, &encoder.finish().unwrap())
                    .await
                    .unwrap();
            } else {
                isyswasfa_guest::copy(&body_rx, &pipe_tx).await.unwrap();
            }

            drop(body_rx);

            if let Some(trailers) = Response::finish(response).await.unwrap() {
                IsyswasfaSenderOwnTrailers::send(trailers_tx, trailers);
            }
        });

        // While the above task is running, synthesize a response from the parts collected above and return it.
        let my_response = Response::new(
            Headers::from_list(&headers).unwrap(),
            pipe_rx,
            Some(trailers_rx),
        );
        my_response.set_status_code(status_code).unwrap();

        Ok(my_response)
    }
}
