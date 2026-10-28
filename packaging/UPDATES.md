# In-app updates

Filescope checks the latest stable release once when its first window opens.
The menu offers **Check for Updates**, **Automatic Update Checks**, and
**Restart to Apply Update**. Downloads/installations always need approval.
Checks and installation run off the GTK thread. Automatic network errors stay
quiet; manual checks explain them. `curl` is required for HTTPS downloads;
installers also use `flock` and `sha256sum` from the usual Linux tools.

## Trust and compatibility

The trust root is the HTTPS GitHub API for `dalpat/filescope`. The updater checks
the release identity, stable numeric version, exact platform asset URL, size,
and GitHub's `sha256:` asset digest. It does not accept a checksum from an
arbitrary URL or execute a downloaded installer. This uses GitHub/TLS trust,
not a separate offline signing key. GitHub documents its digest field in the
[release asset API](https://docs.github.com/en/rest/releases/assets).

The bounded gzip/tar reader rejects absolute/traversing paths, links, device
entries, extension headers and duplicate entries. Only the binary is written
to a private staging directory; all other installation files remain intact.
The manifest, ELF architecture and `--update-probe` must match. The probe runs
without a display and with eager dynamic linking before any installed file is
replaced, checking that the new binary can load the system libraries.

Format 1 changes **only the executable**. Releases that require different
launchers, icons, D-Bus/search registration or filesystem layout must introduce
a new format or require the manual installer. Preferences and bookmarks remain
outside the install transaction.

## Installed-file contract

`update-layout` is shared by the source installer, release installer and Rust
updater. Updates are limited to the running `~/.local/bin/filescope` with a
matching, user-owned receipt written by Filescope's installer. System packages,
portable copies, symlinked executables and unregistered/modified installations
must use their installer or package manager. Existing installations gain a
receipt when reinstalled using the updated installer.

Both installers and the updater take the same `.filescope-update.lock`.
The updater verifies/stages on the destination filesystem, syncs the candidate,
keeps `.filescope-previous`, then atomically renames the new binary into place.
The receipt accepts both old and new hashes across an interruption between
receipt and binary replacement. A failed download, verification, probe or
staging write leaves the previous executable untouched. Interrupted private
staging directories may remain, but never become the installed executable.

There is no automatic restart. Restart requires confirmation and rechecks
transfers, archive jobs, batch operations, Undo/Redo and retained terminals,
including hidden shells. Open folders reopen; selection and Undo history do
not survive. Closing a window does not cancel an already-approved installation.

## Preparing a release

1. Set the intended stable Cargo version and build the release binary on the
   oldest supported runtime for the target architecture.
2. Run `python3 packaging/release.py --output /path/to/fresh-output`.
   It validates the binary's probe and emits the archive plus `.sha256` file.
   The archive includes the update manifest, shared install layout, installer,
   launcher and icon. No key or credential is needed for packaging.
3. Test the archive on supported systems, then upload both files to the matching
   `vMAJOR.MINOR.PATCH` release in `dalpat/filescope`. Publishing remains a
   separate maintainer action. GitHub provides the authenticated asset digest.
4. Verify that the GitHub API reports the expected digest and a non-draft,
   non-prerelease release. Old bundles without the manifest/probe remain usable
   with their manual installer, but cannot be installed by this updater.

## Validation

`cargo test update::tests` exercises versions, malicious archives, entry types,
ownership, atomic replacement and subprocess-isolated complete installations.
The fixtures cover offline downloads, corruption, wrong CPU, runtime rejection,
full storage and lock contention. `python3 tests/install.py` checks both
installers and unchanged desktop/search registration in a private filesystem.
The opt-in GTK test in `src/window/updates.rs` checks the real menu preference,
hidden terminal, transfer/history/batch gates and the final confirmation check.

A public newer-format release still needs an end-to-end acceptance run before
shipping: update an isolated older installation, verify its preferences and
launcher/search behavior, and repeat offline and with interrupted/failed staging.
