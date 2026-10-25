#!/usr/bin/env python3
"""Local-only RPC proxy for the custody scenarios; an optional file enables fabricated logs."""

import argparse
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen


class Proxy(BaseHTTPRequestHandler):
    """Forward JSON-RPC batches and individual calls to one local Anvil process."""

    def do_POST(self):
        """Reply from Anvil, optionally adding a validly encoded fictional deposit."""
        body = self.rfile.read(int(self.headers["content-length"]))
        try:
            with urlopen(Request(self.server.target, body, {"content-type": "application/json"}), timeout=5) as out:
                result = json.load(out)
            if self.server.lie and self.server.lie.exists():
                requests = json.loads(body)
                pairs = zip(requests, result) if isinstance(requests, list) else [(requests, result)]
                for request, response in pairs:
                    if request["method"] == "eth_getLogs" and "result" in response:
                        filt = request["params"][0]
                        height = filt["toBlock"]
                        with urlopen(Request(self.server.target, json.dumps({
                            "jsonrpc": "2.0", "id": 1, "method": "eth_getBlockByNumber", "params": [height, False]
                        }).encode(), {"content-type": "application/json"}), timeout=5) as out:
                            block = json.load(out)["result"]
                        address = filt["address"]
                        if isinstance(address, list):
                            address = address[0]
                        # The scenario supplies the payer-bearing deposit ABI's exact topic.
                        response["result"].append({
                            "address": address, "blockNumber": height, "blockHash": block["hash"],
                            "transactionHash": "0x" + "fa" * 32, "transactionIndex": "0x0", "logIndex": "0xff",
                            "removed": False, "topics": [os.environ.get("DEV_DEPOSIT_TOPIC", filt["topics"][0][0]), "0x" + "0" * 64,
                                                          "0x" + "0" * 24 + "ab" * 20, "0x" + "0" * 24 + "cd" * 20],
                            "data": "0x" + f"{50_000_000:064x}",
                        })
            payload = json.dumps(result).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except (OSError, ValueError, HTTPError):
            self.send_error(502)

    def log_message(self, *_):
        """Keep the scenario output compact."""


def main():
    """Serve a loopback proxy until its process is stopped by the scenario."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--lie-file", type=Path)
    args = parser.parse_args()
    if not args.target.startswith("http://127.0.0.1:"):
        parser.error("target must be local Anvil")
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Proxy)
    server.target, server.lie = args.target, args.lie_file
    server.serve_forever()


if __name__ == "__main__":
    main()
