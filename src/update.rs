//! Release checks and atomic per-user binary updates. No GTK work happens here.
use gtk::glib;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const REPOSITORY: &str = "https://github.com/dalpat/filescope";
const API: &str = "https://api.github.com/repos/dalpat/filescope/releases/latest";
const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
const MAX_BINARY: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub notes: String,
    url: String,
    digest: String,
    size: u64,
}

fn version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.strip_prefix('v').unwrap_or(value).split('.');
    let mut result = [0; 3];
    for slot in &mut result {
        let part = parts.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(result)
}

fn release_from_json(bytes: &[u8], current: &str, arch: &str) -> Result<Option<Release>> {
    let r: serde_json::Value = serde_json::from_slice(bytes)?;
    if r["draft"].as_bool().ok_or("Missing draft status")?
        || r["prerelease"]
            .as_bool()
            .ok_or("Missing prerelease status")?
    {
        return Ok(None);
    }
    let tag = r["tag_name"].as_str().ok_or("Missing release tag")?;
    let Some(next) = version(tag) else {
        return Ok(None);
    };
    if next <= version(current).ok_or("Unknown installed version")? {
        return Ok(None);
    }
    if r["html_url"].as_str() != Some(format!("{REPOSITORY}/releases/tag/{tag}").as_str()) {
        return Err("Untrusted release identity".into());
    }
    let name = format!("filescope-{tag}-{arch}-linux.tar.gz");
    let assets = r["assets"].as_array().ok_or("Missing release assets")?;
    let matching: Vec<_> = assets
        .iter()
        .filter(|a| a["name"].as_str() == Some(&name))
        .collect();
    if matching.len() != 1 {
        return Err(format!("No unique {arch} Linux update is available for this release").into());
    }
    let a = matching[0];
    let url = format!("{REPOSITORY}/releases/download/{tag}/{name}");
    if a["browser_download_url"].as_str() != Some(&url) || a["state"].as_str() != Some("uploaded") {
        return Err("Untrusted release artifact".into());
    }
    let digest = a["digest"]
        .as_str()
        .and_then(|v| v.strip_prefix("sha256:"))
        .filter(|v| valid_digest(v))
        .ok_or("Release has no verifiable SHA-256 digest")?;
    let size = a["size"]
        .as_u64()
        .filter(|s| *s > 0 && *s <= MAX_ARCHIVE)
        .ok_or("Invalid release size")?;
    Ok(Some(Release {
        version: tag.trim_start_matches('v').into(),
        notes: r["body"]
            .as_str()
            .unwrap_or("No release notes provided.")
            .into(),
        url,
        digest: digest.into(),
        size,
    }))
}

fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn fetch(url: &str, limit: u64, redirects: bool) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    let mut command = Command::new("curl");
    // Ignore user curlrc: redirects, TLS checks and output must follow this contract.
    command.args([
        "-q",
        "--fail",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "10",
        "--max-time",
        "120",
        "--max-redirs",
        "5",
        "--user-agent",
        "filescope-updater",
        "--header",
        "Accept: application/vnd.github+json",
    ]);
    if redirects {
        command.arg("--location");
    }
    let mut child = command
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Couldn't start curl: {e}"))?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .unwrap()
        .take(limit + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > limit {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Download failed or exceeded the allowed size".into());
    }
    if !child.wait()?.success() {
        return Err("Couldn't download the update. Check your connection and try again.".into());
    }
    Ok(bytes)
}

pub fn check() -> Result<Option<Release>> {
    release_from_json(
        &fetch(API, 1024 * 1024, false)?,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
    )
}

