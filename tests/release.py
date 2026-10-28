#!/usr/bin/env python3
"""Release packaging regression: a binary symlink must package executable bytes."""
import hashlib
from pathlib import Path
import subprocess
import tarfile
import tempfile


def check_symlinked_binary():
    repo = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="filescope-release-test-") as directory:
        root = Path(directory)
        binary = root / "actual-binary"
        binary.write_bytes(b"#!/bin/sh\nprintf 'filescope-update-v1\\n0.7.0\\nx86_64\\n'\n")
        binary.chmod(0o755)
        link = root / "filescope"
        link.symlink_to(binary.name)
        subprocess.run(["python3", str(repo / "packaging/release.py"), "--binary", str(link),
                        "--output", str(root / "out")], check=True)
        archive = root / "out/filescope-v0.7.0-x86_64-linux.tar.gz"
        with tarfile.open(archive) as bundle:
            entry = bundle.getmember("filescope-v0.7.0-x86_64-linux/filescope")
            assert entry.isfile(), "Release contains the input symlink instead of its binary"
            assert bundle.extractfile(entry).read() == binary.read_bytes()
            assert all(item.isfile() for item in bundle), "Updater rejects non-regular entries"
        assert archive.with_name(archive.name + ".sha256").read_text().split()[0] == hashlib.sha256(archive.read_bytes()).hexdigest()
        print("PASS symlinked binary: real executable bytes and matching archive digest")


if __name__ == "__main__":
    check_symlinked_binary()
