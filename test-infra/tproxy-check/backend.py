import socket, threading
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", 25565)); srv.listen(32)
def handle(conn, addr):
    try:
        data = conn.recv(4096)
        if data:
            print(f"BACKEND accepted peer={addr[0]}:{addr[1]} bytes={len(data)}", flush=True)
            conn.sendall(f"peer={addr[0]}:{addr[1]}\n".encode())
    finally:
        conn.close()
while True:
    c, a = srv.accept()
    threading.Thread(target=handle, args=(c, a), daemon=True).start()
