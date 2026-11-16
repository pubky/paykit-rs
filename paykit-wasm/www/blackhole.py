#!/usr/bin/env python3
"""THROWAWAY (#411): TCP blackhole standing in for a hung homeserver.

Usage: python3 -I blackhole.py <port>
- <port>: accepts TCP connections and never sends a byte. Records when the
  peer closes the connection (for example when the browser aborts a fetch).
  Connections are dropped after HOLD_SECONDS.
- <port + 1>: tiny HTTP status endpoint, GET / returns
  {"accepted", "closedByPeer", "open"} with Access-Control-Allow-Origin: *.
Binds 127.0.0.1 only.
"""
import asyncio
import json
import sys

HOLD_SECONDS = 300
stats = {"accepted": 0, "closedByPeer": 0, "open": 0}


async def hole(reader, writer):
    stats["accepted"] += 1
    stats["open"] += 1
    try:
        async def drain():
            while await reader.read(65536):
                pass
        await asyncio.wait_for(drain(), HOLD_SECONDS)
        stats["closedByPeer"] += 1
    except (asyncio.TimeoutError, ConnectionError):
        pass
    finally:
        stats["open"] -= 1
        writer.close()


async def status(reader, writer):
    try:
        await reader.readuntil(b"\r\n\r\n")
        body = json.dumps(stats).encode()
        writer.write(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            b"Access-Control-Allow-Origin: *\r\nConnection: close\r\n"
            + f"Content-Length: {len(body)}\r\n\r\n".encode()
            + body
        )
        await writer.drain()
    finally:
        writer.close()


async def main(port):
    a = await asyncio.start_server(hole, "127.0.0.1", port)
    b = await asyncio.start_server(status, "127.0.0.1", port + 1)
    print(f"blackhole on 127.0.0.1:{port}, status on 127.0.0.1:{port + 1}", flush=True)
    async with a, b:
        await asyncio.gather(a.serve_forever(), b.serve_forever())


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1])))
