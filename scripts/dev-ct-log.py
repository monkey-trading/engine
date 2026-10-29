#!/usr/bin/env python3
"""A loopback stand-in for an RFC 6962 Certificate Transparency log, for dev-ct-watch.sh.

  dev-ct-log.py leaf CERT.pem KIND TIMESTAMP_MS OUT   write one MerkleTreeLeaf (KIND: x509)
  dev-ct-log.py serve PORT DIR KEY.pem                serve the leaf-*.bin files in DIR, sorted

The served tree head is signed with KEY (an ECDSA P-256 private key, by the openssl command line)
over the tree the files make, and re-signed on every request, so a leaf file added while it runs
joins the tree. get-entries answers at most two entries a call, so the watcher's batching is used.
Only the standard library and openssl; it never leaves loopback.
"""
import hashlib
import json
import struct
import subprocess
import sys
import time
from base64 import b64encode
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

MAX_ENTRIES_PER_CALL = 2


def der_of(cert_pem):
    return subprocess.run(
        ["openssl", "x509", "-in", cert_pem, "-outform", "DER"], check=True, capture_output=True
    ).stdout


def merkle_leaf(cert_pem, kind, timestamp_ms):
    if kind != "x509":
        sys.exit("only x509 entries are built here")
    der = der_of(cert_pem)
    # version 0, leaf type 0 (timestamped entry), timestamp, entry type 0 (x509), 24-bit length.
    return (
        struct.pack(">BBQH", 0, 0, timestamp_ms, 0)
        + len(der).to_bytes(3, "big")
        + der
        + struct.pack(">H", 0)
    )


def tree_hash(leaves):
    """RFC 6962 section 2.1."""
    if not leaves:
        return hashlib.sha256(b"").digest()
    if len(leaves) == 1:
        return hashlib.sha256(b"\x00" + leaves[0]).digest()
    split = 1
    while split * 2 < len(leaves):
        split *= 2
    return hashlib.sha256(b"\x01" + tree_hash(leaves[:split]) + tree_hash(leaves[split:])).digest()


def tree_head(leaves, key):
    timestamp = int(time.time() * 1000)
    root = tree_hash(leaves)
    message = struct.pack(">BBQQ", 0, 1, timestamp, len(leaves)) + root
    signature = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", key], input=message, check=True, capture_output=True
    ).stdout
    # DigitallySigned: SHA-256, ECDSA, 16-bit length, the DER signature.
    signed = bytes([4, 3]) + struct.pack(">H", len(signature)) + signature
    return {
        "tree_size": len(leaves),
        "timestamp": timestamp,
        "sha256_root_hash": b64encode(root).decode(),
        "tree_head_signature": b64encode(signed).decode(),
    }


def serve(port, directory, key):
    def leaves():
        return [path.read_bytes() for path in sorted(Path(directory).glob("leaf-*.bin"))]

    class Log(BaseHTTPRequestHandler):
        def do_GET(self):
            url = urlparse(self.path)
            all_leaves = leaves()
            if url.path == "/ct/v1/get-sth":
                body = tree_head(all_leaves, key)
            elif url.path == "/ct/v1/get-entries":
                query = parse_qs(url.query)
                start, end = int(query["start"][0]), int(query["end"][0])
                end = min(end, start + MAX_ENTRIES_PER_CALL - 1, len(all_leaves) - 1)
                if start > end:
                    self.send_error(400, "start is past the end of the tree")
                    return
                body = {
                    "entries": [
                        {"leaf_input": b64encode(all_leaves[i]).decode(), "extra_data": ""}
                        for i in range(start, end + 1)
                    ]
                }
            else:
                self.send_error(404)
                return
            payload = json.dumps(body).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *args):
            pass

    HTTPServer(("127.0.0.1", port), Log).serve_forever()


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "leaf":
        Path(sys.argv[5]).write_bytes(merkle_leaf(sys.argv[2], sys.argv[3], int(sys.argv[4])))
    elif len(sys.argv) == 5 and sys.argv[1] == "serve":
        serve(int(sys.argv[2]), sys.argv[3], sys.argv[4])
    else:
        sys.exit(__doc__)
