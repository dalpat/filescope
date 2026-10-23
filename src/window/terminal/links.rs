//! Terminal text is data: validate targets without interpreting shell commands.
use std::cell::RefCell;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use vte::prelude::*;

use gtk::{gdk, gio, glib};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Web(String),
    Path(PathBuf),
}

fn file_path(uri: &str, local_host: &str) -> Result<PathBuf, String> {
    let parsed = glib::Uri::parse(uri, glib::UriFlags::NONE).map_err(|_| "Invalid file link")?;
    if !parsed.scheme().eq_ignore_ascii_case("file")
        || parsed.userinfo().is_some()
        || parsed.port() != -1
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Unsupported file link".into());
    }
    if let Some(host) = parsed.host().filter(|host| !host.is_empty()) {
        if !host.eq_ignore_ascii_case("localhost") && !host.eq_ignore_ascii_case(local_host) {
            return Err("Remote file paths cannot be opened as local files".into());
        }
    }
    let (path, _) = glib::filename_from_uri(uri).map_err(|_| "Invalid file link")?;
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
    {
        return Err("Unsupported filename encoding".into());
    }
    Ok(path)
}

fn local_directory(
    reported_uri: Option<&str>,
    process_cwd: Option<&Path>,
    verified_shell: bool,
    local_host: &str,
) -> Result<PathBuf, String> {
    if !verified_shell {
        return Err("Local paths are disabled while another program owns the terminal".into());
    }
    if let Some(uri) = reported_uri {
        file_path(uri, local_host)?;
    }
    process_cwd
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .ok_or_else(|| "The shell's current directory is unknown".into())
}

fn literal(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let syntax = raw.starts_with(['\'', '"']);
    let mut quote = None;
    let mut result = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if syntax && matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
                continue;
            }
            if quote.is_none() {
                quote = Some(ch);
                continue;
            }
        }
        if ch == '\\' && quote != Some('\'') {
            let next = chars.next().ok_or("Unfinished path escape")?;
            if quote == Some('"') && !matches!(next, '\\' | '"' | '$' | '`') {
                result.push('\\');
            }
            result.push(next);
        } else {
            result.push(ch);
        }
    }
    if quote.is_some() {
        return Err("Unclosed path quote".into());
    }
    Ok(result)
}

fn is_uri(text: &str) -> bool {
    glib::Uri::parse_scheme(text).is_some_and(|scheme| text[scheme.len()..].starts_with("://"))
}

fn resolve_uri(raw: &str, cwd: Result<&Path, &str>, local_host: &str) -> Result<Target, String> {
    if raw.chars().any(char::is_control) {
        return Err("Control characters are not allowed in a target".into());
    }
    let scheme = glib::Uri::parse_scheme(raw).ok_or("Invalid link")?;
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        let uri = glib::Uri::parse(raw, glib::UriFlags::NONE).map_err(|_| "Invalid web link")?;
        if uri.host().is_none_or(|host| host.is_empty()) || raw.chars().any(char::is_whitespace) {
            return Err("Invalid web link".into());
        }
        return Ok(Target::Web(raw.into()));
    }
    if scheme.eq_ignore_ascii_case("file") {
        cwd.map_err(str::to_owned)?;
        return file_path(raw, local_host).map(Target::Path);
    }
    Err(format!("Unsupported link scheme: {scheme}"))
}

fn resolve(
    raw: &str,
    cwd: Result<&Path, &str>,
    home: Option<&Path>,
    local_host: &str,
) -> Result<Target, String> {
    if raw.chars().any(char::is_control) {
        return Err("Control characters are not allowed in a target".into());
    }
    let text = literal(raw)?;
    if text.is_empty() {
        return Err("Empty target".into());
    }
    if is_uri(&text) {
        return resolve_uri(&text, cwd, local_host);
    }
    let cwd = cwd.map_err(str::to_owned)?;
    let expand_home = raw.trim_start().starts_with('~');
    let path = if expand_home && text == "~" {
        home.ok_or("The shell's home directory is unknown")?
            .to_path_buf()
    } else if let Some(relative) = text.strip_prefix("~/").filter(|_| expand_home) {
        home.ok_or("The shell's home directory is unknown")?
            .join(relative)
    } else if expand_home {
        return Err("Only the current user's ~ path is supported".into());
    } else {
        cwd.join(text)
    };
    if path
        .to_str()
        .is_none_or(|s| s.chars().any(char::is_control))
    {
        return Err("Unsupported filename encoding".into());
    }
    Ok(Target::Path(path))
}

