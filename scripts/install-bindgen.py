"""Install the official pinned CLI binary after verifying its published SHA256."""
from pathlib import Path
import hashlib
import os
import platform
import shutil
import tarfile
import tempfile
import urllib.request

VERSION = "0.2.130"
TARGET = "x86_64-pc-windows-msvc" if platform.system() == "Windows" else "x86_64-unknown-linux-musl"
ARCHIVE = f"wasm-bindgen-{VERSION}-{TARGET}.tar.gz"
BASE = f"https://github.com/wasm-bindgen/wasm-bindgen/releases/download/{VERSION}/"
binary_name = "wasm-bindgen.exe" if platform.system() == "Windows" else "wasm-bindgen"
destination = Path.home() / ".cargo" / "bin"
destination.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="spark-bindgen-") as temporary:
    archive = Path(temporary) / ARCHIVE
    with urllib.request.urlopen(BASE + ARCHIVE, timeout=90) as source, archive.open("wb") as output:
        shutil.copyfileobj(source, output)
    with urllib.request.urlopen(BASE + ARCHIVE + ".sha256sum", timeout=30) as response:
        expected = response.read().decode("ascii").split()[0]
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if len(expected) != 64 or actual != expected:
        raise SystemExit("Official wasm-bindgen checksum mismatch")
    with tarfile.open(archive, "r:gz") as package:
        member = package.getmember(f"wasm-bindgen-{VERSION}-{TARGET}/{binary_name}")
        if not member.isfile():
            raise SystemExit("Invalid wasm-bindgen archive")
        with package.extractfile(member) as source, (destination / binary_name).open("wb") as output:
            shutil.copyfileobj(source, output)
    (destination / binary_name).chmod(0o755)
if os.environ.get("GITHUB_PATH"):
    with open(os.environ["GITHUB_PATH"], "a", encoding="utf-8") as output:
        output.write(str(destination) + "\n")
print(f"Verified official wasm-bindgen {VERSION} ({TARGET})")
