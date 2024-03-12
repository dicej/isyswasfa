import asyncio
import hashlib
import isyswasfa_guest

from proxy import exports
from proxy.imports import pipe, handler
from proxy.imports.streams import OutputStream
from proxy.imports.types import (
    Request, Response, Fields, Body, Scheme, Scheme_Http, Scheme_Https, Scheme_Other, Method_Post
)
from isyswasfa_guest.streams import Sink, Stream
from urllib import parse

class Handler(exports.Handler):
    async def handle(self, request: Request) -> Response:
        headers = request.headers().entries()
        method = request.method()
        path = request.path_with_query()
        filtered_headers = Fields.from_list(list(filter(lambda pair: pair[0] == "content-type", headers)))

        if isinstance(method, Method_Post) and path == "/echo":
            # Echo the request body back to the client without buffering.
            return Response(filtered_headers, Request.into_parts(request)[1])
        elif isinstance(method, Method_Post) and path == "/double-echo":
            # Pipe the request body to an outgoing request and stream the response back to the client.
            urls = list(map(lambda pair: str(pair[1], "utf-8"), filter(lambda pair: pair[0] == "url", headers)))
            if len(urls) == 1:
                url = urls[0]
                request = Request(filtered_headers, Request.into_parts(request)[1], None)
                request.set_method(method)

                url_parsed = parse.urlparse(url)

                match url_parsed.scheme:
                    case "http":
                        scheme: Scheme = Scheme_Http()
                    case "https":
                        scheme = Scheme_Https()
                    case _:
                        scheme = Scheme_Other(url_parsed.scheme)
                
                request.set_scheme(scheme)
                request.set_authority(url_parsed.netloc)
                request.set_path_with_query(url_parsed.path)

                return await handler.handle(request)
            else:
                return respond(400)
        else:
            return respond(405)

def respond(status: int) -> Response:
    response = Response(Fields(), Body(pipe.make_pipe()[1], None))
    response.set_status_code(status)
    return response
