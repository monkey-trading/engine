"""Client for scripts/dev-quotes-cases.sh: a signed stream with a quote subscription, and market
orders sent at the streamed prices.

usage: dev-quotes-client.py PORT SIGNER_BIN CHAIN_ID VAULT ACCOUNT CHALLENGE FRAMES_OUT

Every frame the stream sends is saved to FRAMES_OUT, one a line, for `engine verify-frames`. The
trader's key is SIGNER_KEY in the environment, as `sign_request` reads it. Exits non-zero, saying
why, when a check fails. The node's certificate is self-signed, so the CA check is skipped.
"""
import base64
import http.client
import json
import os
import queue
import socket
import ssl
import struct
import subprocess
import sys
import threading
import time
from decimal import Decimal

PORT, SIGNER, CHAIN_ID, VAULT, ACCOUNT, CHALLENGE, FRAMES_OUT = sys.argv[1:]
INTERVAL_MS = 100
QTY = "0.01"
context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE


def fail(message):
    sys.exit(f"FAIL: {message}")


def post_order(side, quoted, bound):
    http_ = http.client.HTTPSConnection("127.0.0.1", int(PORT), context=context, timeout=10)
    http_.request("GET", f"/v1/accounts/{ACCOUNT}/nonce")
    nonce = json.load(http_.getresponse())["next_nonce"]
    body = {"account": ACCOUNT, "client_id": 46, "market": 1, "side": side, "qty": QTY,
            "kind": {"kind": "market", "quoted": quoted, "max_slippage_bps": bound}}
    expires = str(int(time.time() * 1000) + 60000)
    signed = subprocess.run([SIGNER, CHAIN_ID, VAULT, str(nonce), expires, "order", json.dumps(body)],
                            capture_output=True, text=True, check=True).stdout
    http_.request("POST", "/v1/orders", signed, {"content-type": "application/json"})
    response = http_.getresponse()
    events = json.load(response)
    if response.status != 200:
        fail(f"the order was refused with {response.status}: {events}")
    return events


class Stream:
    """The signed stream, read by a thread that stamps each frame's arrival."""

    def __init__(self):
        sock = context.wrap_socket(socket.create_connection(("127.0.0.1", int(PORT)), timeout=30))
        key = base64.b64encode(os.urandom(16)).decode()
        self.path = f"/v1/stream?signed=1&challenge={CHALLENGE}"
        sock.sendall((f"GET {self.path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nUpgrade: websocket\r\n"
                      f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        self.sock, self.reader = sock, sock.makefile("rb")
        if b" 101 " not in self.reader.readline():
            fail("the stream upgrade was refused")
        while self.reader.readline() not in (b"\r\n", b""):
            pass
        self.frames, self.quotes, self.opened = [], queue.Queue(), time.monotonic()
        threading.Thread(target=self.read, daemon=True).start()

    def send(self, text):
        data, mask = text.encode(), os.urandom(4)
        header = bytes([0x81]) + (bytes([0x80 | len(data)]) if len(data) < 126 else bytes([0x80 | 126]) + struct.pack(">H", len(data)))
        self.sock.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def read_exactly(self, n):
        data = self.reader.read(n)
        if len(data) != n:
            raise EOFError
        return data

    def read(self):
        message = b""
        try:
            while True:
                first, second = self.read_exactly(2)
                length = second & 0x7F
                if length == 126:
                    length = struct.unpack(">H", self.read_exactly(2))[0]
                elif length == 127:
                    length = struct.unpack(">Q", self.read_exactly(8))[0]
                payload, opcode = self.read_exactly(length), first & 0x0F
                if opcode == 8:
                    return
                if opcode in (0, 1):
                    message += payload
                    if first & 0x80:
                        self.take(message.decode())
                        message = b""
        except (EOFError, OSError):
            return

    def take(self, text):
        self.frames.append(text)
        # The time the node signed the frame at, not when this thread read it: quotes are sent exactly
        # an interval apart while the index moves, so a delay in reading would show as a closer pair.
        arrived = 0.0
        try:
            frame = json.loads(text)
            arrived = frame["time"] / 1000
            msg = json.loads(frame["msg"])
        except (ValueError, KeyError) as error:
            msg = {"type": "quote_error", "error": f"an unreadable frame: {error}"}
        if msg["type"] in ("quote", "quote_error"):
            self.quotes.put((arrived, msg))

    def next_quote(self, wait=10):
        try:
            arrived, msg = self.quotes.get(timeout=wait)
        except queue.Empty:
            fail(f"no quote within {wait}s")
        if msg["type"] == "quote_error":
            fail(f"the node refused the subscription: {msg}")
        return arrived, msg


def ask_for(quote, qty):
    """The price of the first ask step that covers `qty`."""
    for step in quote["ask"]:
        if Decimal(step["qty"]) >= Decimal(qty):
            if "declined" in step:
                fail(f"the step covering {qty} is declined: {step}")
            return step["price"]
    fail(f"no ask step covers {qty}: {quote}")


def fills(events):
    return [event for event in events if event["type"] == "fill"]


def slippage(events):
    return any(event["type"] == "order_rejected" and event["reason"]["kind"] == "slippage" for event in events)


stream = Stream()
stream.send(json.dumps({"op": "quotes", "account": ACCOUNT, "markets": [1]}))
arrived, quote = stream.next_quote()
if quote["market"] != 1 or not quote["open"] or "unavailable" in quote:
    fail(f"market 1 is not quotable: {quote}")
quote_times = [arrived]

# An order at a streamed ask with a 5 bps bound fills at or below the bound. The mock feed walks
# the index every 200 ms, so a fresh quote is used for each attempt.
for attempt in range(5):
    quoted = ask_for(quote, QTY)
    events = post_order("buy", quoted, 5)
    if fills(events):
        price, limit = Decimal(fills(events)[0]["price"]), Decimal(quoted) * Decimal("1.0005")
        if price > limit:
            fail(f"the order filled at {price}, above its {limit} bound")
        print(f"filled at {price}, quoted {quoted}, bound {limit}")
        break
    if not slippage(events):
        fail(f"the order at the streamed ask neither filled nor was declined for slippage: {events}")
    arrived, quote = stream.next_quote()
    quote_times.append(arrived)
else:
    fail("five orders at fresh quotes all moved past 5 bps")

# Once the index moves a quote at the old price is stale: a new quote arrives, and a buy at the old
# ask with no slippage allowed is declined.
stale = ask_for(quote, QTY)
deadline = time.monotonic() + 40
while time.monotonic() < deadline:
    arrived, quote = stream.next_quote()
    quote_times.append(arrived)
    live = ask_for(quote, QTY)
    if Decimal(live) > Decimal(stale):
        events = post_order("buy", stale, 0)
        if slippage(events):
            print(f"a buy at the old ask {stale} was declined for slippage once the ask was {live}")
            break
        stale = ask_for(stream.next_quote()[1], QTY)
    else:
        stale = live
else:
    fail("the index never moved the ask up")

gaps = [(b - a) * 1000 for a, b in zip(quote_times, quote_times[1:])]
if any(gap < INTERVAL_MS * 0.9 for gap in gaps):
    fail(f"quotes came closer than {INTERVAL_MS} ms: {[round(g) for g in gaps]}")
print(f"{len(quote_times)} quotes, the closest {round(min(gaps))} ms apart")
with open(FRAMES_OUT, "w") as out:
    out.write("\n".join(stream.frames) + "\n")
print("OK")
