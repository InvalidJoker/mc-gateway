import socket, struct, sys
def varint(v):
    out = b""
    while True:
        b = v & 0x7F; v >>= 7
        out += bytes([b | 0x80]) if v else bytes([b])
        if not v: return out
def packet(pid, body):
    p = varint(pid) + body; return varint(len(p)) + p
def s(x): x = x.encode(); return varint(len(x)) + x
host, port = sys.argv[1], int(sys.argv[2])
c = socket.create_connection((host, port), timeout=5)
print(f"CLIENT local address {c.getsockname()[0]}:{c.getsockname()[1]}", flush=True)
c.sendall(packet(0, varint(767) + s("echo.example") + struct.pack(">H", 25565) + varint(2)) + packet(0, s("Probe") + b"\0"*16))
print("CLIENT received:", c.recv(200).decode().strip() or "<nothing>", flush=True)
