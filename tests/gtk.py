#!/usr/bin/env python3
"""Run ignored Rust checks in separate private GTK/Broadway/D-Bus sessions.

  python3 tests/gtk.py --self-check
  python3 tests/gtk.py --list
  python3 tests/gtk.py recent_missing_entries_release_layout_slots
  python3 tests/gtk.py 'window::places::tests::*'

Full names, unique final names, and quoted shell patterns are accepted. Build
discovery uses Cargo offline; --test-binary reuses an already compiled harness.
"""
import argparse
from contextlib import contextmanager
import fnmatch
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote

REPO = Path(__file__).resolve().parents[1]


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()


def captured(command, *, env=None, timeout=120):
    # Pipes avoid the inherited RLIMIT_FSIZE used by a Trash regression.
    process = subprocess.Popen(command, cwd=REPO, env=env, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               start_new_session=True)
    try:
        output, _ = process.communicate(timeout=timeout)
    except BaseException:
        stop(process)
        output, _ = process.communicate()
        if output:
            print(output, end="", flush=True)
        raise
    return process.returncode, output


def test_binary():
    cargo = Path.home() / ".cargo/bin/cargo"
    command = [str(cargo) if cargo.is_file() else "cargo", "test", "--offline",
               "--no-run", "--message-format=json"]
    code, output = captured(command, timeout=600)
    executable = None
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get("reason") == "compiler-artifact" and message.get("profile", {}).get("test"):
            executable = message.get("executable") or executable
        if message.get("reason") == "compiler-message":
            print(message["message"].get("rendered", ""), end="", file=sys.stderr)
    if code or not executable:
        raise RuntimeError("Cargo did not produce the test harness:\n" + output)
    return Path(executable)


def ignored_tests(binary):
    code, output = captured([str(binary), "--ignored", "--list"])
    if code:
        raise RuntimeError(output)
    return [line[:-6] for line in output.splitlines() if line.endswith(": test")]


def select_tests(available, selectors):
    selected = []
    for selector in selectors:
        matches = [name for name in available if fnmatch.fnmatchcase(name, selector)]
        if not matches and not any(char in selector for char in "*?["):
            matches = [name for name in available if name.rsplit("::", 1)[-1] == selector]
            if len(matches) > 1:
                raise ValueError("Ambiguous test name: " + selector)
        if not matches:
            raise ValueError("No ignored test matches: " + selector)
        selected.extend(name for name in matches if name not in selected)
    return selected


def fixtures(root, name):
    if name.startswith("window::picker::tests::") and any(
            group in name for group in ["::search_tests::", "::basket_tests::"]):
        # A fresh desktop index has no /tmp fixtures. Exercise the real limited
        # fallback; this does not establish coverage of an installed live index.
        programs = root / "search-bin"
        programs.mkdir()
        for program in ["tinysparql", "tracker3"]:
            script = programs / program
            script.write_text("#!/bin/sh\nexit 1\n")
            script.chmod(0o755)
    if not name.endswith("::trash_grid_and_list_keep_selection_and_restore_identity"):
        return
    (root / "folder").mkdir()
    (root / "folder/visible.txt").write_text("ordinary folder selection")
    trash = root / "data/Trash"
    for directory in [trash / "files", trash / "info"]:
        directory.mkdir(parents=True)
    for stored, original in [("a-storage", "one/notes.txt"), ("b-storage", "two/notes.txt"),
                             ("c-storage", "picture.svg"), ("d-storage", ".hidden"),
                             ("e-storage", "other.txt")]:
        path = root / "original" / original
        path.parent.mkdir(parents=True, exist_ok=True)
        (trash / "files" / stored).write_text("contents of " + stored)
        (trash / "info" / (stored + ".trashinfo")).write_text(
            "[Trash Info]\nPath=" + quote(str(path), safe="/") + "\nDeletionDate=2026-10-22T10:00:00\n")


