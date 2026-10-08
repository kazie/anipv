#!/usr/bin/env python3
"""Minimal stand-in for mpv's JSON IPC, used by the integration tests.

Plays each file argument "instantly": the first file to the end (eof), the
second only halfway, then exits. Answers the `get_property path` request
sent at each start-file and, like mpv, reports the `path` property only
when it changes. Honors --input-ipc-server=PATH.
"""
import json, os, socket, sys

sock_path = None
files = []
for a in sys.argv[1:]:
    if a.startswith("--input-ipc-server="):
        sock_path = a.split("=", 1)[1]
    elif not a.startswith("--"):
        files.append(a)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(1)
conn, _ = srv.accept()
f = conn.makefile("rw")
for _ in range(3):  # observe_property commands
    f.readline()

def send(obj):
    f.write(json.dumps(obj) + "\n")
    f.flush()

last_path = None
for i, path in enumerate(files):
    dur = 1440.0
    send({"event": "start-file", "playlist_entry_id": i + 1})
    req = json.loads(f.readline())  # get_property path
    send({"request_id": req["request_id"], "error": "success", "data": path})
    if path != last_path:  # like mpv, an unchanged value is not reported again
        send({"event": "property-change", "id": 1, "name": "path", "data": path})
    last_path = path
    send({"event": "property-change", "id": 3, "name": "duration", "data": dur})
    pos = dur if i == 0 else dur / 2
    send({"event": "property-change", "id": 2, "name": "time-pos", "data": pos})
    send({"event": "end-file", "reason": "eof" if i == 0 else "quit", "playlist_entry_id": i + 1})
    if i > 0:
        break
conn.close()
os.unlink(sock_path)