fn should_open(button: u32, presses: i32, modifiers: gdk::ModifierType) -> bool {
    button == gdk::BUTTON_PRIMARY
        && presses == 1
        && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
        && !modifiers.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn terminal_web_link_opens_browser() {
        for uri in [
            "https://example.org/a?one=1&two=2",
            "http://localhost:8080/日本語",
        ] {
            assert_eq!(
                resolve(uri, Err("unknown shell"), None, "my-host"),
                Ok(Target::Web(uri.into()))
            );
        }
        for uri in [
            "javascript:alert(1)",
            "data:text/html,hello",
            "https:///missing-host",
            "https://example.org/\ncommand",
        ] {
            assert!(resolve_uri(uri, Ok(Path::new("/shell")), "my-host").is_err());
        }
    }

    #[test]
    fn terminal_compound_quotes_and_literal_filename_syntax() {
        for (raw, name) in [
            (r#"'John'\''s "report".txt'"#, "John's \"report\".txt"),
            ("notes:2026.txt", "notes:2026.txt"),
            ("http:notes.txt", "http:notes.txt"),
            ("file:notes.txt", "file:notes.txt"),
            (r#"""'My folder'"#, "My folder"),
            (r#""a\"b"'c'\ d"#, "a\"bc d"),
            ("'~'", "~"),
            ("'~backup'", "~backup"),
            (r"\~", "~"),
            ("'~/Documents'", "~/Documents"),
        ] {
            assert_eq!(
                resolve(
                    raw,
                    Ok(Path::new("/shell")),
                    Some(Path::new("/home/user")),
                    "my-host"
                ),
                Ok(Target::Path(Path::new("/shell").join(name))),
                "{raw}"
            );
        }
    }

    #[test]
    fn terminal_path_uses_shell_directory() {
        let cwd =
            local_directory(None, Some(Path::new("/shell/current")), true, "my-host").unwrap();
        assert_eq!(
            resolve("child/file.txt", Ok(&cwd), None, "my-host"),
            Ok(Target::Path("/shell/current/child/file.txt".into()))
        );
        assert_eq!(
            resolve("/absolute/folder", Ok(&cwd), None, "my-host"),
            Ok(Target::Path("/absolute/folder".into()))
        );
        assert_eq!(
            resolve(
                "~/Documents",
                Ok(&cwd),
                Some(Path::new("/user/home")),
                "my-host"
            ),
            Ok(Target::Path("/user/home/Documents".into()))
        );
    }

    #[test]
    fn terminal_target_preserves_spaces_and_unicode() {
        for (raw, expected) in [
            ("'/tmp/My folder/日本語.txt'", "/tmp/My folder/日本語.txt"),
            ("\"/tmp/My folder/日本語.txt\"", "/tmp/My folder/日本語.txt"),
            (r"/tmp/My\ folder/日本語.txt", "/tmp/My folder/日本語.txt"),
            (
                "file:///tmp/My%20folder/%E6%97%A5%E6%9C%AC%E8%AA%9E.txt",
                "/tmp/My folder/日本語.txt",
            ),
            (
                "file://my-host/tmp/literal%2520name.txt",
                "/tmp/literal%20name.txt",
            ),
            ("file://localhost/tmp/has%23hash.txt", "/tmp/has#hash.txt"),
            (
                "'/tmp/$(touch marker); echo nope'",
                "/tmp/$(touch marker); echo nope",
            ),
        ] {
            assert_eq!(
                resolve(raw, Ok(Path::new("/shell")), None, "my-host"),
                Ok(Target::Path(expected.into())),
                "{raw}"
            );
        }
    }

    #[test]
    fn terminal_unknown_or_remote_path_is_not_local() {
        assert!(local_directory(None, None, true, "my-host").is_err());
        assert!(local_directory(None, Some(Path::new("/shell")), false, "my-host").is_err());
        assert!(
            local_directory(
                Some("file://remote-host/etc"),
                Some(Path::new("/shell")),
                true,
                "my-host"
            )
            .is_err()
        );
        for uri in [
            "file://remote-host/etc/passwd",
            "sftp://remote-host/etc/passwd",
            "file://user@localhost/tmp/x",
            "file:///tmp/x?query",
            "file:///tmp/x#fragment",
            "file:///tmp/%00",
            "file:///tmp/%FF",
        ] {
            assert!(
                resolve(uri, Ok(Path::new("/shell")), None, "my-host").is_err(),
                "{uri}"
            );
        }
        for path in ["relative/path", "/etc/passwd", "file:///etc/passwd"] {
            assert!(resolve(path, Err("remote or unknown shell"), None, "my-host").is_err());
        }
        assert_eq!(
            local_directory(
                Some("file://my-host/stale/path"),
                Some(Path::new("/actual/shell")),
                true,
                "my-host"
            ),
            Ok("/actual/shell".into())
        );
    }

    #[test]
    fn plain_terminal_click_does_not_open() {
        use gtk::gdk::{BUTTON_PRIMARY, BUTTON_SECONDARY, ModifierType as M};
        assert!(!should_open(BUTTON_PRIMARY, 1, M::empty()));
        assert!(!should_open(BUTTON_PRIMARY, 2, M::CONTROL_MASK));
        assert!(!should_open(BUTTON_SECONDARY, 1, M::CONTROL_MASK));
        assert!(!should_open(
            BUTTON_PRIMARY,
            1,
            M::CONTROL_MASK | M::ALT_MASK
        ));
        assert!(should_open(BUTTON_PRIMARY, 1, M::CONTROL_MASK));
    }
}

// Prefer a complete absolute path; two spaces or a tab separate ls columns.
// Keep quotes, URLs, and plain names together so an inner quote cannot steal a match.
const PATTERNS: [&str; 2] = [
    r#"^[ \t]*(?:/|~/|\./|\.\./)(?:\\[^\r\n]|[^\s\\]| [^\s])*[ \t]*$"#,
    r#""(?:\\[^\r\n]|[^"\\\r\n])+"|'(?:[^'\r\n]|'\\'')+'|(?i:[a-z][a-z0-9+.-]*://)[^\s<>"]+|(?:\\[^\r\n]|[^\s<>"|;&\\])+"#,
];

fn clean_detected(raw: &str) -> String {
    let mut text = raw.trim();
    if text
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || text
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        loop {
            let next = text.trim_end_matches(['.', ',', ';', '!']);
            let next =
                if next.ends_with(')') && next.matches(')').count() > next.matches('(').count() {
                    &next[..next.len() - 1]
                } else {
                    next
                };
            if next == text {
                break;
            }
            text = next;
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod detection_tests {
    use super::*;

    fn detected(text: &str, at: usize) -> Option<String> {
        let input = glib::GString::from(text);
        for pattern in PATTERNS {
            let regex = glib::Regex::new(
                pattern,
                glib::RegexCompileFlags::MULTILINE,
                glib::RegexMatchFlags::DEFAULT,
            )
            .unwrap()
            .unwrap();
            // glib 0.20's match_full debug binding asserts on a normal no-match.
            if !glib::Regex::match_simple(
                pattern,
                text,
                glib::RegexCompileFlags::MULTILINE,
                glib::RegexMatchFlags::DEFAULT,
            ) {
                continue;
            }
            let Some(info) = regex.match_(input.as_gstr(), glib::RegexMatchFlags::DEFAULT) else {
                continue;
            };
            loop {
                if let Some((start, end)) = info.fetch_pos(0) {
                    if start as usize <= at && at < end as usize {
                        return Some(clean_detected(&info.fetch(0).unwrap()));
                    }
                }
                if !info.next().unwrap() {
                    break;
                }
            }
        }
        None
    }

    #[test]
    fn terminal_detection_keeps_compound_quoted_ls_words() {
        let word = r#"'John'\''s "report".txt'"#;
        let line = format!(" plain.txt  {word}  notes:2026.txt");
        let start = line.find(word).unwrap();
        for at in start..start + word.len() {
            assert_eq!(detected(&line, at), Some(word.into()), "column {at}");
        }
    }

    #[test]
    fn terminal_detection_handles_plain_ls_names_and_columns() {
        for (line, target) in [
            (" plain.txt", "plain.txt"),
            (" folder", "folder"),
            ("plain.txt  folder  other.txt", "folder"),
            ("plain.txt  folder  other.txt", "other.txt"),
            (" plain.txt\t日本語.txt", "日本語.txt"),
            (" plain.txt  'My folder'  folder", "'My folder'"),
            (r"plain.txt  My\ folder.txt", r"My\ folder.txt"),
            ("/tmp/first  /tmp/second", "/tmp/first"),
            ("/tmp/first  /tmp/second", "/tmp/second"),
            ("plain.txt  it's.txt", "it's.txt"),
        ] {
            let start = line.find(target).unwrap();
            for (offset, _) in target.char_indices() {
                assert_eq!(
                    detected(line, start + offset),
                    Some(target.into()),
                    "{line:?} at {}",
                    start + offset
                );
            }
        }
    }

    #[test]
    fn terminal_detection_preserves_apostrophes() {
        for text in [
            "https://example.org/O'Reilly?q=1",
            "https://example.org/it's/John's?q=1&two=2",
            "/tmp/it's John's file.txt",
        ] {
            for at in 0..text.len() {
                assert_eq!(detected(text, at), Some(text.into()), "{text:?} at {at}");
            }
        }
        let quoted = "See \"https://example.org/O'Reilly\" here";
        assert_eq!(
            detected(quoted, 15),
            Some("\"https://example.org/O'Reilly\"".into())
        );
    }

    #[test]
    fn terminal_detection_keeps_the_whole_path_and_url() {
        for text in [
            "/tmp/My folder/日本語.txt",
            r"/tmp/My\ folder/日本語.txt",
            "../relative folder/file.txt",
            "relative/file.txt",
            "a.txt",
        ] {
            assert_eq!(
                detected(text, text.find('.').unwrap_or(2)),
                Some(text.into())
            );
        }
        assert_eq!(
            detected("See '/tmp/My folder' here", 12),
            Some("'/tmp/My folder'".into())
        );
        assert_eq!(
            detected("See https://example.org/a(b)). next", 15),
            Some("https://example.org/a(b)".into())
        );
        assert_eq!(
            detected("See /tmp/My folder", 7),
            Some("/tmp/My".into()),
            "unquoted whitespace separates filename tokens"
        );
        assert_eq!(detected("My folder.txt", 4), Some("folder.txt".into()));
        let quoted = r#"See "/tmp/My \"quoted\" folder.txt" here"#;
        assert_eq!(
            detected(quoted, 12),
            Some(r#""/tmp/My \"quoted\" folder.txt""#.into())
        );
    }
}

pub(super) type Open = Rc<RefCell<Option<Box<dyn Fn(Result<Target, String>)>>>>;

fn directory(session: &super::Session) -> Result<PathBuf, String> {
    let pid = session.pid.get().ok_or("The shell is not running")?.0;
    let pty = session.terminal.pty().ok_or("The terminal has no PTY")?;
    let foreground = unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) };
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    let verified = !session.closing.get()
        && !session.exited.get()
        && foreground == pid
        && session.shell_exe.is_some()
        && executable == session.shell_exe;
    local_directory(
        session.terminal.current_directory_uri().as_deref(),
        cwd.as_deref(),
        verified,
        &glib::host_name(),
    )
}

#[derive(Clone)]
struct Hit {
    target: Result<Target, String>,
    copy: Option<String>,
    tooltip: String,
    web: bool,
}

fn hit_at(term: &vte::Terminal, session: &Weak<super::Session>, x: f64, y: f64) -> Option<Hit> {
    let raw = text_at(term, x, y)?;
    let cwd = session
        .upgrade()
        .ok_or_else(|| "The shell is not running".to_owned())
        .and_then(|session| directory(&session));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let hyperlink = term.check_hyperlink_at(x, y).is_some();
    let target = if hyperlink {
        resolve_uri(
            &raw,
            cwd.as_deref().map_err(String::as_str),
            &glib::host_name(),
        )
    } else {
        resolve(
            &raw,
            cwd.as_deref().map_err(String::as_str),
            home.as_deref(),
            &glib::host_name(),
        )
    };
    let web = match &target {
        Ok(Target::Web(_)) => true,
        Ok(Target::Path(_)) => false,
        Err(_) => hyperlink || literal(&raw).is_ok_and(|text| is_uri(&text)),
    };
    let copy = match &target {
        Ok(Target::Web(uri)) => Some(uri.clone()),
        Ok(Target::Path(path)) => Some(path.to_string_lossy().into_owned()),
        Err(_) if !raw.chars().any(char::is_control) => Some(raw.clone()),
        _ => None,
    };
    let tooltip = match &target {
        Ok(Target::Web(uri)) => uri.clone(),
        Ok(Target::Path(path)) => path.display().to_string(),
        Err(error) => format!("{}\n{error}", raw.escape_debug()),
    };
    Some(Hit {
        target,
        copy,
        tooltip,
        web,
    })
}

fn install_patterns(term: &vte::Terminal) {
    // PCRE2_UTF | PCRE2_UCP | PCRE2_MULTILINE (pcre2.h).
    const FLAGS: u32 = 0x0008_0000 | 0x0002_0000 | 0x0000_0400;
    term.set_allow_hyperlink(true);
    for pattern in PATTERNS {
        let regex = vte::Regex::for_match(pattern, FLAGS).expect("valid built-in terminal pattern");
        term.match_add_regex(&regex, 0);
    }
}

fn text_at(term: &vte::Terminal, x: f64, y: f64) -> Option<String> {
    // An OSC 8 target is authoritative; do not strip punctuation from its URI.
    term.check_hyperlink_at(x, y)
        .map(|s| s.to_string())
        .or_else(|| term.check_match_at(x, y).0.map(|s| clean_detected(&s)))
}

fn popup(term: &vte::Terminal, hit: Hit, open: &Open, x: f64, y: f64) {
    let actions = gio::SimpleActionGroup::new();
    let open_action = gio::SimpleAction::new("open", None);
    let copy_action = gio::SimpleAction::new("copy", None);
    let handler = open.clone();
    open_action.connect_activate(move |_, _| {
        if let Some(open) = handler.borrow().as_ref() {
            open(hit.target.clone());
        }
    });
    let weak = term.downgrade();
    copy_action.set_enabled(hit.copy.is_some());
    copy_action.connect_activate(move |_, _| {
        if let (Some(term), Some(text)) = (weak.upgrade(), hit.copy.as_ref()) {
            term.clipboard().set_text(text);
        }
    });
    actions.add_action(&open_action);
    actions.add_action(&copy_action);
    term.insert_action_group("term-target", Some(&actions));
    let menu = gio::Menu::new();
    menu.append(
        Some(if hit.web { "Open Link" } else { "Open Path" }),
        Some("term-target.open"),
    );
    menu.append(
        Some(if hit.web { "Copy Link" } else { "Copy Path" }),
        Some("term-target.copy"),
    );
    super::super::view::popup_context_menu(term.upcast_ref(), &menu, x, y);
}

pub(super) fn install(term: &vte::Terminal, session: &Rc<super::Session>, open: &Open) {
    install_patterns(term);
    term.connect_unmap(|term| unsafe {
        if let Some(popover) = term.steal_data::<gtk::PopoverMenu>("fs-context-menu") {
            popover.unparent();
        }
    });

    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = term.downgrade();
    let session = Rc::downgrade(session);
    let clicked_session = session.clone();
    let handler = open.clone();
    click.connect_pressed(move |gesture, presses, x, y| {
        let Some(term) = weak.upgrade() else {
            return;
        };
        let button = gesture.current_button();
        let activate = should_open(button, presses, gesture.current_event_state());
        if !activate && button != gdk::BUTTON_SECONDARY {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        let Some(hit) = hit_at(&term, &clicked_session, x, y) else {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if activate {
            if let Some(open) = handler.borrow().as_ref() {
                open(hit.target);
            }
        } else {
            popup(&term, hit, &handler, x, y);
        }
    });
    term.add_controller(click);
    let motion = gtk::EventControllerMotion::new();
    let weak = term.downgrade();
    motion.connect_motion(move |_, x, y| {
        if let Some(term) = weak.upgrade() {
            let hit = hit_at(&term, &session, x, y);
            term.set_tooltip_text(hit.as_ref().map(|hit| hit.tooltip.as_str()));
            term.set_cursor_from_name(hit.as_ref().map(|_| "pointer"));
        }
    });
    let weak = term.downgrade();
    motion.connect_leave(move |_| {
        if let Some(term) = weak.upgrade() {
            term.set_tooltip_text(None);
            term.set_cursor(None);
        }
    });
    term.add_controller(motion);
}

#[cfg(test)]
mod gtk_tests {
    use super::*;

    async fn wait_for(mut ready: impl FnMut() -> bool) {
        for _ in 0..200 {
            if ready() {
                return;
            }
            glib::timeout_future(std::time::Duration::from_millis(25)).await;
        }
        panic!("terminal condition did not become ready");
    }

    fn point_for(term: &vte::Terminal, text: &str) -> Option<(f64, f64)> {
        let width = term.char_width().max(1) as f64;
        let height = term.char_height().max(1) as f64;
        for row in 0..term.row_count().min(20) {
            for col in 0..term.column_count().min(150) {
                let (x, y) = ((col as f64 + 0.5) * width, (row as f64 + 0.5) * height);
                if text_at(term, x, y).as_deref() == Some(text) {
                    return Some((x, y));
                }
            }
        }
        None
    }

    async fn feed_target(term: &vte::Terminal, output: &str, target: &str) -> (f64, f64) {
        // Allocate before feeding, just as a visible terminal is before user output.
        term.allocate(1100, 320, -1, None);
        term.reset(true, true);
        term.feed(output.as_bytes());
        for _ in 0..40 {
            glib::timeout_future(std::time::Duration::from_millis(25)).await;
            if let Some(point) = point_for(term, target) {
                return point;
            }
        }
        panic!(
            "VTE did not detect {target:?}; grid={}x{}; first match={:?}",
            term.column_count(),
            term.row_count(),
            term.check_match_at(5.0, 5.0)
        );
    }

    #[test]
    #[ignore = "requires an isolated graphical session with SHELL=/bin/sh"]
    fn terminal_real_ls_plain_quoted_and_column_names_are_targets() {
        use super::super::Pane;
        gtk::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let root =
            std::env::temp_dir().join(format!("filescope-terminal-ls-{}", std::process::id()));
        std::fs::create_dir_all(root.join("folder")).unwrap();
        for name in [
            "plain.txt",
            "日本語.txt",
            "My file.txt",
            "it's John's.txt",
            "John's \"report\".txt",
            "notes:2026.txt",
            "~",
            "~backup",
        ] {
            std::fs::write(root.join(name), "keep").unwrap();
        }
        glib::MainContext::default().block_on(async {
            let pane = Pane::new();
            let window = gtk::Window::builder().default_width(1400).default_height(500).build();
            window.set_child(Some(&pane.widget()));
            window.present();
            pane.toggle(Some(&root));
            let session = pane.session.borrow().clone().unwrap();
            wait_for(|| directory(&session).is_ok()).await;
            let term = &session.terminal;
            let expected = [
                ("plain.txt", "plain.txt"),
                ("folder", "folder"),
                ("日本語.txt", "日本語.txt"),
                ("'My file.txt'", "My file.txt"),
                ("\"it's John's.txt\"", "it's John's.txt"),
                (r#"'John'\''s "report".txt'"#, "John's \"report\".txt"),
                ("notes:2026.txt", "notes:2026.txt"),
                ("'~'", "~"),
                ("'~backup'", "~backup"),
            ];
            for layout in ["-1", "-C"] {
                term.reset(true, true);
                term.feed_child(format!("LS_COLORS='di=01;34:fi=0' LC_ALL=C.UTF-8 ls {layout} --color=always --quoting-style=shell-escape --width=100\n").as_bytes());
                wait_for(|| directory(&session).is_ok() && expected.iter().all(|(printed, _)| point_for(term, printed).is_some())).await;
                for (printed, name) in expected {
                    let (x, y) = point_for(term, printed).unwrap();
                    let hit = hit_at(term, &Rc::downgrade(&session), x, y).unwrap();
                    assert_eq!(hit.target, Ok(Target::Path(root.join(name))), "{layout}: {printed}");
                }
                term.select_all();
                assert!(term.has_selection());
                term.unselect_all();
            }
            for uri in ["javascript:alert(1)", "data:text/plain,hello", "http:notes.txt"] {
                let output = format!("\x1b]8;;{uri}\x1b\\label\x1b]8;;\x1b\\\r\n");
                let (x, y) = feed_target(term, &output, uri).await;
                let hit = hit_at(term, &Rc::downgrade(&session), x, y).unwrap();
                assert!(hit.target.is_err(), "an explicit hyperlink was treated as a filename: {uri}");
                assert!(hit.web);
            }
            pane.shutdown();
            window.close();
            drop(session);
            glib::timeout_future(std::time::Duration::from_millis(800)).await;
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn terminal_selection_cancels_after_navigation_and_broken_links_are_revealed() {
        use super::super::super::{self as window, navigation};
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.TerminalSelectionTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!(
            "filescope-terminal-selection-{}",
            std::process::id()
        ));
        for dir in ["one", "two"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("report.txt"), "keep").unwrap();
        }
        std::os::unix::fs::symlink("missing-target", root.join("one/broken-link")).unwrap();
        glib::MainContext::default().block_on(async {
            window::open_files(&application, &[gio::File::for_path(root.join("two"))]);
            let app = window::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            for navigate_away in [true, false] {
                navigation::navigate(&app, &tab, gio::File::for_path(root.join("two")));
                let acted = Rc::new(std::cell::Cell::new(false));
                let acted_ref = acted.clone();
                let a = Rc::downgrade(&app);
                let t = Rc::downgrade(&tab);
                let one = root.join("one");
                let two = root.join("two");
                let handler = tab.dir_list.connect_file_notify(move |list| {
                    if list.file().and_then(|f| f.path()) != Some(one.clone()) {
                        return;
                    }
                    let a = a.clone();
                    let t = t.clone();
                    let acted = acted_ref.clone();
                    let two = two.clone();
                    // Run a user action after selection is queued but before the listing arrives.
                    glib::idle_add_local_full(glib::Priority::HIGH, move || {
                        let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) else {
                            return glib::ControlFlow::Break;
                        };
                        if navigate_away {
                            navigation::navigate(&app, &tab, gio::File::for_path(&two));
                        } else {
                            tab.terminal.open.borrow().as_ref().unwrap()(Err(
                                "Newer terminal request".into(),
                            ));
                        }
                        acted.set(true);
                        glib::ControlFlow::Break
                    });
                });
                tab.terminal.open.borrow().as_ref().unwrap()(Ok(Target::Path(
                    root.join("one/report.txt"),
                )));
                wait_for(|| acted.get() && !tab.dir_list.is_loading()).await;
                assert_eq!(
                    tab.dir_list.file().and_then(|f| f.path()),
                    Some(root.join(if navigate_away { "two" } else { "one" }))
                );
                assert!(
                    window::commands::selected(&tab).is_empty(),
                    "an obsolete callback selected a file after a newer action"
                );
                tab.dir_list.disconnect(handler);
            }
            tab.terminal.open.borrow().as_ref().unwrap()(Ok(Target::Path(
                root.join("one/broken-link"),
            )));
            wait_for(|| {
                window::commands::selected(&tab)
                    .first()
                    .is_some_and(|(f, _)| f.path() == Some(root.join("one/broken-link")))
            })
            .await;
            assert_eq!(
                std::fs::read_link(root.join("one/broken-link")).unwrap(),
                PathBuf::from("missing-target")
            );
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn terminal_vte_apostrophes_keep_the_complete_target() {
        gtk::init().unwrap();
        let term = vte::Terminal::new();
        install_patterns(&term);
        let window = gtk::Window::new();
        window.set_child(Some(&term));
        window.present();
        glib::MainContext::default().block_on(async {
            for text in [
                "https://example.org/O'Reilly?q=1",
                "https://example.org/it's/John's?q=1&two=2",
                "/tmp/it's John's file.txt",
            ] {
                let (_, y) = feed_target(&term, &format!("{text}\r\n"), text).await;
                let width = term.char_width() as f64;
                for col in 0..text.len() {
                    assert_eq!(
                        text_at(&term, (col as f64 + 0.5) * width, y).as_deref(),
                        Some(text),
                        "column {col}"
                    );
                }
            }
        });
        window.close();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn terminal_reveals_hidden_files_and_cancels_stale_opens() {
        use super::super::super::{self as window, navigation};
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.TerminalNavigationTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!(
            "filescope-terminal-regressions-{}",
            std::process::id()
        ));
        for name in ["one", "two"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("one/.env"), "keep").unwrap();
        glib::MainContext::default().block_on(async {
            window::open_files(&application, &[gio::File::for_path(root.join("two"))]);
            let app = window::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            if app.show_hidden.get() {
                app.window
                    .lookup_action("toggle-hidden")
                    .unwrap()
                    .activate(None);
            }
            let open = |path| tab.terminal.open.borrow().as_ref().unwrap()(Ok(Target::Path(path)));
            open(root.join("one/.env"));
            wait_for(|| {
                tab.dir_list.file().and_then(|f| f.path()) == Some(root.join("one"))
                    && !tab.dir_list.is_loading()
            })
            .await;
            assert_eq!(
                window::commands::selected(&tab)
                    .iter()
                    .map(|(file, _)| file.path().unwrap())
                    .collect::<Vec<_>>(),
                [root.join("one/.env")]
            );
            assert_eq!(
                std::fs::read_to_string(root.join("one/.env")).unwrap(),
                "keep"
            );

            open(root.join("one"));
            navigation::navigate(&app, &tab, gio::File::for_path(root.join("two")));
            glib::timeout_future(std::time::Duration::from_millis(300)).await;
            assert_eq!(
                tab.dir_list.file().and_then(|f| f.path()),
                Some(root.join("two")),
                "a pending open overrode newer folder navigation"
            );

            open(root.join("one"));
            window::places::show_trash(&app, &tab);
            glib::timeout_future(std::time::Duration::from_millis(300)).await;
            assert_eq!(
                tab.view_stack.visible_child_name().as_deref(),
                Some("trash")
            );

            navigation::navigate(&app, &tab, gio::File::for_path(root.join("two")));
            open(root.join("one"));
            open(root.join("missing"));
            glib::timeout_future(std::time::Duration::from_millis(300)).await;
            assert_eq!(
                tab.dir_list.file().and_then(|f| f.path()),
                Some(root.join("two")),
                "a superseded open still navigated"
            );

            open(root.join("one"));
            window::places::show_computer(&app, &tab);
            glib::timeout_future(std::time::Duration::from_millis(300)).await;
            assert_eq!(
                tab.view_stack.visible_child_name().as_deref(),
                Some("computer")
            );

            navigation::navigate(&app, &tab, gio::File::for_path(root.join("two")));
            open(root.join("one"));
            tab.search_bar.set_search_mode(true);
            tab.search_entry.set_text("sample");
            window::search::changed(&app, &tab, true);
            glib::timeout_future(std::time::Duration::from_millis(300)).await;
            assert!(
                tab.search.active.get(),
                "a pending open replaced newer search navigation"
            );

            window::places::show_trash(&app, &tab);
            open(root.join("one"));
            window::places::show_trash(&app, &tab); // Background refresh is not new navigation.
            wait_for(|| tab.dir_list.file().and_then(|f| f.path()) == Some(root.join("one"))).await;
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session with SHELL=/bin/sh"]
    fn terminal_vte_targets_context_menu_and_shell_context() {
        use super::super::{Pane, Session};
        assert_eq!(std::env::var("SHELL").as_deref(), Ok("/bin/sh"));
        gtk::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let root =
            std::env::temp_dir().join(format!("filescope-terminal-targets-{}", std::process::id()));
        std::fs::create_dir_all(root.join("one")).unwrap();
        std::fs::create_dir_all(root.join("two")).unwrap();
        let path = root.join("one/My folder/日本語.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"keep me").unwrap();
        let context = glib::MainContext::default();
        context.with_thread_default(|| context.block_on(async {
            let pane = Pane::new();
            let opened = Rc::new(RefCell::new(Vec::new()));
            let log = opened.clone();
            *pane.open.borrow_mut() = Some(Box::new(move |target| log.borrow_mut().push(target)));
            let window = gtk::Window::builder().default_width(1100).default_height(420).build();
            window.set_child(Some(&pane.widget())); window.present();
            pane.toggle(Some(&root.join("one")));
            let session: Rc<Session> = pane.session.borrow().clone().unwrap();
            wait_for(|| session.pid.get().is_some()).await;
            assert_eq!(directory(&session).unwrap(), root.join("one"));
            let term = session.terminal.clone();
            term.set_font(Some(&gtk::pango::FontDescription::from_string("Monospace 12")));
            let raw = format!("\"{}\"", path.display());
            let (x, y) = feed_target(&term, &format!("{raw}\r\n"), &raw).await;
            let hit = hit_at(&term, &Rc::downgrade(&session), x, y).unwrap();
            assert_eq!(hit.target, Ok(Target::Path(path.clone())));
            assert_eq!(hit.tooltip, path.to_str().unwrap());
            popup(&term, hit, &pane.open, x, y);
            term.activate_action("term-target.copy", None).unwrap();
            assert_eq!(term.clipboard().read_text_future().await.unwrap().as_deref(), path.to_str());
            term.activate_action("term-target.open", None).unwrap();
            // Direct action activation bypasses the menu item's usual popdown.
            unsafe { term.data::<gtk::PopoverMenu>("fs-context-menu").unwrap().as_ref().popdown(); }
            assert_eq!(opened.borrow().last(), Some(&Ok(Target::Path(path.clone()))));
            term.select_all(); assert!(term.has_selection()); term.unselect_all();
            let uri = glib::filename_to_uri(&path, None).unwrap();
            let (x, y) = feed_target(&term, &format!("{uri}\r\n"), &uri).await;
            let file_hit = hit_at(&term, &Rc::downgrade(&session), x, y).unwrap();
            assert_eq!(file_hit.target, Ok(Target::Path(path.clone())));
            assert!(!file_hit.web, "a local file URI must offer Copy Path for its decoded path");

            let uri = "https://example.org/exact!";
            let (x, y) = feed_target(&term, &format!("\x1b]8;;{uri}\x1b\\hidden label\x1b]8;;\x1b\\\r\n"), uri).await;
            term.feed(b"\x1b]7;file://remote-host/etc\x07");
            wait_for(|| term.current_directory_uri().as_deref() == Some("file://remote-host/etc")).await;
            assert!(directory(&session).is_err(), "a reported remote host must never use the local shell cwd");
            let hit = hit_at(&term, &Rc::downgrade(&session), x, y).unwrap();
            assert_eq!(hit.target, Ok(Target::Web(uri.into())), "an explicit URI must retain its punctuation and work without a local cwd");
            popup(&term, hit, &pane.open, x, y);
            term.activate_action("term-target.open", None).unwrap();
            // Direct action activation bypasses the menu item's usual popdown.
            unsafe { term.data::<gtk::PopoverMenu>("fs-context-menu").unwrap().as_ref().popdown(); }
            assert_eq!(opened.borrow().last(), Some(&Ok(Target::Web(uri.into()))));

            term.feed_child(format!("cd '{}'\n", root.join("two").display()).as_bytes());
            let local = glib::filename_to_uri(root.join("two"), Some(glib::host_name().as_str())).unwrap();
            term.feed(format!("\x1b]7;{local}\x07").as_bytes());
            wait_for(|| directory(&session).ok().as_deref() == Some(root.join("two").as_path())).await;
            assert_eq!(resolve("file.txt", directory(&session).as_deref().map_err(String::as_str), None, &glib::host_name()).unwrap(), Target::Path(root.join("two/file.txt")));
            term.feed_child(b"sleep 10\n");
            wait_for(|| unsafe { libc::tcgetpgrp(term.pty().unwrap().fd().as_raw_fd()) } != session.pid.get().unwrap().0).await;
            assert!(directory(&session).is_err(), "foreground programs, including SSH and full-screen apps, cannot supply local paths");
            term.feed_child(b"\x03");
            wait_for(|| directory(&session).is_ok()).await;
            assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
            // exec keeps the PID/process group; checking only those would trust SSH.
            term.feed_child(b"exec sleep 10\n");
            let pid = session.pid.get().unwrap().0;
            wait_for(|| std::fs::read_link(format!("/proc/{pid}/exe")).ok() != session.shell_exe).await;
            assert_eq!(unsafe { libc::tcgetpgrp(term.pty().unwrap().fd().as_raw_fd()) }, pid);
            assert!(directory(&session).is_err());


            let weak = Rc::downgrade(&session); let weak_term = term.downgrade(); let shell_pid = session.pid.get().unwrap().0;
            pane.shutdown(); window.close(); drop(term); drop(session);
            glib::timeout_future(std::time::Duration::from_millis(800)).await;
            assert!(weak.upgrade().is_none(), "terminal interactions retained the session");
            assert_eq!(unsafe { libc::kill(shell_pid, 0) }, -1, "shell was not reaped");
            assert!(weak_term.upgrade().is_none(), "terminal interactions retained the widget");
        })).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session with SHELL=/bin/sh and temporary XDG_CONFIG_HOME"]
    fn terminal_paths_open_in_the_owning_tab_without_changing_the_shell() {
        use super::super::super::{self as window, navigation};
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.TerminalTargetsTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root =
            std::env::temp_dir().join(format!("filescope-terminal-owner-{}", std::process::id()));
        for name in ["shell/child", "view", "other"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let file = root.join("shell/child/日本語.txt");
        std::fs::write(&file, "keep").unwrap();
        let context = glib::MainContext::default();
        context
            .with_thread_default(|| {
                context.block_on(async {
                    window::open_files(&application, &[gio::File::for_path(root.join("shell"))]);
                    let app = window::app_state(&application).unwrap();
                    let owner = navigation::active_tab(&app);
                    owner.terminal.toggle(Some(&root.join("shell")));
                    let session = owner.terminal.session.borrow().clone().unwrap();
                    wait_for(|| session.pid.get().is_some()).await;
                    assert!(application.accels_for_action("win.copy").is_empty());
                    session.terminal.feed_child(b"\x04");
                    wait_for(|| owner.terminal.shell.borrow().is_none()).await;
                    assert!(
                        !application.accels_for_action("win.copy").is_empty(),
                        "Ctrl+D must restore file-view shortcuts"
                    );
                    drop(session);
                    owner.terminal.toggle(Some(&root.join("shell"))); // Hide the exited shell's note.
                    owner.terminal.toggle(Some(&root.join("shell")));
                    let session = owner.terminal.session.borrow().clone().unwrap();
                    wait_for(|| session.pid.get().is_some()).await;

                    navigation::navigate(&app, &owner, gio::File::for_path(root.join("view")));
                    navigation::new_tab(&app, gio::File::for_path(root.join("other")), true);
                    let other = navigation::active_tab(&app);
                    let target = resolve(
                        "child/日本語.txt",
                        directory(&session).as_deref().map_err(String::as_str),
                        None,
                        &glib::host_name(),
                    )
                    .unwrap();
                    owner.terminal.open.borrow().as_ref().unwrap()(Ok(target));
                    wait_for(|| {
                        owner.dir_list.file().and_then(|f| f.path())
                            == Some(root.join("shell/child"))
                            && owner.selection.selection().size() == 1
                    })
                    .await;
                    assert_eq!(
                        super::super::super::commands::selected(&owner)[0].0.path(),
                        Some(file)
                    );
                    assert!(Rc::ptr_eq(&navigation::active_tab(&app), &other));
                    assert_eq!(
                        other.dir_list.file().and_then(|f| f.path()),
                        Some(root.join("other"))
                    );
                    assert_eq!(directory(&session).unwrap(), root.join("shell"));
                    std::fs::create_dir_all(root.join("elsewhere/deeper")).unwrap();
                    std::os::unix::fs::symlink(
                        root.join("elsewhere/deeper"),
                        root.join("shell/link"),
                    )
                    .unwrap();
                    std::fs::write(root.join("elsewhere/important.txt"), "correct target").unwrap();
                    let target = resolve(
                        "link/../important.txt",
                        directory(&session).as_deref().map_err(String::as_str),
                        None,
                        &glib::host_name(),
                    )
                    .unwrap();
                    owner.terminal.open.borrow().as_ref().unwrap()(Ok(target));
                    wait_for(|| {
                        owner.dir_list.file().and_then(|f| f.path()) == Some(root.join("elsewhere"))
                            && owner.selection.selection().size() == 1
                    })
                    .await;
                    assert_eq!(
                        window::commands::selected(&owner)[0].0.path(),
                        Some(root.join("elsewhere/important.txt"))
                    );

                    owner.terminal.open.borrow().as_ref().unwrap()(Ok(Target::Path(
                        root.join("shell"),
                    )));
                    wait_for(|| {
                        owner.dir_list.file().and_then(|f| f.path()) == Some(root.join("shell"))
                    })
                    .await;
                    owner.terminal.open.borrow().as_ref().unwrap()(Ok(Target::Path(
                        root.join("view"),
                    )));
                    let page = owner.page.borrow().clone().unwrap();
                    let weak = Rc::downgrade(&owner);
                    app.tab_view.close_page(&page);
                    drop(page);
                    drop(owner);
                    drop(session);
                    glib::timeout_future(std::time::Duration::from_millis(800)).await;
                    assert!(
                        weak.upgrade().is_none(),
                        "a pending terminal open kept a closed tab alive"
                    );
                    app.window.close();
                })
            })
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
