#!/usr/bin/env python3
"""
Mock typst worker that speaks the NDJSON protocol.

Behavior is controlled by the source field in each request:
  - "__timeout__"  → sleep forever (simulates hang)
  - "__crash__"    → exit immediately (simulates crash)
  - "__error__"    → return ok:false with a compile error
  - anything else  → return ok:true with a tiny 1x1 PNG (base64)
"""

import json
import sys
import time
import base64

# Minimal valid 1x1 white PNG (67 bytes)
TINY_PNG = (
    b"\x89PNG\r\n\x1a\n"
    b"\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02"
    b"\x00\x00\x00\x90wS\xde\x00\x00\x00\x0cIDATx"
    b"\x9cc\xf8\x0f\x00\x00\x01\x01\x00\x05\x18\xd8N"
    b"\x00\x00\x00\x00IEND\xaeB`\x82"
)
TINY_PNG_B64 = base64.b64encode(TINY_PNG).decode()


def main():
    ready = {
        "ready": True,
        "protocol_version": 1,
        "version": "mock-0.1.0",
        "fonts_loaded": 0,
    }
    sys.stdout.write(json.dumps(ready) + "\n")
    sys.stdout.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        req = json.loads(line)
        req_id = req.get("id")
        source = req.get("source", "")

        if source == "__timeout__":
            time.sleep(3600)
            continue

        if source == "__crash__":
            sys.exit(1)

        if source == "__error__":
            resp = {
                "id": req_id,
                "ok": False,
                "errors": [
                    {
                        "kind": "compile",
                        "message": "mock compile error",
                        "span": {"line": 1, "column": 1},
                        "hints": [],
                    }
                ],
            }
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
            continue

        resp = {
            "id": req_id,
            "ok": True,
            "format": "png",
            "data": TINY_PNG_B64,
            "pages": 1,
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
