#!/usr/bin/env python3
"""Stage owned distribution files without modifying defaults or user state."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
APP_ID = "dev.filescope.Filescope"
OWNERS = {"deb": "apt", "rpm": "dnf", "flatpak": "flatpak", "tarball": "user-installer"}


def appstream(version, epoch):
    date = datetime.fromtimestamp(epoch, timezone.utc).date().isoformat()
    return (ROOT / f"data/{APP_ID}.metainfo.xml.in").read_text().replace("@VERSION@", version).replace("@DATE@", date)


def distribution(version, architecture, channel):
    return {"app_id": APP_ID, "version": version, "architecture": architecture, "channel": channel,
            "update_owner": OWNERS[channel], "minimum_runtime": {"gtk": "4.16", "libadwaita": "1.5", "vte_gtk4": "0.70"}}


def third_party_licenses():
    notices = []
    for crate in sorted((ROOT / "vendor").glob("*")):
        files = {file for pattern in ["LICENSE*", "LICENCE*", "COPYING*", "NOTICE*"] for file in crate.glob(pattern) if file.is_file()}
        for file in sorted(files):
            notices.append(f"===== {crate.name}/{file.name} =====\n{file.read_text(errors='replace')}\n")
    return "\n".join(notices) or "Dependency source licenses accompany the source bundle.\n"


def stage(binary, dest, channel):
    binary = binary.resolve(strict=True)
    dest = dest.resolve()
    if dest == Path("/") or dest.exists() and any(dest.iterdir()):
        raise ValueError("Use a new, empty staging directory")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    probe = subprocess.run([str(binary), "--update-probe"], check=True, text=True, capture_output=True, timeout=10).stdout.splitlines()
    if probe != ["filescope-update-v1", version, "x86_64"]:
        raise ValueError("Binary probe must match the source version and x86_64 target")
    prefix = "app" if channel == "flatpak" else "usr"
    base = dest / prefix

    def write(relative, text, mode=0o644):
        path = base / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(mode)

    def copy(source, relative, mode=0o644):
        path = base / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, path)
        path.chmod(mode)

    copy(binary, "bin/filescope", 0o755)
    copy(ROOT / f"data/icons/{APP_ID}.svg", f"share/icons/hicolor/scalable/apps/{APP_ID}.svg")
    executable = "filescope" if channel == "flatpak" else "/usr/bin/filescope"
    desktop = (ROOT / f"data/{APP_ID}.desktop").read_text().replace("Exec=filescope %U", f"Exec={executable} %U")
    write(f"share/applications/{APP_ID}.desktop", desktop)
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", (ROOT / "Cargo.toml").stat().st_mtime))
    write(f"share/metainfo/{APP_ID}.metainfo.xml", appstream(version, epoch))
    copy(ROOT / "LICENSE", "share/doc/filescope/LICENSE")
    write("share/doc/filescope/THIRD_PARTY_LICENSES", third_party_licenses())
    metadata = distribution(version, "x86_64", channel)
    write("share/filescope/distribution.json", json.dumps(metadata, indent=2) + "\n")
    if channel != "flatpak":
        write(f"share/dbus-1/services/{APP_ID}.service", f"[D-BUS Service]\nName={APP_ID}\nExec=/usr/bin/filescope --gapplication-service\n")
        picker = "org.freedesktop.impl.portal.desktop.filescope"
        write(f"share/dbus-1/services/{picker}.service", f"[D-BUS Service]\nName={picker}\nExec=/usr/bin/filescope --file-picker-portal\n")
        write("share/xdg-desktop-portal/portals/zz-filescope.portal", f"[portal]\nDBusName={picker}\nInterfaces=org.freedesktop.impl.portal.FileChooser;\nUseIn=filescope;\n")
        write(f"share/gnome-shell/search-providers/{APP_ID}.search-provider.ini", f"[Shell Search Provider]\nDesktopId={APP_ID}.desktop\nBusName={APP_ID}\nObjectPath=/dev/filescope/SearchProvider\nVersion=2\n")
        copy(ROOT / "packaging/check-migration.py", "bin/filescope-migration-check", 0o755)
    return metadata


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--dest", type=Path, required=True)
    parser.add_argument("--channel", choices=["deb", "rpm", "flatpak"], required=True)
    args = parser.parse_args()
    print(json.dumps(stage(args.binary, args.dest, args.channel), indent=2))