@contextmanager
def session(name):
    prefix = "filescope-gtk-"
    if name.endswith("::trash_refresh_does_not_select_reused_storage_name"):
        prefix = "filescope-trash-fixes-"
    elif name.endswith("::trash_grid_and_list_keep_selection_and_restore_identity"):
        prefix = "filescope-trash-appearance-acceptance-"
    with tempfile.TemporaryDirectory(prefix=prefix) as directory:
        root = Path(directory)
        env = os.environ.copy()
        for kind in ["CONFIG", "DATA", "CACHE", "STATE", "RUNTIME"]:
            path = root / kind.lower()
            path.mkdir(mode=0o700)
            env["XDG_" + kind + ("_DIR" if kind == "RUNTIME" else "_HOME")] = str(path)
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "DBUS_STARTER_ADDRESS",
                    "DBUS_STARTER_BUS_TYPE", "SESSION_MANAGER", "FILESCOPE_SHOT", "FILESCOPE_SHOT_VIEW",
                    "FILESCOPE_DEMO"]:
            env.pop(key, None)
        env.update(GDK_BACKEND="broadway", BROADWAY_DISPLAY=":1", GTK_A11Y="none",
                   GIO_USE_VFS="local", GSETTINGS_BACKEND="memory", ADW_DISABLE_PORTAL="1",
                   GTK_USE_PORTAL="0", SHELL="/bin/sh", TMPDIR=str(root),
                   FILESCOPE_TEST_INDEX="1", FILESCOPE_PRIVATE_PORTAL_TEST="1")
        fixtures(root, name)
        if (root / "search-bin").is_dir():
            env["PATH"] = str(root / "search-bin") + os.pathsep + env.get("PATH", "")
            env["FILESCOPE_TEST_SEARCH_FALLBACK"] = "1"
            print("FIXTURE: unavailable local index; exercising limited search fallback", flush=True)
        daemon = subprocess.Popen(["gtk4-broadwayd", "--unixsocket", str(root / "http.sock"), ":1"],
                                  env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  text=True, start_new_session=True)
        try:
            deadline = time.monotonic() + 5
            while not any((root / "runtime").glob("broadway*.socket")):
                if daemon.poll() is not None or time.monotonic() >= deadline:
                    stop(daemon)
                    raise RuntimeError("Broadway failed to start:\n" + daemon.stdout.read())
                time.sleep(0.02)
            yield env
        finally:
            stop(daemon)
            daemon.communicate()


def self_check_child():
    import gi
    gi.require_version("Gtk", "4.0")
    gi.require_version("Adw", "1")
    from gi.repository import Adw, Gio, GLib, Gtk

    root = Path(os.environ["TMPDIR"])
    assert root.name.startswith("filescope-gtk-")
    assert Path(GLib.get_user_data_dir()) == root / "data"
    assert (root / "runtime").stat().st_mode & 0o777 == 0o700
    assert "/tmp/" in os.environ["DBUS_SESSION_BUS_ADDRESS"]
    assert "FILESCOPE_SHOT" not in os.environ and "WAYLAND_DISPLAY" not in os.environ
    Adw.init()
    app = Adw.Application(application_id="dev.filescope.GtkRunnerCheck", flags=Gio.ApplicationFlags.NON_UNIQUE)
    app.register(None)
    window = Adw.ApplicationWindow(application=app, default_width=320, default_height=240)
    window.set_content(Gtk.Label(label="Private GTK runner"))
    window.present()
    loop = GLib.MainLoop()
    GLib.timeout_add(100, lambda: (loop.quit(), False)[1])
    loop.run()
    window.allocate(320, 240, -1, None)
    assert window.get_width() > 0 and window.get_height() > 0
    assert Gio.bus_get_sync(Gio.BusType.SESSION, None).get_unique_name()
    window.close()
    print("PASS: private D-Bus, Broadway window allocation, isolated XDG paths")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tests", nargs="*")
    parser.add_argument("--test-binary", type=Path)
    parser.add_argument("--app-binary", type=Path, help="built app for process/backend checks")
    parser.add_argument("--timeout", type=float, default=120, help="seconds per test (default: 120)")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--self-check-child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.self_check_child:
        self_check_child()
        return 0
    if args.self_check:
        assert select_tests(["a::test", "b::other"], ["test", "b::*"]) == ["a::test", "b::other"]
        try:
            select_tests(["a::test", "b::test"], ["test"])
        except ValueError:
            pass
        else:
            raise AssertionError("ambiguous test names must fail")
        with session("self-check") as env:
            code, output = captured(["dbus-run-session", "--", "/usr/bin/python3", str(Path(__file__).resolve()),
                                     "--self-check-child"], env=env, timeout=args.timeout)
            print(output, end="")
            return code
    if not args.tests and not args.list:
        parser.error("pass test names/patterns, --list, or --self-check")
    binary = args.test_binary.resolve() if args.test_binary else test_binary()
    available = ignored_tests(binary)
    if args.list:
        print("\n".join(available))
        return 0
    selected = select_tests(available, args.tests)
    failed = 0
    for name in selected:
        print("RUN " + name, flush=True)
        with session(name) as env:
            if args.app_binary:
                env["FILESCOPE_PICKER_BINARY"] = str(args.app_binary.resolve())
                env["FILESCOPE_TEST_BINARY"] = str(args.app_binary.resolve())
            try:
                code, output = captured(["dbus-run-session", "--", str(binary), name, "--ignored", "--exact",
                                         "--test-threads=1"], env=env, timeout=args.timeout)
            except subprocess.TimeoutExpired:
                code, output = 1, "Timed out after " + str(args.timeout) + " seconds\n"
            passed = code == 0 and "1 passed; 0 failed" in output
            if not passed:
                failed += 1
            print(output, end="", flush=True)
            print(("PASS " if passed else "FAIL ") + name, flush=True)
    print(f"{len(selected) - failed} passed; {failed} failed; {len(selected)} isolated checks")
    return int(bool(failed))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print("GTK runner: " + str(error), file=sys.stderr)
        sys.exit(1)
