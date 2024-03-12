mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "proxy",
        isyswasfa: "-hash-all",
        exports: {
            "wasi:http/handler": super::Component
        }
    });
}

use {
    anyhow::{anyhow, bail, Result},
    async_trait::async_trait,
    bindings::{
        exports::wasi::http::handler::Guest,
        isyswasfa::io::pipe,
        wasi::http::{
            handler,
            types::{Body, ErrorCode, Fields, Request, Response, Scheme},
        },
    },
    futures::{
        sink::SinkExt,
        stream::{self, StreamExt, TryStreamExt},
    },
    url::Url,
};

const MAX_CONCURRENCY: usize = 16;

struct Component;

#[async_trait(?Send)]
impl Guest for Component {
    /// Send outgoing GET requests concurrently to the URLs specified as headers and stream the hashes of the
    /// response bodies as they arrive.
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let (response_body_tx, response_body_rx) = pipe::make_pipe();
        let mut response_body_tx = isyswasfa_guest::sink(response_body_tx);

        isyswasfa_guest::spawn(async move {
            let headers = request.headers().entries();

            let urls = headers.iter().filter_map(|(k, v)| {
                (k == "url")
                    .then_some(v)
                    .and_then(|v| std::str::from_utf8(v).ok())
                    .and_then(|v| Url::parse(v).ok())
            });

            let results = urls.map(|url| async move {
                let result = hash(&url).await;
                (url, result)
            });

            let mut results = stream::iter(results).buffer_unordered(MAX_CONCURRENCY);

            while let Some((url, result)) = results.next().await {
                let payload = match result {
                    Ok(hash) => format!("{url}: {hash}\n"),
                    Err(e) => format!("{url}: {e:?}\n"),
                }
                .into_bytes();

                if let Err(e) = response_body_tx.send(payload).await {
                    eprintln!("Error sending payload: {e}");
                }
            }
        });

        Ok(Response::new(
            Fields::from_list(&[("content-type".to_string(), b"text/plain".to_vec())]).unwrap(),
            Body::new(response_body_rx, None),
        ))
    }
}

async fn hash(url: &Url) -> Result<String> {
    let request = Request::new(Fields::new(), Body::new(pipe::make_pipe().1, None), None);

    request
        .set_path_with_query(Some(url.path()))
        .map_err(|()| anyhow!("failed to set path_with_query"))?;

    request
        .set_scheme(Some(&match url.scheme() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            scheme => Scheme::Other(scheme.into()),
        }))
        .map_err(|()| anyhow!("failed to set scheme"))?;

    request
        .set_authority(Some(&format!(
            "{}{}",
            url.host_str().unwrap_or(""),
            if let Some(port) = url.port() {
                format!(":{port}")
            } else {
                String::new()
            }
        )))
        .map_err(|()| anyhow!("failed to set authority"))?;

    let response = handler::handle(request).await?;

    let status = response.status_code();

    if !(200..300).contains(&status) {
        bail!("unexpected status: {status}");
    }

    let (_, body) = Response::into_parts(response);
    let mut stream = isyswasfa_guest::stream(body.stream().unwrap());

    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = stream.try_next().await? {
        hasher.update(&chunk);
    }

    Ok(hex::encode(hasher.finalize()))
}
