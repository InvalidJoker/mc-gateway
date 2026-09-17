# A customer's Minecraft server, as far as the network can tell: waits for the
# handshake and the status request (sent separately by real clients), answers
# with its own MOTD, echoes the latency ping, and logs who it thinks connected.
import json, socket, sys, threading
NAME = sys.argv[1]
def varint(v):
    out = b""
    while True:
        b = v & 0x7F; v >>= 7
        out += bytes([b | 0x80]) if v else bytes([b])
        if not v: return out
def read_varint(buf, pos):
    n = 0
    for i in range(5):
        if pos >= len(buf): return None, pos
        b = buf[pos]; pos += 1; n |= (b & 0x7F) << (7 * i)
        if not b & 0x80: return n, pos
    raise ValueError
def frame(buf):
    length, pos = read_varint(buf, 0)
    if length is None or len(buf) < pos + length: return None
    return buf[pos:pos + length], pos + length
def handle(c, peer):
    buf = b""
    try:
        while True:
            f = frame(buf)
            if f: break
            d = c.recv(4096)
            if not d: return
            buf += d
        body, used = f
        next_state = body[-1]
        if next_state == 1:
            rest = buf[used:]
            while frame(rest) is None:
                d = c.recv(4096)
                if not d: return
                rest += d
            doc = {"version": {"name": "Paper 1.21.1", "protocol": 767},
                   "players": {"max": 20, "online": 3},
                   "description": {"extra": [{"text": NAME, "color": "gold"}, {"text": "\n"},
                                             {"text": "customer line two", "color": "gray"}], "text": ""}}
            data = json.dumps(doc).encode()
            payload = varint(0) + varint(len(data)) + data
            c.sendall(varint(len(payload)) + payload)
            print(f"{NAME} STATUS from {peer[0]}:{peer[1]}", flush=True)
            while True:
                d = c.recv(4096)
                if not d: return
                c.sendall(d)
        else:
            print(f"{NAME} LOGIN from {peer[0]}:{peer[1]} bytes={len(buf)}", flush=True)
            c.sendall(f"LOGIN-SEEN-FROM {peer[0]}".encode())
    finally:
        c.close()
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 25565)); s.listen(64)
while True:
    c, p = s.accept(); threading.Thread(target=handle, args=(c, p), daemon=True).start()
