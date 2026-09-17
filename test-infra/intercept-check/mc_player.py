import json, socket, struct, sys, time
def varint(v):
    out = b""
    while True:
        b = v & 0x7F; v >>= 7
        out += bytes([b | 0x80]) if v else bytes([b])
        if not v: return out
def rv(s):
    n = 0
    for i in range(5):
        b = s.recv(1)
        if not b: raise EOFError("closed")
        n |= (b[0] & 0x7F) << (7 * i)
        if not b[0] & 0x80: return n
def pkt(i, b): p = varint(i) + b; return varint(len(p)) + p
def st(x): x = x.encode(); return varint(len(x)) + x
host, port, mode = sys.argv[1], int(sys.argv[2]), sys.argv[3]
try:
    c = socket.create_connection((host, port), timeout=5)
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    hs = pkt(0, varint(767) + st(host) + struct.pack(">H", port) + varint(1 if mode == "status" else 2))
    if mode == "status":
        c.sendall(hs); time.sleep(0.1); c.sendall(pkt(0, b""))
        rv(c); rv(c); n = rv(c); buf = b""
        while len(buf) < n: buf += c.recv(n - len(buf))
        d = json.loads(buf)
        print(f"port {port}: players {d['players']['online']}/{d['players']['max']} | " + json.dumps(d["description"], ensure_ascii=False)[:160])
    else:
        c.sendall(hs + pkt(0, st("Steve") + b"\0" * 16))
        print(f"port {port}: " + c.recv(100).decode())
except Exception as e:
    print(f"port {port}: {type(e).__name__}: {e}")
