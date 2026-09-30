"""Exercise the actual run recipe with Unix sockets and isolated runtime tools."""

import os
from pathlib import Path
import socket
import subprocess
import tempfile
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]


class SignalStartupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="signal-run-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.socket_path = self.root / "signal.sock"
        self.trace = self.root / "trace"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        lines = (ROOT / "justfile").read_text().split("run: setup\n", 1)[1]
        lines = lines.split("\n# Build the full workspace", 1)[0]
        self.recipe = self.root / "run.sh"
        self.recipe.write_text("\n".join(line[4:] for line in lines.splitlines()) + "\n")
        self.env = {
            **os.environ,
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "SIGNAL_CLI_SOCKET": str(self.socket_path),
            "SIGNAL_CLI_DATA_DIR": str(self.root),
            "SIGNAL_ACCOUNT": "+15555550123",
            "GEMINI_API_KEY": "test",
            "CHOTU_CAFFEINATE": "1",
            "STARTUP_TRACE": str(self.trace),
        }
        # Portable equivalents of nc/plutil, using real socket I/O and JSON.
        self.tool("nc", '''import socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
try:
    s.connect(sys.argv[-1])
    s.sendall(sys.stdin.buffer.read())
    print(s.recv(65536).decode(), end="")
except (OSError, TimeoutError):
    sys.exit(1)
finally:
    s.close()
''')
        self.tool("plutil", '''import json, sys
value = json.load(sys.stdin)[sys.argv[2]]
if isinstance(value, str):
    xml = "<string>" + value + "</string>"
elif isinstance(value, int):
    xml = "<integer>" + str(value) + "</integer>"
elif value == []:
    xml = "<array/>"
else:
    sys.exit(1)
print('<plist version="1.0">\\n' + xml + '\\n</plist>')
''')
        self.tool("cargo", '''import os
with open(os.environ["STARTUP_TRACE"], "a") as f:
    f.write("coordinator\\n")
''')
        self.tool("signal-cli", '''import os, signal, socket, sys
with open(os.environ["STARTUP_TRACE"], "a") as f:
    f.write("daemon\\n")
s = socket.socket(socket.AF_UNIX)
s.bind(os.environ["SIGNAL_CLI_SOCKET"])
s.listen()
def stop(*args):
    with open(os.environ["STARTUP_TRACE"], "a") as f:
        f.write("stopped\\n")
    s.close()
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
while True:
    c, _ = s.accept()
    with c:
        c.recv(65536)
        c.sendall(b'{"jsonrpc":"2.0","id":1,"result":[]}\\n')
''')

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)

    def run_recipe(self):
        return subprocess.run(
            ["bash", str(self.recipe)], env=self.env,
            capture_output=True, text=True, timeout=15,
        )

    def events(self):
        return self.trace.read_text().splitlines() if self.trace.exists() else []

    def external_socket(self, respond):
        server = socket.socket(socket.AF_UNIX)
        server.bind(str(self.socket_path))
        server.listen()
        self.addCleanup(server.close)
        stop = threading.Event()

        def serve():
            connection, _ = server.accept()
            with connection:
                connection.recv(65536)
                if respond:
                    connection.sendall(b'{"jsonrpc":"2.0","id":1,"result":[]}\n')
                else:
                    stop.wait(5)  # Live daemon whose reply exceeds the probe timeout.

        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 2)
        self.addCleanup(stop.set)

    def test_slow_external_daemon_keeps_socket(self):
        self.external_socket(respond=False)
        inode = self.socket_path.stat().st_ino
        result = self.run_recipe()
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("Leaving the socket intact", result.stderr)
        self.assertEqual(self.socket_path.stat().st_ino, inode)
        self.assertEqual(self.events(), [])
        # The external listener remains reachable by its original path.
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(1)
            client.connect(str(self.socket_path))

    def test_genuinely_stale_socket_requires_manual_removal(self):
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(self.socket_path))
        inode = self.socket_path.stat().st_ino
        result = self.run_recipe()
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("confirming no daemon is running", result.stderr)
        self.assertEqual(self.socket_path.stat().st_ino, inode)
        self.assertEqual(self.events(), [])

    def test_healthy_external_daemon_is_reused(self):
        self.external_socket(respond=True)
        inode = self.socket_path.stat().st_ino
        result = self.run_recipe()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.socket_path.stat().st_ino, inode)
        self.assertEqual(self.events(), ["coordinator"])

    def test_missing_socket_starts_and_stops_owned_daemon(self):
        result = self.run_recipe()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.events(), ["daemon", "coordinator", "stopped"])


if __name__ == "__main__":
    unittest.main()
