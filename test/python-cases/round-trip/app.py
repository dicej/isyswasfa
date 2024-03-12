from round_trip import exports
from round_trip.imports import baz, isyswasfa_io_poll, monotonic_clock

class Baz(exports.Baz):
    async def foo(self, s: str) -> str:
        await isyswasfa_io_poll.block(monotonic_clock.subscribe_duration(10_000_000))

        v = await baz.foo(f"{s} - entered guest")
        
        return f"{v} - exited guest"
