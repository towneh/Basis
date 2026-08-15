# Paced HTTP-TS server: streams a .ts file at ~1x (byte-rate derived from
# its duration) with no Content-Length, looping the file, like a live edge.
import socket
import sys
import threading
import time

path, duration_s, port = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
data = open(path, "rb").read()
rate = len(data) / duration_s  # bytes per second
TICK = 0.05


def serve(conn):
    try:
        conn.recv(65536)  # request; ignore
        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n")
        sent = 0.0
        start = time.monotonic()
        pos = 0
        while True:
            due = (time.monotonic() - start) * rate
            if due > sent:
                n = int(due - sent)
                chunk = data[pos:pos + n]
                if len(chunk) < n:  # end of file: stop like a finished live event
                    if chunk:
                        conn.sendall(chunk)
                    return
                pos += n
                conn.sendall(chunk)
                sent += n
            time.sleep(TICK)
    except OSError:
        pass
    finally:
        conn.close()


listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", port))
listener.listen(4)
print(f"serving {path} at {rate:.0f} B/s on 127.0.0.1:{port}", flush=True)
while True:
    conn, _ = listener.accept()
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
