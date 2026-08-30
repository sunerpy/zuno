#!/usr/bin/env python3
import json
import sys

PROTOCOL = "zuno.plugin/1"


def response(request_id, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": request_id}
    if error is None:
        message["result"] = result
    else:
        message["error"] = {"code": -32000, "message": error}
    print(json.dumps(message, ensure_ascii=False), flush=True)


for raw in sys.stdin:
    try:
        request = json.loads(raw)
        request_id = request["id"]
        method = request["method"]
        params = request.get("params", {})

        if method == "initialize":
            if params.get("protocolVersion") != PROTOCOL:
                response(request_id, error="unsupported protocol")
                continue
            response(request_id, {"protocolVersion": PROTOCOL})
            continue

        if method == "tools/call":
            if params.get("tool") != "review_outline":
                response(request_id, error="unknown tool")
                continue
            subject = params.get("arguments", {}).get("subject", "").strip()
            if not subject:
                response(request_id, error="subject is required")
                continue
            output = "\n".join(
                [
                    f"# Review outline: {subject}",
                    "",
                    "- Identify trust and authorization boundaries.",
                    "- Trace durable state and retry behavior.",
                    "- Verify cleanup and rollback paths.",
                    "- Record the exact validation evidence.",
                ]
            )
            response(
                request_id,
                {
                    "title": "Review outline",
                    "output": output,
                    "metadata": {"example": True},
                },
            )
            continue

        if method == "shutdown":
            response(request_id, {})
            break

        response(request_id, error=f"unknown method: {method}")
    except Exception as error:
        response(None, error=f"invalid request: {error}")
