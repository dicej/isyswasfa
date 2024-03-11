from proxy import exports
from proxy.imports.types import Request, Response

class Handler(exports.Handler):
    async def handle(self, request: Request) -> Response:
        """Return a response which echoes the request headers, body, and trailers."""
        headers, body = Request.into_parts(request)
        return Response(headers, body)
