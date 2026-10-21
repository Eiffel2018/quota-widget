#!/usr/bin/env python3
"""Synthetic protocol fixture. Never reads auth or reaches the network.

This test-only script is launched through Python by cfg(test) code. Production
executable validation intentionally rejects scripts and script launchers.
"""
import json
import os
import sys
import time

mode, transcript, pidfile = sys.argv[1:4]
with open(pidfile, "w", encoding="utf-8") as file:
    file.write(str(os.getpid()))
with open(pidfile + ".cwd", "w", encoding="utf-8") as file:
    file.write(os.getcwd())


def send(value):
    print(json.dumps(value), flush=True)


reads = 0
initialized = False
for line in sys.stdin:
    request = json.loads(line)
    with open(transcript, "a", encoding="utf-8") as file:
        file.write(json.dumps(request) + "\n")
    method = request["method"]
    if method == "initialize":
        if mode == "timeout":
            time.sleep(60)
        if mode == "malformed":
            print("secret-token-must-never-escape", flush=True)
            time.sleep(60)
        if mode == "oversized":
            print("x" * (1024 * 1024 + 1), flush=True)
            time.sleep(60)
        send({"id": request["id"], "result": {"userAgent": "synthetic"}})
    elif method == "initialized":
        initialized = True
    elif method == "account/rateLimits/read":
        assert initialized
        reads += 1
        if mode == "read_timeout":
            time.sleep(60)
        if mode == "error":
            send({"id": request["id"], "error": {"message": "secret@example.invalid bearer secret"}})
            continue
        if mode == "server_request":
            send({"id": 99, "method": "account/chatgptAuthTokens/refresh", "params": {"previousAccountId": "synthetic"}})
            continue
        if mode in ("coalesce", "switch") and reads == 1:
            time.sleep(0.15)
        if mode == "switch" and reads == 1:
            send({"method": "account/updated", "params": {"authMode": "chatgpt"}})
        send({"id": request["id"], "result": {"rateLimits": {
            "limitId": "codex", "primary": {"usedPercent": reads, "windowDurationMins": 300,
            "resetsAt": 1800000000}, "secondary": None, "credits": {"balance": "12.3456"}}}})
        if mode == "sparse" and reads == 1:
            send({"method": "account/rateLimits/updated", "params": {"rateLimits": {"primary": {"usedPercent": 99}}}})
    else:
        raise AssertionError("A forbidden method was sent")
