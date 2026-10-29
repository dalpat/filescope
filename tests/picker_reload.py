#!/usr/bin/python3
"""Verify live chooser switching through the real portal on a private bus.

Run after cargo build: /usr/bin/python3 tests/picker_reload.py target/debug/filescope
The two chooser backends and systemctl are fixtures; the portal frontend is real.
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET

import gi
from gi.repository import Gio, GLib

FRONTEND = 'org.freedesktop.portal.Desktop'
OBJECT = '/org/freedesktop/portal/desktop'
CHOOSER = 'org.freedesktop.portal.FileChooser'
IMPL = 'org.freedesktop.impl.portal.FileChooser'
CONTROL = 'dev.filescope.ReloadTest'


def wait(ready, seconds=10):
    deadline = time.monotonic() + seconds
    context = GLib.MainContext.default()
    while not ready():
        while context.pending():
            context.iteration(False)
        assert time.monotonic() < deadline, 'Private portal operation timed out'
        time.sleep(.002)


def connection():
    return Gio.DBusConnection.new_for_address_sync(
        os.environ['DBUS_SESSION_BUS_ADDRESS'],
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
        None, None)


def own(bus, name):
    reply = bus.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
                          'org.freedesktop.DBus', 'RequestName', GLib.Variant('(su)', (name, 4)),
                          None, Gio.DBusCallFlags.NONE, 3000, None)
    assert reply.unpack()[0] == 1, name


def call(bus, destination, path, interface, method, args):
    replies = []

    def done(source, result, *_):
        try:
            replies.append(source.call_finish(result))
        except GLib.Error as error:
            replies.append(error)

    bus.call(destination, path, interface, method, args, None,
             Gio.DBusCallFlags.NONE, 5000, None, done)
    wait(lambda: bool(replies))
    if isinstance(replies[0], Exception):
        raise replies[0]
    return replies[0]


class Backend:
    def __init__(self, identifier, uri):
        self.bus = connection()
        self.uri = uri
        self.pending = {}
        self.calls = []
        own(self.bus, 'org.freedesktop.impl.portal.desktop.' + identifier)
        methods = ''.join(
            f'<method name="{name}"><arg type="o" direction="in"/>'
            '<arg type="s" direction="in"/><arg type="s" direction="in"/>'
            '<arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>'
            '<arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>'
            for name in ['OpenFile', 'SaveFile', 'SaveFiles'])
        info = Gio.DBusNodeInfo.new_for_xml(
            f'<node><interface name="{IMPL}"><property name="version" type="u" access="read"/>'
            + methods + '</interface></node>').interfaces[0]
        self.bus.register_object(OBJECT, info, self.request,
                                 lambda *_: GLib.Variant('u', 4), None)

    def request(self, bus, sender, path, interface, method, params, invocation):
        handle, _, _, title, _ = params.unpack()
        self.calls.append((method, title))
        if title == 'busy':
            info = Gio.DBusNodeInfo.new_for_xml(
                '<node><interface name="org.freedesktop.impl.portal.Request"><method name="Close"/></interface></node>').interfaces[0]

            def close(*args):
                original, registration = self.pending.pop(handle)
                original.return_value(GLib.Variant('(ua{sv})', (1, {})))
                bus.unregister_object(registration)
                args[-1].return_value(GLib.Variant('()', ()))

            registration = bus.register_object(handle, info, close, None, None)
            self.pending[handle] = (invocation, registration)
        else:
            invocation.return_value(GLib.Variant('(ua{sv})',
                (0, {'uris': GLib.Variant('as', [self.uri])})))


class Desktop:
    def __init__(self, root, binary):
        self.root, self.binary = root, binary
        self.client = connection()
        self.responses = {}
        self.restarts = 0
        self.break_next_restart = False
        self.frontend = None
        self.log = (root / 'portal.log').open('w')
        self.previous = Backend('gtk', (root / 'previous.txt').as_uri())
        self.filescope = Backend('filescope', (root / 'filescope.txt').as_uri())
        own(self.client, CONTROL)
        info = Gio.DBusNodeInfo.new_for_xml(
            f'<node><interface name="{CONTROL}"><method name="Restart"/></interface></node>').interfaces[0]
        self.client.register_object('/dev/filescope/ReloadTest', info, self.restart, None, None)
        self.client.signal_subscribe(FRONTEND, 'org.freedesktop.portal.Request', 'Response',
                                     None, None, Gio.DBusSignalFlags.NONE,
                                     lambda _bus, _sender, path, _iface, _signal, params, *_:
                                     self.responses.update({path: params.unpack()}))

    def owner(self):
        try:
            reply = self.client.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetNameOwner', GLib.Variant('(s)', (FRONTEND,)),
                None, Gio.DBusCallFlags.NONE, 1000, None)
            return reply.unpack()[0]
        except GLib.Error as error:
            if Gio.DBusError.get_remote_error(error) == 'org.freedesktop.DBus.Error.NameHasNoOwner':
                return None
            raise

    def start(self):
        env = os.environ.copy()
        if self.break_next_restart:
            # Simulate a frontend that restarts but cannot provide the requested chooser.
            broken = self.root / 'unavailable-chooser'
            broken.mkdir(exist_ok=True)
            (broken / 'filescopetest-portals.conf').write_text('[preferred]\ndefault=none\n')
            env['XDG_DESKTOP_PORTAL_DIR'] = str(broken)
            self.break_next_restart = False
        self.frontend = subprocess.Popen(['/usr/libexec/xdg-desktop-portal', '--verbose'],
                                          env=env, stdout=self.log, stderr=subprocess.STDOUT)

    def stop(self):
        if self.frontend is not None and self.frontend.poll() is None:
            self.frontend.terminate()
            try:
                self.frontend.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.frontend.kill()
                self.frontend.wait()

    def restart(self, bus, sender, path, interface, method, params, invocation):
        self.restarts += 1
        previous = self.owner()
        self.stop()
        self.start()
        deadline = time.monotonic() + 8

        def ready():
            owner = self.owner()
            if owner and owner != previous:
                invocation.return_value(GLib.Variant('()', ()))
                return False
            if time.monotonic() >= deadline:
                invocation.return_dbus_error(CONTROL + '.Failed', 'Frontend did not restart')
                return False
            return True

        GLib.timeout_add(10, ready)

    def request(self, method='OpenFile', title='choose'):
        handle = call(self.client, FRONTEND, OBJECT, CHOOSER, method,
            GLib.Variant('(ssa{sv})', ('', title, {}))).unpack()[0]
        if title == 'busy':
            wait(lambda: bool(self.previous.pending))
            return handle
        wait(lambda: handle in self.responses)
        code, results = self.responses.pop(handle)
        assert code == 0, (code, results)
        return results['uris'][0]

    def apply(self, enabled, expected=0):
        process = subprocess.Popen([str(self.binary), '--enable-file-picker' if enabled else '--disable-file-picker'],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            wait(lambda: process.poll() is not None, seconds=15)
            stdout, stderr = process.communicate(timeout=1)
            assert process.returncode == expected, (stdout, stderr)
            assert 'sign out' not in stdout.lower() and 'log out' not in stdout.lower()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def session(root, binary):
    desktop = Desktop(root, binary)
    preference = Path(os.environ['XDG_CONFIG_HOME']) / 'xdg-desktop-portal/filescopetest-portals.conf'
    original = preference.read_bytes()
    try:
        desktop.start()
        wait(lambda: desktop.owner() is not None)
        assert desktop.request() == desktop.previous.uri
        caller = desktop.client.get_unique_name()
        owner = desktop.owner()
        busy = desktop.request(title='busy')
        desktop.apply(True, expected=1)
        assert preference.read_bytes() == original and desktop.owner() == owner
        assert desktop.restarts == 0, 'An active dialog must not be restarted'
        call(desktop.client, FRONTEND, busy, 'org.freedesktop.portal.Request', 'Close', GLib.Variant('()', ()))
        wait(lambda: not desktop.previous.pending)
        wait(lambda: not ET.fromstring(call(desktop.client, FRONTEND, OBJECT,
            'org.freedesktop.DBus.Introspectable', 'Introspect', None).unpack()[0]).findall("node[@name='request']"))
        desktop.break_next_restart = True
        desktop.apply(True, expected=1)
        assert preference.read_bytes() == original
        assert desktop.restarts == 2, 'Failure must also reload the restored preference'
        assert desktop.request() == desktop.previous.uri, 'The previous live chooser was restored'
        owner = desktop.owner()
        desktop.apply(True)
        assert desktop.owner() != owner and desktop.restarts == 3
        assert desktop.request() == desktop.filescope.uri
        assert desktop.request('SaveFile') == desktop.filescope.uri
        desktop.apply(False)
        assert desktop.restarts == 4 and preference.read_bytes() == original
        assert desktop.request() == desktop.previous.uri
        assert desktop.request('SaveFile') == desktop.previous.uri
        disabled = '[preferred]\ndefault=none\norg.freedesktop.impl.portal.FileChooser=none\n'
        preference.write_text(disabled)
        desktop.apply(True)
        assert desktop.request() == desktop.filescope.uri
        desktop.apply(False)
        assert preference.read_text() == disabled
        xml = call(desktop.client, FRONTEND, OBJECT,
                   'org.freedesktop.DBus.Introspectable', 'Introspect', None).unpack()[0]
        assert not ET.fromstring(xml).findall(f"interface[@name='{CHOOSER}']")
        assert desktop.client.get_unique_name() == caller, 'The requesting application stayed connected'
        print('PASS: live Open/Save switching, busy protection, failed-restart recovery, and disabled-chooser restoration; no logout.')
    finally:
        desktop.stop()
        desktop.log.close()


def main():
    if len(sys.argv) == 4 and sys.argv[1] == 'session':
        session(Path(sys.argv[2]), Path(sys.argv[3]))
        return
    assert len(sys.argv) == 2, 'Pass the built Filescope binary'
    binary = Path(sys.argv[1]).resolve()
    assert binary.is_file(), 'Build Filescope first'
    with tempfile.TemporaryDirectory(prefix='filescope-picker-live-reload-') as directory:
        root = Path(directory)
        env = os.environ.copy()
        for kind in ['CONFIG', 'DATA', 'CACHE', 'STATE', 'RUNTIME']:
            path = root / kind.lower()
            path.mkdir(mode=0o700)
            env['XDG_' + kind + ('_DIR' if kind == 'RUNTIME' else '_HOME')] = str(path)
        env['XDG_CURRENT_DESKTOP'] = 'FilescopeTest'
        portals = root / 'config/xdg-desktop-portal'
        portals.mkdir()
        env['XDG_DESKTOP_PORTAL_DIR'] = str(portals)
        for name, backend in [('gtk', 'gtk'), ('zz-filescope', 'filescope')]:
            (portals / (name + '.portal')).write_text('[portal]\nDBusName=org.freedesktop.impl.portal.desktop.' + backend + '\nInterfaces=' + IMPL + ';\n')
        (portals / 'filescopetest-portals.conf').write_text('[preferred]\ndefault=none\norg.freedesktop.impl.portal.FileChooser=gtk;\n')
        services = root / 'data/dbus-1/services'
        services.mkdir(parents=True)
        (services / 'org.freedesktop.impl.portal.desktop.filescope.service').write_text('[D-BUS Service]\nName=org.freedesktop.impl.portal.desktop.filescope\nExec="' + str(binary) + '" --file-picker-portal\n')
        tools = root / 'bin'
        tools.mkdir()
        stub = tools / 'systemctl'
        stub.write_text('#!/bin/sh\n[ "$*" = "--user --no-ask-password restart xdg-desktop-portal.service" ] || exit 64\nexec /usr/bin/gdbus call --session --dest ' + CONTROL + ' --object-path /dev/filescope/ReloadTest --method ' + CONTROL + '.Restart\n')
        stub.chmod(0o755)
        env['PATH'] = str(tools) + ':' + env.get('PATH', '')
        for name in ['previous.txt', 'filescope.txt']:
            (root / name).write_text(name)
        try:
            subprocess.run(['dbus-run-session', '--', '/usr/bin/python3', str(Path(__file__).resolve()),
                            'session', str(root), str(binary)], env=env, check=True)
        finally:
            # The private document portal can leave a disconnected FUSE mount after its bus exits.
            unmount = shutil.which('fusermount3') or shutil.which('fusermount')
            if unmount:
                subprocess.run([unmount, '-u', '-z', str(root / 'runtime/doc')],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
