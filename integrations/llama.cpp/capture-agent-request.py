#!/usr/bin/env python3
"""Capture what a coding assistant really sends to its model.

Stands in for an OpenAI-compatible server on 127.0.0.1:PORT (default 7790), saves every
request body to OUT_DIR/req-N.json and answers "ok" (streamed or not), so the assistant
finishes its turn. Point the assistant's provider at http://127.0.0.1:PORT/v1, give it one
task, stop this with Ctrl+C. agent-bench.sh replays the captures against a real model.

    python3 capture-agent-request.py OUT_DIR [PORT]

Example for opencode, in a scratch project's opencode.json:
    {"provider": {"local": {"npm": "@ai-sdk/openai-compatible",
       "options": {"baseURL": "http://127.0.0.1:7790/v1"},
       "models": {"m": {"tool_call": true}}}}, "model": "local/m", "small_model": "local/m"}
then `opencode run "List the files in src"`.

Captures hold the assistant's whole system prompt, which can name your folders and the
skills or tools you have installed. Keep them out of anything you publish.
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = sys.argv[1] if len(sys.argv) > 1 else sys.exit(__doc__)
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 7790
os.makedirs(OUT, exist_ok=True)
count = [len([f for f in os.listdir(OUT) if f.startswith("req-")])]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, body, kind="application/json"):
        self.send_response(200)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply(json.dumps({"object": "list", "data": [{"id": "captured", "object": "model"}]}).encode())

    def do_POST(self):
        data = self.rfile.read(int(self.headers.get("content-length", 0)))
        count[0] += 1
        path = os.path.join(OUT, f"req-{count[0]}.json")
        with open(path, "wb") as f:
            f.write(data)
        try:
            req = json.loads(data)
        except ValueError:
            req = {}
        print(f"saved {path} ({len(data):,} bytes, {len(req.get('messages', []))} messages, {len(req.get('tools', []))} tools)", flush=True)
        now = int(time.time())
        if req.get("stream"):
            out = b""
            for delta, stop in ((({"role": "assistant", "content": "ok"}), None), ({}, "stop")):
                ev = {"id": "c", "object": "chat.completion.chunk", "created": now, "model": "captured",
                      "choices": [{"index": 0, "delta": delta, "finish_reason": stop}]}
                out += b"data: " + json.dumps(ev).encode() + b"\n\n"
            self.reply(out + b"data: [DONE]\n\n", "text/event-stream")
        else:
            self.reply(json.dumps({"id": "c", "object": "chat.completion", "created": now, "model": "captured",
                                   "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                                   "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}).encode())


print(f"Capturing on http://127.0.0.1:{PORT}/v1 into {OUT}. Ctrl+C stops.", flush=True)
try:
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
except KeyboardInterrupt:
    pass
