#!/usr/bin/env python3
"""TLS terminator for Tailscale userspace: 127.0.0.1:8443 -> 127.0.0.1:ORIGIN."""
import socket
import ssl
import sys
import threading

ORIGIN_PORT = int(sys.argv[1])
CERT = sys.argv[2]
KEY = sys.argv[3]
LISTEN = ("127.0.0.1", 8443)


def pipe(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except Exception:
        pass
    for s in (src, dst):
        try:
            s.close()
        except Exception:
            pass


def handle(client):
    try:
        origin = socket.create_connection(("127.0.0.1", ORIGIN_PORT), 10)
    except Exception:
        try:
            client.close()
        except Exception:
            pass
        return
    threading.Thread(target=pipe, args=(client, origin), daemon=True).start()
    pipe(origin, client)


def main():
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(CERT, KEY)
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(LISTEN)
    sock.listen(128)
    while True:
        conn, _ = sock.accept()
        try:
            tls = ctx.wrap_socket(conn, server_side=True)
        except Exception:
            try:
                conn.close()
            except Exception:
                pass
            continue
        threading.Thread(target=handle, args=(tls,), daemon=True).start()


if __name__ == "__main__":
    main()
