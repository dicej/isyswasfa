from round_trip import exports
from round_trip.imports import baz
from round_trip.imports.monotonic_clock import subscribe_duration
from round_trip.imports.isyswasfa_io_poll import block

class Baz(exports.Baz):
    async def foo(self, s: str) -> str:
        await block(subscribe_duration(10_000_000))

        v = await baz.foo(f"{s} - entered guest")
        
        return f"{v} - exited guest"
