#!/usr/bin/env python3
"""Installer regression check; requires bubblewrap and system Python with PyGObject."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def check_installers():
    from gi.repository import GLib

    root = Path.cwd()
    app_id = "dev.filescope.Filescope"
    data = Path(os.environ["XDG_DATA_HOME"])
    provider = Path("/usr/local/share/gnome-shell/search-providers") / f"{app_id}.search-provider.ini"
    legacy = data / "gnome-shell/search-providers" / provider.name
    service = data / "dbus-1/services" / f"{app_id}.service"
    launcher = data / "applications" / f"{app_id}.desktop"
    binary = Path.home() / ".local/bin/filescope"
    auth_log = root / "auth.log"

    def run(script, *args, denied=False):
        before = auth_log.read_text() if auth_log.exists() else ""
        result = subprocess.run(["sh", str(script), *args], env={**os.environ, "DENY_AUTH": str(int(denied))},
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        assert (result.returncode != 0) == denied, result.stdout
        if denied:
            assert auth_log.read_text() != before, "Failure did not reach authentication"

    for script in [root / "source/install.sh", root / "bundle/install.sh"]:
        legacy.parent.mkdir(parents=True, exist_ok=True)
        legacy.write_text("obsolete registration")
        run(script, denied=True)
        assert not provider.exists() and legacy.exists()
        run(script)
        assert not legacy.exists()
        assert binary.exists() and launcher.exists() and service.exists()
        assert provider.stat().st_mode & 0o777 == 0o644
        assert any((Path(d) / "gnome-shell/search-providers" / provider.name).exists()
                   for d in GLib.get_system_data_dirs()), "GNOME cannot discover registration"
        keyfile = GLib.KeyFile()
        keyfile.load_from_file(str(provider), GLib.KeyFileFlags.NONE)
        assert keyfile.get_string("Shell Search Provider", "DesktopId") == launcher.name
        assert keyfile.get_string("Shell Search Provider", "Version") == "2"
        assert f'Exec="{binary}" --gapplication-service' in service.read_text()
        calls = auth_log.read_text()
        run(script)  # Identical registration must not authenticate again.
        assert auth_log.read_text() == calls
        run(script, "--uninstall", denied=True)
        assert all(p.exists() for p in [provider, service, launcher, binary])
        run(script, "--uninstall")
        assert not any(p.exists() for p in [provider, legacy, service, launcher, binary])
        run(script, "--uninstall")
        print(f"PASS {script.parent.name}: discovery, migration, reinstall, uninstall, denied authentication")


if __name__ == "__main__":
    if os.environ.get("FILESCOPE_INSTALL_TEST") == "1":
        check_installers()
    else:
        repo = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory(prefix="filescope-install-") as directory:
            work = Path(directory)
            for name in ["home", "share", "data", "bin", "source/data/icons", "source/target/release", "bundle"]:
                (work / name).mkdir(parents=True)
            shutil.copy(__file__, work / "check.py")
            shutil.copy(repo / "install.sh", work / "source/install.sh")
            shutil.copy(repo / "packaging/install-release.sh", work / "bundle/install.sh")
            app_id = "dev.filescope.Filescope"
            for dest in ["source/data", "bundle"]:
                shutil.copy(repo / f"data/{app_id}.desktop", work / dest)
            for dest in ["source/data/icons", "bundle"]:
                shutil.copy(repo / f"data/icons/{app_id}.svg", work / dest)
            for dest in ["source/target/release/filescope", "bundle/filescope", "bin/cargo"]:
                (work / dest).write_text("#!/bin/sh\nexit 0\n")
                (work / dest).chmod(0o755)
            # Substitute only authentication, and restrict the elevated command scope.
            for tool in ["pkexec", "sudo"]:
                shim = work / "bin" / tool
                shim.write_text('''#!/bin/sh
set -eu
echo "$*" >> auth.log
[ "$DENY_AUTH" = 0 ] || exit 1
case "$*" in
  "install -D -m 0644 "*" /usr/local/share/gnome-shell/search-providers/dev.filescope.Filescope.search-provider.ini"|"rm -f /usr/local/share/gnome-shell/search-providers/dev.filescope.Filescope.search-provider.ini") exec "$@" ;;
  *) echo "unexpected privileged command: $*" >&2; exit 2 ;;
esac
''')
                shim.chmod(0o755)
            subprocess.run([
                "bwrap", "--ro-bind", "/", "/", "--dev", "/dev",
                "--bind", str(work), str(work), "--bind", str(work / "home"), str(Path.home()),
                "--bind", str(work / "share"), "/usr/local/share", "--chdir", str(work),
                "--setenv", "FILESCOPE_INSTALL_TEST", "1",
                "--setenv", "TMPDIR", str(work),
                "--setenv", "XDG_DATA_HOME", str(work / "data"),
                "--setenv", "XDG_DATA_DIRS", "/usr/local/share:/usr/share",
                "--setenv", "PATH", f"{work}/bin:/usr/bin:/bin",
                "/usr/bin/python3", str(work / "check.py"),
            ], check=True)
