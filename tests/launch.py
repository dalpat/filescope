#!/usr/bin/env python3
"""Check actual CLI forwarding and FileManager1 activation on private buses.

  /usr/bin/python3 tests/launch.py
  /usr/bin/python3 tests/launch.py --binary target/debug/filescope warm-forwarding

The default builds the application offline. Each case gets private D-Bus, XDG
paths, Trash, and an authenticated Xvfb display. Requires Xvfb and xauth; pass
--xvfb for a privately extracted Xvfb. The GTK runner supplies the isolation
helpers. Native desktop/dock interaction is not tested.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET

sys.dont_write_bytecode = True
import gtk as runner

APP = "dev.filescope.Filescope"
APP_PATH = "/dev/filescope/Filescope"
FILE_MANAGER = "org.freedesktop.FileManager1"
CASES = ["cold-trash-double", "cold-trash-triple", "warm-forwarding", "filemanager-activation"]


def build_application():
    cargo = Path.home() / ".cargo/bin/cargo"
    code, output = runner.captured([str(cargo) if cargo.is_file() else "cargo", "build", "--offline",
                                    "--message-format=json"], timeout=600)
    executable = None
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if (message.get("reason") == "compiler-artifact"
                and message.get("target", {}).get("name") == "filescope"
                and message.get("target", {}).get("kind") == ["bin"]):
            executable = message.get("executable") or executable
        if message.get("reason") == "compiler-message":
            print(message["message"].get("rendered", ""), end="", file=sys.stderr)
    if code or not executable:
        raise RuntimeError("Cargo did not build the application:\n" + output)
    return Path(executable)


def prepare(env, binary):
    root = Path(env["TMPDIR"])
    config = Path(env["XDG_CONFIG_HOME"]) / "filescope"
    config.mkdir()
    (config / "settings").write_text("automatic_updates=false\nrestore_session=false\n")
    folder = root / "fixture-folder"
    folder.mkdir()
    (folder / "notes.txt").write_text("read-only launch fixture")
    trash = Path(env["XDG_DATA_HOME"]) / "Trash"
    for directory in [trash / "files", trash / "info"]:
        directory.mkdir(parents=True)
    (trash / "files/fixture-item").write_text("disposable Trash fixture")
    (trash / "info/fixture-item.trashinfo").write_text(
        f"[Trash Info]\nPath={root}/original.txt\nDeletionDate=2026-10-30T10:00:00\n")
    services = Path(env["XDG_DATA_HOME"]) / "dbus-1/services"
    services.mkdir(parents=True)
    # Match file_manager::install_service without changing MIME/default settings.
    (services / (FILE_MANAGER + ".service")).write_text(
        f"[D-BUS Service]\nName={FILE_MANAGER}\nExec={binary} --gapplication-service\n")
    config = ET.Element("busconfig")
    ET.SubElement(config, "type").text = "session"
    ET.SubElement(config, "listen").text = "unix:tmpdir=" + env["XDG_RUNTIME_DIR"]
    ET.SubElement(config, "auth").text = "EXTERNAL"
    ET.SubElement(config, "servicedir").text = str(services)
    policy = ET.SubElement(config, "policy", user=str(os.getuid()))
    ET.SubElement(policy, "allow", send_destination="*")
    ET.SubElement(policy, "allow", receive_sender="*")
    ET.SubElement(policy, "allow", own="*")
    ET.ElementTree(config).write(root / "bus.conf", encoding="unicode")
    env.update(GDK_BACKEND="x11", GDK_DEBUG="no-portals", GSK_RENDERER="cairo", XAUTHORITY=str(root / "display.auth"),
               LIBGL_ALWAYS_SOFTWARE="1", XDG_SESSION_TYPE="x11",
               DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(root / "no-system-bus"))


def session_case(binary, case, xvfb):
    import gi
    import resource
    from gi.repository import Gio, GLib

    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    root = Path(os.environ["TMPDIR"])
    assert root.name.startswith("filescope-gtk-")
    assert Path(os.environ["XDG_DATA_HOME"]).parent == root
    assert "/tmp/" in os.environ["DBUS_SESSION_BUS_ADDRESS"]
    assert "FILESCOPE_SHOT" not in os.environ
    assert os.environ["DBUS_SYSTEM_BUS_ADDRESS"] == "unix:path=" + str(root / "no-system-bus")
    assert "automatic_updates=false" in (Path(os.environ["XDG_CONFIG_HOME"]) / "filescope/settings").read_text()
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    process = None
    primary_pid = None
    primary_fd = None
    primary_owner = None
    window_path = None
    compositor = None
    passed = False

    def call(destination, path, interface, method, args=None):
        return bus.call_sync(destination, path, interface, method, args, None,
                             Gio.DBusCallFlags.NONE, 5000, None)

    def daemon(method, name):
        return call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                    method, GLib.Variant("(s)", (name,))).unpack()[0]

    def owner():
        try:
            return daemon("GetNameOwner", APP)
        except GLib.Error:
            return None

    def until(condition, description):
        deadline = time.monotonic() + 10
        while True:
            if compositor is not None:
                assert compositor.poll() is None, f"Private display server exited during {description}"
            if process is not None:
                assert process.poll() is None, f"Primary exited during {description}"
            if primary_fd is not None:
                poll = select.poll()
                poll.register(primary_fd, select.POLLIN)
                assert not poll.poll(0), f"Primary exited during {description}"
            result = condition()
            if result:
                return result
            assert time.monotonic() < deadline, f"Timed out waiting for {description}"
            time.sleep(0.03)

    def track_primary():
        nonlocal primary_pid, primary_fd, primary_owner
        primary_owner = until(owner, "the application bus name")
        primary_pid = daemon("GetConnectionUnixProcessID", primary_owner)
        primary_fd = os.pidfd_open(primary_pid)
        if process is not None:
            assert primary_pid == process.pid, "CLI launch did not become the primary application"

    def describe_window():
        nonlocal window_path
        if window_path:
            try:
                return call(APP, window_path, "org.gtk.Actions", "DescribeAll").unpack()[0]
            except GLib.Error:
                window_path = None
        pending = [(APP_PATH, 0)]
        while pending:
            path, depth = pending.pop()
            try:
                node = ET.fromstring(call(APP, path, "org.freedesktop.DBus.Introspectable", "Introspect").unpack()[0])
                if node.find("interface[@name='org.gtk.Actions']") is not None:
                    actions = call(APP, path, "org.gtk.Actions", "DescribeAll").unpack()[0]
                    if "new-folder" in actions:
                        window_path = path
                        return actions
                if depth < 3:
                    pending.extend((path + "/" + child.attrib["name"], depth + 1) for child in node.findall("node"))
            except GLib.Error:
                pass
        return None

    def settled(folder):
        observed = None
        def ready():
            nonlocal observed
            actions = describe_window()
            observed = actions.get("new-folder", (None,))[0] if actions else None
            return actions if actions and actions["new-folder"][0] == folder else None
        try:
            actions = until(ready, "folder actions" if folder else "virtual Trash actions")
        except AssertionError as error:
            raise AssertionError(f"{error}; observed new-folder={observed}, window={window_path}") from None
        assert actions["new-tab"][0] and actions["location"][0], "Window has no usable navigation actions"
        assert daemon("GetConnectionUnixProcessID", APP) == primary_pid
        assert owner() == primary_owner, "The primary application owner changed"

    def secondary(*args):
        code, output = runner.captured([str(binary), *args], timeout=10)
        assert code == 0, f"Secondary launch failed ({code}):\n{output}"
        assert owner() == primary_owner, "Secondary launch replaced the primary"

    def activate(name):
        call(APP, window_path, "org.gtk.Actions", "Activate", GLib.Variant("(sava{sv})", (name, [], {})))

    try:
        auth = os.environ["XAUTHORITY"]
        cookie = secrets.token_hex(16)
        # Xvfb loads the cookie before its display number is chosen. Xauthority
        # records the selected number afterward for the clients' lookup.
        subprocess.run(["xauth", "-f", auth, "source", "-"],
                       input=f"add :0 MIT-MAGIC-COOKIE-1 {cookie}\n", text=True,
                       check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        read_fd, write_fd = os.pipe()
        try:
            compositor = subprocess.Popen([str(xvfb), "-displayfd", str(write_fd), "-screen", "0", "800x600x24",
                "-nolisten", "tcp", "-auth", auth, "-noreset"], pass_fds=(write_fd,),
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
            os.close(write_fd)
            write_fd = None
            until(lambda: select.select([read_fd], [], [], 0)[0], "private X display")
            number = int(os.read(read_fd, 64).decode("ascii").strip())
        finally:
            os.close(read_fd)
            if write_fd is not None:
                os.close(write_fd)
        display = f":{number}"
        subprocess.run(["xauth", "-f", auth, "source", "-"],
                       input=f"add {display} MIT-MAGIC-COOKIE-1 {cookie}\n", text=True,
                       check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        os.environ["DISPLAY"] = display
        call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "UpdateActivationEnvironment",
             GLib.Variant("(a{ss})", ({"DISPLAY": display, "XAUTHORITY": auth},)))
        assert owner() is None, "Launch check requires a fresh private bus"
        folder = root / "fixture-folder"
        if case == "filemanager-activation":
            assert not daemon("NameHasOwner", FILE_MANAGER)
            reply = call(FILE_MANAGER, "/org/freedesktop/FileManager1", FILE_MANAGER, "ShowFolders",
                         GLib.Variant("(ass)", (["trash:///"], "")))
            assert reply.unpack() == (), "FileManager1 did not finish its reply"
            track_primary()
            assert daemon("GetConnectionUnixProcessID", FILE_MANAGER) == primary_pid
            settled(False)
            call(FILE_MANAGER, "/org/freedesktop/FileManager1", FILE_MANAGER, "ShowFolders",
                 GLib.Variant("(ass)", ([folder.as_uri()], "")))
            settled(True)
            call(FILE_MANAGER, "/org/freedesktop/FileManager1", FILE_MANAGER, "ShowFolders",
                 GLib.Variant("(ass)", (["trash://"], "")))
            settled(False)
        else:
            first = {"cold-trash-double": "trash://", "cold-trash-triple": "trash:///",
                     "warm-forwarding": str(folder)}[case]
            process = subprocess.Popen([str(binary), first], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                       text=True, start_new_session=True)
            track_primary()
            settled(case == "warm-forwarding")
            if case == "warm-forwarding":
                for uri in ["trash://", "trash:///"]:
                    secondary(uri)
                    settled(False)
                secondary()  # GApplication Activate keeps the existing window usable.
                settled(False)
                secondary(str(folder))
                settled(True)
                secondary("trash:///")
                settled(False)
        activate("new-tab")
        settled(True)
        activate("close-tab")
        settled(False)
        print(f"PASS {case}: primary PID {primary_pid}; GTK action transitions and navigation are usable")
        passed = True
    finally:
        try:
            if primary_fd is None and owner():
                track_primary()
            if process is not None:
                runner.stop(process)
                output = process.communicate()[0]
                if output:
                    print(output, end="", file=sys.stderr)
            elif primary_fd is not None:
                # The activated process is not our Popen child; the private-bus
                # pidfd prevents cleanup from signalling a reused/user PID.
                poll = select.poll()
                poll.register(primary_fd, select.POLLIN)
                if not poll.poll(0):
                    signal.pidfd_send_signal(primary_fd, signal.SIGTERM)
                    if not poll.poll(3000):
                        signal.pidfd_send_signal(primary_fd, signal.SIGKILL)
                        assert poll.poll(3000), "Activated test process did not exit"
            if primary_fd is not None:
                os.close(primary_fd)
        finally:
            if compositor is not None:
                runner.stop(compositor)
                output = compositor.communicate()[0]
                if output and not passed:
                    print(output[-4000:], end="", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cases", nargs="*", choices=CASES)
    parser.add_argument("--binary", type=Path, help="use an already built application")
    parser.add_argument("--xvfb", type=Path, help="private Xvfb executable instead of the installed one")
    parser.add_argument("--session", choices=CASES, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.session:
        session_case(args.binary.resolve(), args.session, args.xvfb.resolve())
        return 0
    xvfb = args.xvfb or shutil.which("Xvfb")
    if not xvfb or not shutil.which("xauth"):
        raise RuntimeError("Xvfb and xauth are required; --xvfb accepts a privately extracted Xvfb")
    xvfb = Path(xvfb).resolve()
    source = args.binary.resolve() if args.binary else build_application()
    with tempfile.TemporaryDirectory(prefix="filescope-launch-build-") as directory:
        binary = Path(directory) / "filescope"
        shutil.copy2(source, binary)
        print("Application SHA-256: " + hashlib.sha256(binary.read_bytes()).hexdigest(), flush=True)
        cases = args.cases or CASES
        for case in cases:
            print("RUN " + case, flush=True)
            with runner.session("launch-" + case) as env:
                prepare(env, binary)
                code, output = runner.captured(["dbus-run-session", "--config-file", str(Path(env["TMPDIR"]) / "bus.conf"), "--", "/usr/bin/python3",
                    str(Path(__file__).resolve()), "--binary", str(binary), "--xvfb", str(xvfb), "--session", case], env=env, timeout=60)
                print(output, end="", flush=True)
                if code:
                    return code
        print(f"{len(cases)} actual-process checks passed; native desktop/dock acceptance remains pending")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (AssertionError, OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print("Launch check: " + str(error), file=sys.stderr)
        sys.exit(1)
