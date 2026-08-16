"""A stand-in for OpenAI and Anthropic that records what actually reached it.

The demo's whole point is this file's contents: if the gateway works, nothing
here is a real name, IBAN or diagnosis. It answers using the placeholders it
was handed, so restoration on the way back is visible in the same run.
"""

import json
import re
from http.server import BaseHTTPRequestHandler, HTTPServer

RECEIVED = "/received/received.json"
PLACEHOLDER = re.compile(r"\[[A-Z_]{1,40}_\d+\]")


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(raw)
        with open(RECEIVED, "w") as f:
            json.dump({"path": self.path, "body": body}, f, indent=2)

        seen = PLACEHOLDER.findall(json.dumps(body, ensure_ascii=False))
        listed = ", ".join(dict.fromkeys(seen)) or "nothing identifiable"
        reply = f"Eingang bestätigt. Betroffen sind: {listed}."

        if self.path.startswith("/v1/messages"):
            payload = {
                "id": "msg_demo",
                "type": "message",
                "role": "assistant",
                "model": "claude-demo",
                "content": [{"type": "text", "text": reply}],
                "stop_reason": "end_turn",
            }
        else:
            payload = {
                "id": "chatcmpl-demo",
                "object": "chat.completion",
                "model": "gpt-demo",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": reply},
                        "finish_reason": "stop",
                    }
                ],
            }

        out = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def do_GET(self) -> None:
        self.send_response(200)
        self.send_header("content-length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args: object) -> None:
        pass


HTTPServer(("0.0.0.0", 9099), Handler).serve_forever()
