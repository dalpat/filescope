#!/usr/bin/env python3
"""Release packaging regression: a binary symlink must package executable bytes."""
import hashlib
import json
import os
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


def check_reproducible_metadata():
    repo = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="filescope-reproducible-release-") as directory:
        root = Path(directory)
        binary = root / "filescope"
        binary.write_bytes(b"#!/bin/sh\nprintf 'filescope-update-v1\\n0.8.0\\nx86_64\\n'\n")
        binary.chmod(0o755)
        env = {**os.environ, "SOURCE_DATE_EPOCH": "1793318400"}
        archives = []
        for index in [1, 2]:
            os.utime(binary, (1000 * index, 1000 * index))
            dest = root / str(index)
            subprocess.run(["python3", str(repo / "packaging/release.py"), "--binary", str(binary), "--output", str(dest)], check=True, env=env)
            archives.append(next(dest.glob("*.tar.gz")))
        assert archives[0].read_bytes() == archives[1].read_bytes(), "Identical bytes produced different release archives"
        with tarfile.open(archives[0]) as bundle:
            prefix = "filescope-v0.8.0-x86_64-linux/"
            metadata = json.load(bundle.extractfile(prefix + "distribution.json"))
            assert metadata["version"] == "0.8.0" and metadata["update_owner"] == "user-installer"
            assert b'version="0.8.0"' in bundle.extractfile(prefix + "dev.filescope.Filescope.metainfo.xml").read()
            assert bundle.extractfile(prefix + "THIRD_PARTY_LICENSES").read(), "Tarball omitted dependency notices"
        print("PASS reproducible archive and shared version/update ownership metadata")


if __name__ == "__main__":
    check_symlinked_binary()
    check_reproducible_metadata()
