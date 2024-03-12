import asyncio
import hashlib
import isyswasfa_guest

from proxy import exports
from proxy.imports import pipe, handler
from proxy.imports.streams import OutputStream
from proxy.imports.types import Request, Response, Fields, Body, Scheme, Scheme_Http, Scheme_Https, Scheme_Other
from isyswasfa_guest.streams import Sink, Stream
from typing import Tuple
from urllib import parse

class Handler(exports.Handler):
    async def handle(self, request: Request) -> Response:
        """
        Send outgoing GET requests concurrently to the URLs specified as headers and stream the hashes of the
        response bodies as they arrive.
        """
        response_body_tx, response_body_rx = pipe.make_pipe()

        isyswasfa_guest.spawn(hash_all(request, response_body_tx))

        return Response(Fields.from_list([("content-type", b"text/plain")]), Body(response_body_rx, None))

async def hash_all(request: Request, tx: OutputStream):
    with Sink(tx) as sink:
        headers = request.headers().entries()
    
        urls = map(lambda pair: str(pair[1], "utf-8"), filter(lambda pair: pair[0] == "url", headers))

        for result in asyncio.as_completed(map(sha256, urls)):
            url, sha = await result
            await sink.send(bytes(f"{url}: {sha}\n", "utf-8"))
    
async def sha256(url: str) -> Tuple[str, str]:
    """Download the contents of the specified URL, computing the SHA-256
    incrementally as the response body arrives.

    This returns a tuple of the original URL and either the hex-encoded hash or
    an error message.
    """
    url_parsed = parse.urlparse(url)

    match url_parsed.scheme:
        case "http":
            scheme: Scheme = Scheme_Http()
        case "https":
            scheme = Scheme_Https()
        case _:
            scheme = Scheme_Other(url_parsed.scheme)

    request = Request(Fields(), Body(pipe.make_pipe()[1], None), None)
    request.set_scheme(scheme)
    request.set_authority(url_parsed.netloc)
    request.set_path_with_query(url_parsed.path)

    response = await handler.handle(request)
    status = response.status_code()
    if status < 200 or status > 299:
        return url, f"unexpected status: {status}"

    _headers, body = Response.into_parts(response)

    with Stream(body.stream()) as stream:
        hasher = hashlib.sha256()
        while True:
            chunk = await stream.next()
            if chunk is None:
                return url, hasher.hexdigest()
            else:
                hasher.update(chunk)
