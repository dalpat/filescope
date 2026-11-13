#!/usr/bin/env python3
"""Build the release archive and SHA-256 file accepted by Filescope's updater."""
import argparse
import hashlib
import gzip
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile

import stage


def package(binary: Path, output: Path):
    binary = binary.resolve(strict=True)
    probe = subprocess.run([str(binary), "--update-probe"], check=True,
                           capture_output=True, text=True, timeout=10).stdout.splitlines()
    if len(probe) != 3 or probe[0] != "filescope-update-v1":
        raise ValueError("Binary does not support update format v1")
    version, arch = probe[1:]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Only stable release versions are accepted")
    if arch not in ("x86_64", "aarch64"):
        raise ValueError(f"Unsupported architecture: {arch}")
    root = Path(__file__).resolve().parents[1]
    name = f"filescope-v{version}-{arch}-linux"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"{name}.tar.gz"
    checksum = output / f"{name}.tar.gz.sha256"
    if archive.exists() or checksum.exists():
        raise FileExistsError("Release artifacts already exist; choose a fresh output directory")
    with tempfile.TemporaryDirectory(prefix="filescope-release-") as temp:
        epoch = int(os.environ.get("SOURCE_DATE_EPOCH", binary.stat().st_mtime))
        manifest = Path(temp) / "update-manifest"
        manifest.write_text("\n".join(probe) + "\n")
        metadata = Path(temp) / "distribution.json"
        distribution = stage.distribution(version, arch, "tarball")
        metadata.write_text(json.dumps(distribution, indent=2) + "\n")
        appstream = Path(temp) / f"{stage.APP_ID}.metainfo.xml"
        appstream.write_text(stage.appstream(version, epoch))
        notices = Path(temp) / "THIRD_PARTY_LICENSES"
        notices.write_text(stage.third_party_licenses())
        files = {
            "filescope": binary,
            "update-manifest": manifest,
            "distribution.json": metadata,
            f"{stage.APP_ID}.metainfo.xml": appstream,
            "install.sh": root / "packaging/install-release.sh",
            "install-binary.sh": root / "packaging/install-binary.sh",
            "update-layout": root / "packaging/update-layout",
            "dev.filescope.Filescope.desktop": root / "data/dev.filescope.Filescope.desktop",
            "dev.filescope.Filescope.svg": root / "data/icons/dev.filescope.Filescope.svg",
            "README.md": root / "README.md",
            "CHANGELOG.md": root / "CHANGELOG.md",
            "LICENSE": root / "LICENSE",
            "THIRD_PARTY_LICENSES": notices,
        }
        try:
            with archive.open("xb") as destination:
                with gzip.GzipFile(fileobj=destination, mode="wb", filename="", mtime=epoch) as compressed:
                    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as bundle:
                        for entry, source in files.items():
                            info = bundle.gettarinfo(str(source), arcname=f"{name}/{entry}")
                            info.uid = info.gid = 0
                            info.uname = info.gname = ""
                            info.mtime = epoch
                            info.mode = 0o755 if entry in ["filescope", "install.sh", "install-binary.sh"] else 0o644
                            with source.open("rb") as contents:
                                bundle.addfile(info, contents)
            sha = hashlib.sha256(archive.read_bytes()).hexdigest()
            with checksum.open("x") as destination:
                destination.write(f"{sha}  {archive.name}\n")
        except BaseException:
            archive.unlink(missing_ok=True)
            raise
    return archive, checksum


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/filescope"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for artifact in package(args.binary, args.output):
        print(artifact)
