"""End-to-end smoke test against the real mc-gateway binary."""
import json, socket, struct, subprocess, threading, time, sys, urllib.request

GW = ("127.0.0.1", 25599)
BACKEND_PORT = 25601

def varint(v):
    out = b""
    v &= 0xFFFFFFFF
    while True:
        b = v & 0x7F
        v >>= 7
        if v:
            out += bytes([b | 0x80])
        else:
            return out + bytes([b])

def read_varint(sock):
    n = 0
    for i in range(5):
        b = sock.recv(1)
        if not b:
            raise EOFError
        n |= (b[0] & 0x7F) << (7 * i)
        if not b[0] & 0x80:
            return n
    raise ValueError("varint too long")

def packet(pid, body):
    payload = varint(pid) + body
    return varint(len(payload)) + payload

def mcstring(s):
    b = s.encode()
    return varint(len(b)) + b

def handshake(host, state, protocol=767):
    return packet(0x00, varint(protocol) + mcstring(host) + struct.pack(">H", 25565) + varint(state))

received = []

def backend():
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", BACKEND_PORT))
    srv.listen(8)
    while True:
        conn, _ = srv.accept()
        data = conn.recv(4096)
        if data:
            received.append(data)
        threading.Thread(target=lambda c=conn: echo(c), daemon=True).start()

def echo(conn):
    try:
        while True:
            data = conn.recv(4096)
            if not data:
                return
            conn.sendall(data)
    except OSError:
        pass

threading.Thread(target=backend, daemon=True).start()
time.sleep(0.3)

gateway = subprocess.Popen(sys.argv[1:], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
time.sleep(1.5)

failures = []
def check(name, ok, detail=""):
    print(("  PASS  " if ok else "  FAIL  ") + name + (f"  [{detail}]" if detail else ""))
    if not ok:
        failures.append(name)

try:
    # --- status ping -------------------------------------------------------
    s = socket.create_connection(GW, timeout=5)
    s.sendall(handshake("survival.example.net", 1) + packet(0x00, b""))
    read_varint(s); read_varint(s)
    length = read_varint(s)
    buf = b""
    while len(buf) < length:
        buf += s.recv(length - len(buf))
    status = json.loads(buf.decode())
    s.close()

    print("\nMOTD as a client sees it:")
    print("  " + status["description"]["text"].replace("\n", "\n  "))
    check("status answered by the gateway", status["version"]["name"] == "MyNetwork")
    check("protocol echoed back", status["version"]["protocol"] == 767)
    check("max players", status["players"]["max"] == 1000)
    check("sample line present", status["players"]["sample"][0]["name"].endswith("Welcome!"))

    # --- old client --------------------------------------------------------
    s = socket.create_connection(GW, timeout=5)
    s.sendall(bytes([0xFE, 0x01]))
    legacy = s.recv(1024)
    s.close()
    fields = legacy[3:].decode("utf-16-be").split("\x00")
    check("1.6 legacy ping answered", legacy[0] == 0xFF and fields[2] == "MyNetwork", fields[3][:30])

    # --- login with PROXY protocol ----------------------------------------
    before = len(received)
    s = socket.create_connection(GW, timeout=5)
    s.sendall(handshake("survival.example.net", 2) + packet(0x00, mcstring("Notch") + b"\0" * 16))
    deadline = time.time() + 5
    while len(received) <= before and time.time() < deadline:
        time.sleep(0.05)
    head = received[-1] if len(received) > before else b""
    check("session reached the backend", bool(head))
    check("PROXY v2 signature present", head.startswith(b"\r\n\r\n\x00\r\nQUIT\n"), head[:12].hex())
    check("PROXY command is PROXY/TCP4", head[12] == 0x21 and head[13] == 0x11)
    src = ".".join(str(b) for b in head[16:20])
    check("real client IP forwarded", src == "127.0.0.1", src)
    check("handshake follows the header", head[28:29] == varint(len(varint(0) + varint(767) + mcstring("survival.example.net") + struct.pack(">H", 25565) + varint(2))))

    s.sendall(b"post-login")
    check("pipe carries traffic", s.recv(64) == b"post-login")
    s.close()

    # --- unknown host ------------------------------------------------------
    s = socket.create_connection(GW, timeout=5)
    s.sendall(handshake("modded.example.net", 2) + packet(0x00, mcstring("Bob") + b"\0" * 16))
    read_varint(s); pid = read_varint(s)
    length = read_varint(s)
    reason = json.loads(s.recv(length).decode())
    s.close()
    check("offline backend produces a kick", pid == 0x00 and "offline" in reason["text"], reason["text"])

    # --- metrics -----------------------------------------------------------
    metrics = urllib.request.urlopen("http://127.0.0.1:9199/metrics", timeout=5).read().decode()
    check("metrics exported", "mc_gateway_connections_total" in metrics)
    check("backend health gauge", 'mc_gateway_backend_up{backend="survival"' in metrics)
    print("\nSelected metrics:")
    for line in metrics.splitlines():
        if line.startswith(("mc_gateway_connections_total", "mc_gateway_login_attempts_total",
                            "mc_gateway_status_requests_total", "mc_gateway_backend_up",
                            "mc_gateway_backends_healthy", "mc_gateway_bytes_total")):
            print("  " + line)
finally:
    gateway.terminate()
    out, _ = gateway.communicate(timeout=10)

print("\nGateway log:")
for line in out.splitlines():
    print("  " + line)

print()
print("FAILURES: " + (", ".join(failures) if failures else "none"))
sys.exit(1 if failures else 0)
