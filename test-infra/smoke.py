"""End-to-end smoke test against the real mc-gateway binary.

Starts a fake Minecraft backend, runs the gateway in front of it, and checks
what a client and the backend actually see on the wire.

    python3 test-infra/smoke.py ./target/release/mc-gateway --config test-infra/smoke.yaml
"""
import json, socket, struct, subprocess, threading, time, sys, urllib.request

GW = ("127.0.0.1", 25599)
BACKEND_PORT = 25601

BACKEND_STATUS = {
    "version": {"name": "Paper 1.21.1", "protocol": 767},
    "players": {"max": 100, "online": 7, "sample": [{"name": "Notch", "id": "x"}]},
    "description": {"text": "A Minecraft Server"},
    "favicon": "data:image/png;base64,AAAA",
    "enforcesSecureChat": False,
}


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
        threading.Thread(target=serve_backend, args=(conn,), daemon=True).start()


def serve_backend(conn):
    """Records the first bytes, answers status pings, echoes everything else."""
    try:
        head = conn.recv(4096)
        if not head:
            return
        received.append(head)

        # next_state == 1 means the client wants a status response.
        if head[-1:] == b"\x01" or b"\x01\x01\x00" in head:
            body = json.dumps(BACKEND_STATUS).encode()
            conn.sendall(packet(0x00, varint(len(body)) + body))
        while True:
            data = conn.recv(4096)
            if not data:
                return
            conn.sendall(data)  # echoing a ping packet is a valid pong
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


def status_ping(host):
    s = socket.create_connection(GW, timeout=5)
    s.sendall(handshake(host, 1) + packet(0x00, b""))
    read_varint(s)
    read_varint(s)
    length = read_varint(s)
    buf = b""
    while len(buf) < length:
        buf += s.recv(length - len(buf))
    s.close()
    return json.loads(buf.decode())


try:
    # --- status ping: passed through, second line replaced -----------------
    status = status_ping("survival.example.net")
    lines = status["description"]["text"].split("\n")

    print("\nMOTD as a client sees it:")
    for line in lines:
        print("  " + line)

    check("backend's first line kept", lines[0] == "A Minecraft Server", lines[0])
    check("second line replaced by the gateway", len(lines) > 1 and "MY NETWORK" in lines[1],
          lines[1] if len(lines) > 1 else "<missing>")
    check("backend version passed through", status["version"]["name"] == "Paper 1.21.1")
    check("backend player count passed through", status["players"]["online"] == 7)
    check("backend sample passed through", status["players"]["sample"][0]["name"] == "Notch")
    check("backend favicon passed through", status["favicon"] == "data:image/png;base64,AAAA")

    # --- login --------------------------------------------------------------
    before = len(received)
    s = socket.create_connection(GW, timeout=5)
    s.sendall(handshake("survival.example.net", 2) + packet(0x00, mcstring("Notch") + b"\0" * 16))
    deadline = time.time() + 5
    while len(received) <= before and time.time() < deadline:
        time.sleep(0.05)
    head = received[-1] if len(received) > before else b""

    check("session reached the backend", bool(head))
    check("no PROXY header, no framing of our own",
          head[:12] != b"\r\n\r\n\x00\r\nQUIT\n" and not head.startswith(b"PROXY "),
          head[:12].hex())
    check("handshake arrives verbatim", b"survival.example.net" in head)

    s.sendall(b"post-login")
    check("pipe carries traffic", s.recv(64) == b"post-login")
    s.close()

    # --- a route with no backend -------------------------------------------
    s = socket.create_connection(GW, timeout=5)
    s.sendall(handshake("modded.example.net", 2) + packet(0x00, mcstring("Bob") + b"\0" * 16))
    read_varint(s)
    pid = read_varint(s)
    length = read_varint(s)
    reason = json.loads(s.recv(length).decode())
    s.close()
    check("offline backend produces a kick", pid == 0x00 and "offline" in reason["text"],
          reason["text"])

    offline = status_ping("modded.example.net")
    check("offline route gets the offline MOTD", "offline" in offline["description"]["text"].lower(),
          offline["description"]["text"])

    # --- metrics ------------------------------------------------------------
    metrics = urllib.request.urlopen("http://127.0.0.1:9199/metrics", timeout=5).read().decode()
    check("metrics exported", "mc_gateway_connections_total" in metrics)
    check("MOTD rewrites counted", "mc_gateway_motd_rewrites_total" in metrics)
    print("\nSelected metrics:")
    for line in metrics.splitlines():
        if line.startswith(("mc_gateway_connections_total", "mc_gateway_login_attempts_total",
                            "mc_gateway_status_requests_total", "mc_gateway_backend_up",
                            "mc_gateway_motd_rewrites_total", "mc_gateway_backends_healthy")):
            print("  " + line)
finally:
    gateway.terminate()
    out, _ = gateway.communicate(timeout=30)

print("\nGateway log:")
for line in out.splitlines():
    print("  " + line)

print()
print("FAILURES: " + (", ".join(failures) if failures else "none"))
sys.exit(1 if failures else 0)