fn unpack(bytes: &[u8], release: &Release, arch: &str, output: &Path) -> Result<()> {
    use std::{collections::HashSet, os::unix::fs::PermissionsExt};
    if bytes.len() as u64 != release.size || digest(bytes) != release.digest {
        return Err("The downloaded update failed its SHA-256 check".into());
    }
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let root = format!("filescope-v{}-{arch}-linux", release.version);
    let mut names = HashSet::new();
    let mut binary = None;
    let mut manifest = None;
    let mut total = 0u64;
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("Unsafe path in update archive".into());
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) || !names.insert(path.clone()) || names.len() > 32 {
            return Err("Unsupported or duplicate archive entry".into());
        }
        total = total
            .checked_add(entry.size())
            .filter(|n| *n <= MAX_BINARY)
            .ok_or("Update expands beyond the size limit")?;
        let relative = path.strip_prefix(&root).unwrap_or(&path);
        if kind.is_dir() {
            continue;
        }
        match relative.to_str() {
            Some("filescope") => {
                if binary.is_some() {
                    return Err("Duplicate update binary".into());
                }
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                binary = Some(data);
            }
            Some("update-manifest") => {
                if manifest.is_some() || entry.size() > 1024 {
                    return Err("Invalid update manifest".into());
                }
                let mut data = String::new();
                entry.read_to_string(&mut data)?;
                manifest = Some(data);
            }
            _ => {} // The updater never extracts or executes bundled installers or integration files.
        }
    }
    let expected = format!("filescope-update-v1\n{}\n{arch}\n", release.version);
    if manifest.as_deref() != Some(&expected) {
        return Err("This release is incompatible with the update format or architecture".into());
    }
    let binary = binary
        .filter(|b| !b.is_empty())
        .ok_or("The archive has no executable")?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    file.write_all(&binary)?;
    file.set_permissions(fs::Permissions::from_mode(0o755))?;
    file.sync_all()?;
    Ok(())
}

fn layout(key: &str) -> &'static str {
    include_str!("../packaging/update-layout")
        .lines()
        .find_map(|line| {
            line.split_once('=')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v)
        })
        .expect("compiled install layout")
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = glib::Checksum::new(glib::ChecksumType::Sha256).ok_or("SHA-256 unavailable")?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.string().ok_or("SHA-256 unavailable")?.to_string())
}

fn user_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o6022 != 0 {
        return Err("Installation files must be owned by you and not writable by others".into());
    }
    Ok(())
}

pub fn installed_binary() -> Result<PathBuf> {
    let target = glib::home_dir()
        .join(".local/bin")
        .join(layout("UPDATE_BINARY"));
    user_file(&target)?;
    let installed = target.canonicalize()?;
    if std::env::current_exe()? != installed {
        return Err("This copy is not the per-user Filescope installation. Update it using its installer or package manager.".into());
    }
    let receipt = target.with_file_name(layout("UPDATE_RECEIPT"));
    user_file(&receipt).map_err(|_| "This installation is not registered for in-app updates. Reinstall with Filescope’s installer or use its package manager.")?;
    let text = fs::read_to_string(receipt)?;
    let hash = hash_file(&target)?;
    if !text.starts_with("filescope-user-install-v1\n") || !text.lines().skip(1).any(|l| l == hash)
    {
        return Err("This installation is not registered for in-app updates. Reinstall with Filescope's installer first.".into());
    }
    Ok(target)
}

struct Stage(PathBuf);
impl Stage {
    fn new(parent: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        for _ in 0..10 {
            let p = parent.join(format!(".filescope-update-{}", glib::uuid_string_random()));
            match fs::DirBuilder::new().mode(0o700).create(&p) {
                Ok(()) => return Ok(Self(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err("Couldn't create an update staging directory".into())
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn replace_binary(binary: &Path, candidate: &Path, expected: &str) -> Result<()> {
    user_file(binary)?;
    user_file(candidate)?;
    if hash_file(binary)? != expected {
        return Err("The installed binary changed during the update. Check again.".into());
    }
    let parent = binary.parent().ok_or("No installation directory")?;
    let stage = Stage::new(parent)?;
    fs::hard_link(binary, stage.0.join("previous"))?;
    fs::rename(
        stage.0.join("previous"),
        parent.join(layout("UPDATE_BACKUP")),
    )?;
    // Both hashes remain valid across interruption between receipt and binary replacement.
    let receipt = stage.0.join("receipt");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&receipt)?;
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    file.write_all(
        format!(
            "filescope-user-install-v1\n{expected}\n{}\n",
            hash_file(candidate)?
        )
        .as_bytes(),
    )?;
    file.sync_all()?;
    fs::rename(receipt, parent.join(layout("UPDATE_RECEIPT")))?;
    fs::File::open(parent)?.sync_all()?;
    fs::rename(candidate, binary)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn verify_elf(candidate: &Path) -> Result<()> {
    let mut header = [0u8; 20];
    fs::File::open(candidate)?.read_exact(&mut header)?;
    let machine = match std::env::consts::ARCH {
        "x86_64" => 62,
        "aarch64" => 183,
        _ => return Err("No updater support for this architecture".into()),
    };
    if &header[..4] != b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || !matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3)
        || u16::from_le_bytes([header[18], header[19]]) != machine
    {
        return Err("The release binary has the wrong format or architecture".into());
    }
    Ok(())
}

fn probe(candidate: &Path, version: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    verify_elf(candidate)?;
    let expected = format!(
        "filescope-update-v1\n{version}\n{}\n",
        std::env::consts::ARCH
    );
    // Only a checksum-verified release is executed; eager linking checks runtime dependencies.
    let mut child = Command::new(candidate)
        .arg("--update-probe")
        .env("LD_BIND_NOW", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut output = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .take(1024)
                .read_to_string(&mut output)?;
            if status.success() && output == expected {
                return Ok(());
            }
            return Err("The new release cannot run with this system's libraries. The installed version was kept.".into());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("The new release's compatibility check timed out".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[derive(Clone)]
pub struct Installed {
    pub(crate) path: PathBuf,
    pub(crate) digest: String,
}

pub fn install(release: &Release) -> Result<Installed> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let binary = installed_binary()?;
    let parent = binary.parent().ok_or("No installation directory")?;
    let lock_path = parent.join(layout("UPDATE_LOCK"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another install or update is already running".into());
    }
    installed_binary()?;
    let expected = hash_file(&binary)?;
    let stage = Stage::new(parent)?;
    let candidate = stage.0.join("filescope");
    let bytes = fetch(&release.url, release.size, true)?;
    unpack(&bytes, release, std::env::consts::ARCH, &candidate)?;
    probe(&candidate, &release.version)?;
    let next_hash = hash_file(&candidate)?;
    replace_binary(&binary, &candidate, &expected)?;
    Ok(Installed {
        path: binary,
        digest: next_hash,
    })
}

pub fn restart_blocked(operations: bool, terminals: bool) -> Option<&'static str> {
    if operations {
        Some("Restart is waiting for file operations to finish.")
    } else if terminals {
        Some(
            "Restart is waiting for terminal sessions. Exit each shell, including hidden terminals, or close its tab.",
        )
    } else {
        None
    }
}

/// Called before GTK initialization: this must also work without a display.
pub fn print_probe() -> bool {
    // The generated high-level version getters assert GTK initialization unnecessarily.
    let compatible = unsafe {
        (
            gtk::ffi::gtk_get_major_version(),
            gtk::ffi::gtk_get_minor_version(),
        ) >= (4, 16)
            && (
                adw::ffi::adw_get_major_version(),
                adw::ffi::adw_get_minor_version(),
            ) >= (1, 5)
            && (
                vte::ffi::vte_get_major_version(),
                vte::ffi::vte_get_minor_version(),
            ) >= (0, 70)
    };
    if compatible {
        println!(
            "filescope-update-v1\n{}\n{}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH
        );
    }
    compatible
}

fn digest(bytes: &[u8]) -> String {
    glib::compute_checksum_for_data(glib::ChecksumType::Sha256, bytes)
        .unwrap()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn metadata(version: &str) -> serde_json::Value {
        let tag = format!("v{version}");
        let name = format!("filescope-{tag}-x86_64-linux.tar.gz");
        json!({"tag_name":tag,"draft":false,"prerelease":false,
            "html_url":format!("{REPOSITORY}/releases/tag/{tag}"), "body":"Release notes",
            "assets":[{"name":name,"state":"uploaded","size":100,
                "browser_download_url":format!("{REPOSITORY}/releases/download/{tag}/{name}"),
                "digest":format!("sha256:{}", "a".repeat(64))}]})
    }
    fn parse(value: &serde_json::Value) -> Result<Option<Release>> {
        release_from_json(&serde_json::to_vec(value).unwrap(), "0.6.0", "x86_64")
    }
    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "filescope-update-test-{}",
            glib::uuid_string_random()
        ));
        fs::create_dir(&p).unwrap();
        p
    }
    fn bundle(path: &str, kind: tar::EntryType) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for (name, bytes, ty) in [
            (
                "update-manifest",
                b"filescope-update-v1\n0.7.0\nx86_64\n".as_slice(),
                tar::EntryType::Regular,
            ),
            (path, b"binary".as_slice(), kind),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o755);
            h.set_entry_type(ty);
            // Raw headers exercise traversal paths rejected by tar's writer API.
            h.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            tar.append(&h, bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }
    fn fixture(bytes: &[u8]) -> Release {
        let mut r = metadata("0.7.0");
        r["assets"][0]["digest"] = json!(format!("sha256:{}", digest(bytes)));
        r["assets"][0]["size"] = json!(bytes.len());
        parse(&r).unwrap().unwrap()
    }
    #[test]
    fn update_requires_a_newer_stable_version() {
        for v in ["0.6.0", "0.5.9", "0.7.0-rc1", "01.0.0", "0.7", "../0.7.0"] {
            assert!(parse(&metadata(v)).ok().flatten().is_none(), "{v}");
        }
        for key in ["draft", "prerelease"] {
            let mut r = metadata("0.7.0");
            r[key] = json!(true);
            assert!(parse(&r).unwrap().is_none());
        }
        assert_eq!(parse(&metadata("0.7.0")).unwrap().unwrap().version, "0.7.0");
        assert!(parse(&metadata("0.10.0")).unwrap().is_some());
    }
    #[test]
    fn update_rejects_invalid_artifacts() {
        let dir = tmp();
        let out = dir.join("candidate");
        for (path, kind) in [
            ("../escape", tar::EntryType::Regular),
            ("/escape", tar::EntryType::Regular),
            ("filescope", tar::EntryType::Symlink),
            ("filescope", tar::EntryType::Link),
        ] {
            let bytes = bundle(path, kind);
            assert!(unpack(&bytes, &fixture(&bytes), "x86_64", &out).is_err());
            let _ = fs::remove_file(&out);
        }
        let bytes = bundle("filescope", tar::EntryType::Regular);
        let r = fixture(&bytes);
        assert!(unpack(&bytes, &r, "aarch64", &out).is_err());
        assert!(unpack(b"altered", &r, "x86_64", &out).is_err());
        let mut bad = metadata("0.7.0");
        bad["assets"][0]["browser_download_url"] = json!("https://attacker.invalid/update");
        assert!(parse(&bad).is_err());
        bad = metadata("0.7.0");
        bad["assets"][0]["digest"] = json!(null);
        assert!(parse(&bad).is_err());
        unpack(&bytes, &r, "x86_64", &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"binary");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_update_preserves_previous_install() {
        let dir = tmp();
        let binary = dir.join("filescope");
        let candidate = dir.join("candidate");
        fs::write(&binary, b"old").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&candidate, b"new").unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(replace_binary(&binary, &dir.join("missing"), &digest(b"old")).is_err());
        assert_eq!(fs::read(&binary).unwrap(), b"old");
        assert!(replace_binary(&binary, &candidate, &digest(b"different install")).is_err());
        assert_eq!(fs::read(&binary).unwrap(), b"old");
        replace_binary(&binary, &candidate, &digest(b"old")).unwrap();
        assert_eq!(fs::read(&binary).unwrap(), b"new");
        assert_eq!(fs::read(dir.join(".filescope-previous")).unwrap(), b"old");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn isolated_update_preserves_installation_and_rejects_failures() {
        use std::process::Command;
        if let Ok(mode) = std::env::var("FILESCOPE_UPDATE_CHILD") {
            let binary = installed_binary().unwrap();
            let before = hash_file(&binary).unwrap();
            let release = release_from_json(
                &fs::read(std::env::var_os("FILESCOPE_UPDATE_METADATA").unwrap()).unwrap(),
                "0.6.0",
                "x86_64",
            )
            .unwrap()
            .unwrap();
            if mode == "full" {
                unsafe {
                    libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                    let limit = libc::rlimit {
                        rlim_cur: 1,
                        rlim_max: 1,
                    };
                    assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
                }
            }
            let lock = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(binary.with_file_name(".filescope-update.lock"))
                .unwrap();
            if mode == "locked" {
                use std::os::fd::AsRawFd;
                assert_eq!(
                    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
            }
            let result = install(&release);
            if mode == "success" {
                let installed = result.unwrap();
                assert_eq!(installed.path, binary);
                assert_ne!(hash_file(&binary).unwrap(), before);
                assert_eq!(
                    hash_file(&binary.with_file_name(".filescope-previous")).unwrap(),
                    before
                );
                let receipt =
                    fs::read_to_string(binary.with_file_name(".filescope-install")).unwrap();
                assert!(receipt.contains(&installed.digest));
                user_file(&binary.with_file_name(".filescope-install")).unwrap();
            } else {
                assert!(result.is_err(), "{mode}");
                assert_eq!(hash_file(&binary).unwrap(), before, "{mode}");
            }
            assert_eq!(
                fs::read_to_string(glib::home_dir().join("settings")).unwrap(),
                "preferences"
            );
            assert_eq!(
                fs::read_to_string(glib::home_dir().join("launcher")).unwrap(),
                "launcher and search registration"
            );
            return;
        }
        let build = tmp();
        let source = build.join("probe.rs");
        let executable = build.join("probe");
        fs::write(&source, "fn main() { if std::env::var(\"FILESCOPE_UPDATE_CHILD\").as_deref() == Ok(\"runtime\") { std::process::exit(1); } print!(\"filescope-update-v1\\n0.7.0\\nx86_64\\n\"); }\n").unwrap();
        assert!(
            Command::new("rustc")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        let executable = fs::read(&executable).unwrap();
        for mode in [
            "success",
            "offline",
            "altered",
            "incompatible",
            "runtime",
            "full",
            "locked",
            "interrupt",
        ] {
            let dir = tmp();
            let bin = dir.join(".local/bin");
            fs::create_dir_all(&bin).unwrap();
            let exe = bin.join("filescope");
            fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
            let receipt = bin.join(".filescope-install");
            fs::write(
                &receipt,
                format!("filescope-user-install-v1\n{}\n", hash_file(&exe).unwrap()),
            )
            .unwrap();
            fs::set_permissions(receipt, fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(dir.join("settings"), "preferences").unwrap();
            fs::write(dir.join("launcher"), "launcher and search registration").unwrap();
            let tools = dir.join("tools");
            fs::create_dir(&tools).unwrap();
            let curl = tools.join("curl");
            fs::write(&curl,"#!/usr/bin/python3\nimport os,sys,time\nif os.environ['FILESCOPE_UPDATE_CHILD']=='interrupt':\n open(os.environ['FILESCOPE_UPDATE_STARTED'],'w').write('started')\n time.sleep(60)\nif os.environ['FILESCOPE_UPDATE_CHILD']=='offline': sys.exit(22)\nassert sys.argv[1]=='-q'\nassert sys.argv[-1].startswith('https://github.com/dalpat/filescope/releases/download/v0.7.0/')\nsys.stdout.buffer.write(open(os.environ['FILESCOPE_UPDATE_ARCHIVE'],'rb').read())\n").unwrap();
            fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            ));
            let mut candidate = executable.clone();
            if mode == "incompatible" {
                candidate[18] = 0;
                candidate[19] = 0;
            }
            for (name, bytes) in [
                ("filescope", candidate.as_slice()),
                (
                    "update-manifest",
                    b"filescope-update-v1\n0.7.0\nx86_64\n".as_slice(),
                ),
            ] {
                let mut header = tar::Header::new_ustar();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, name, bytes).unwrap();
            }
            let bytes = tar.into_inner().unwrap().finish().unwrap();
            let mut json = metadata("0.7.0");
            json["assets"][0]["digest"] = json!(format!("sha256:{}", digest(&bytes)));
            json["assets"][0]["size"] = json!(bytes.len());
            let meta = dir.join("release.json");
            fs::write(&meta, serde_json::to_vec(&json).unwrap()).unwrap();
            let archive = dir.join("release.tar.gz");
            fs::write(
                &archive,
                if mode == "altered" {
                    b"altered".as_slice()
                } else {
                    bytes.as_slice()
                },
            )
            .unwrap();
            let started = dir.join("started");
            let mut command = Command::new(&exe);
            command
                .args([
                    "--exact",
                    "update::tests::isolated_update_preserves_installation_and_rejects_failures",
                    "--nocapture",
                ])
                .env("HOME", &dir)
                .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
                .env("FILESCOPE_UPDATE_CHILD", mode)
                .env("FILESCOPE_UPDATE_METADATA", meta)
                .env("FILESCOPE_UPDATE_ARCHIVE", archive)
                .env("FILESCOPE_UPDATE_STARTED", &started);
            if mode == "interrupt" {
                use std::os::unix::process::CommandExt;
                let before = hash_file(&exe).unwrap();
                let mut child = command
                    .process_group(0)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !started.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                child.wait().unwrap();
                assert!(started.exists(), "worker did not reach download");
                assert_eq!(
                    hash_file(&exe).unwrap(),
                    before,
                    "interrupted update changed the installation"
                );
            } else {
                let result = command.output().unwrap();
                assert!(
                    result.status.success(),
                    "{mode}: {} {}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            fs::remove_dir_all(dir).unwrap();
        }
        fs::remove_dir_all(build).unwrap();
    }

    #[test]
    fn update_rejects_duplicate_entries_and_wrong_manifest() {
        let dir = tmp();
        let output = dir.join("candidate");
        let bytes = bundle("update-manifest", tar::EntryType::Regular);
        assert!(unpack(&bytes, &fixture(&bytes), "x86_64", &output).is_err());
        let bytes = bundle("filescope", tar::EntryType::Regular);
        let mut release = fixture(&bytes);
        release.version = "0.8.0".into();
        assert!(unpack(&bytes, &release, "x86_64", &output).is_err());
        assert!(!output.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unmanaged_and_symlinked_files_are_rejected() {
        let dir = tmp();
        let file = dir.join("file");
        fs::write(&file, b"binary").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(user_file(&file).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(user_file(&file).is_ok());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(user_file(&link).is_err());
        assert!(
            installed_binary().is_err(),
            "the test runner must never qualify as the installed app"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "requires FILESCOPE_UPDATE_BUNDLE from packaging/release.py"]
    fn packaged_release_runs_before_replacing_an_isolated_installation() {
        let bytes =
            fs::read(std::env::var_os("FILESCOPE_UPDATE_BUNDLE").expect("bundle path")).unwrap();
        let release = Release {
            version: env!("CARGO_PKG_VERSION").into(),
            notes: String::new(),
            url: String::new(),
            digest: digest(&bytes),
            size: bytes.len() as u64,
        };
        let dir = tmp();
        let candidate = dir.join("candidate");
        let installed = dir.join("filescope");
        fs::write(&installed, b"previous version").unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        unpack(&bytes, &release, std::env::consts::ARCH, &candidate).unwrap();
        probe(&candidate, &release.version).unwrap();
        replace_binary(&installed, &candidate, &digest(b"previous version")).unwrap();
        probe(&installed, &release.version).unwrap();
        assert_eq!(
            fs::read(dir.join(".filescope-previous")).unwrap(),
            b"previous version"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "contacts the public GitHub release API"]
    fn update_check_uses_live_release_metadata() {
        assert!(check().is_ok());
    }

    #[test]
    fn restart_waits_for_active_work() {
        assert!(restart_blocked(true, false).is_some());
        assert!(restart_blocked(false, true).is_some());
        assert!(restart_blocked(true, true).is_some());
        assert!(restart_blocked(false, false).is_none());
    }
}

/// Replace this process only after the UI has rechecked that no work is active.
pub fn restart(installed: &Installed, folders: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    user_file(&installed.path)?;
    if hash_file(&installed.path)? != installed.digest {
        return Err("The installed version changed. Close and reopen Filescope normally.".into());
    }
    Err(std::process::Command::new(&installed.path)
        .args(folders)
        .exec()
        .into())
}
