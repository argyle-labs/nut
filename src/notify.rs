//! `orca-nut-notify` — the NUT event handler `upsmon` invokes as its
//! `NOTIFYCMD`.
//!
//! It is the plugin binary itself, reached through a symlink named
//! [`HANDLER_NAME`]; `main` dispatches here when `argv[0]` carries that name.
//! It reads the `# orca-*` markers in `upssched.conf`, appends the event to
//! `log_path`, forwards it to the remote syslog target, and on `ONBATT` runs the
//! capture command. Every step is bounded and failures are swallowed: the
//! handler always exits 0 so it can never block or crash `upsmon`.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{ToSocketAddrs, UdpSocket};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{detect_conf_dir, marker_raw, marker_value, MANAGED_EVENTS};

/// File name of the handler symlink; `main` dispatches on it.
pub const HANDLER_NAME: &str = "orca-nut-notify";

/// Directories the handler symlink may be installed into. Unraid uses the same
/// `/usr/local/bin` (RAM-backed there; the `notify-handler` repair re-creates it).
const HANDLER_DIRS: [&str; 2] = ["/usr/local/bin", "/usr/local/sbin"];

const LOG_TIMEOUT: Duration = Duration::from_secs(3);
const SYSLOG_TIMEOUT: Duration = Duration::from_secs(3);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether this process was started as the handler (via the symlink).
pub fn invoked_as_handler(argv0: &str) -> bool {
    Path::new(argv0).file_name().and_then(|n| n.to_str()) == Some(HANDLER_NAME)
}

/// The commands the rendered `upssched.conf` names (`log-<event>`, `onbattery-capture`).
fn is_upssched_command(arg: &str) -> bool {
    arg == "onbattery-capture"
        || arg
            .strip_prefix("log-")
            .is_some_and(|e| MANAGED_EVENTS.iter().any(|m| m.eq_ignore_ascii_case(e)))
}

/// One handler invocation, decoded from argv + environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// `NOTIFYTYPE` (upsmon) or the upssched command (`log-onbatt`, `onbattery-capture`).
    pub kind: String,
    pub ups: String,
    pub message: String,
}

impl Event {
    /// upsmon passes the message as argv[1] with `NOTIFYTYPE`/`UPSNAME` set; an
    /// upssched command in argv[1] wins over `NOTIFYTYPE`, which upssched
    /// inherits from the upsmon that spawned it.
    pub fn from_parts(
        args: &[String],
        notifytype: Option<String>,
        upsname: Option<String>,
    ) -> Self {
        let arg = args.first().cloned().unwrap_or_default();
        let ups = upsname.unwrap_or_default();
        match notifytype.filter(|t| !t.is_empty() && !is_upssched_command(&arg)) {
            Some(kind) => Self {
                kind,
                ups,
                message: arg,
            },
            None => Self {
                kind: arg,
                ups,
                message: String::new(),
            },
        }
    }

    fn wants_capture(&self) -> bool {
        self.kind.eq_ignore_ascii_case("ONBATT") || self.kind == "onbattery-capture"
    }

    fn line(&self) -> String {
        let mut s = format!("event={}", self.kind);
        if !self.ups.is_empty() {
            s.push_str(&format!(" ups={}", self.ups));
        }
        if !self.message.is_empty() {
            s.push_str(&format!(" msg={}", self.message));
        }
        s
    }
}

/// Handler targets read from the managed `upssched.conf` markers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    pub log_path: Option<String>,
    pub remote_syslog: Option<String>,
    pub capture_cmd: Option<String>,
}

impl Targets {
    pub fn from_upssched(text: &str) -> Self {
        let nonempty = |v: String| (!v.is_empty()).then_some(v);
        Self {
            log_path: marker_value(text, "orca-log-path").and_then(nonempty),
            remote_syslog: marker_value(text, "orca-remote-syslog").and_then(nonempty),
            capture_cmd: marker_raw(text, "orca-onbattery-capture")
                .map(str::to_string)
                .and_then(nonempty),
        }
    }
}

/// Per-step results of [`handle`]; `None` = step not attempted.
#[derive(Debug, Default)]
pub struct Outcome {
    pub log: Option<Result<(), String>>,
    pub syslog: Option<Result<(), String>>,
    pub capture: Option<Result<(), String>>,
}

/// Handler entrypoint. Never panics outward and never returns an error.
pub fn run() {
    std::panic::catch_unwind(|| {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let event = Event::from_parts(
            &args,
            std::env::var("NOTIFYTYPE").ok(),
            std::env::var("UPSNAME").ok(),
        );
        let sched = fs::read_to_string(detect_conf_dir().join("upssched.conf")).unwrap_or_default();
        handle(&event, &Targets::from_upssched(&sched));
    })
    .ok();
}

/// Log, forward, and (on battery) capture. Each step is independent.
pub fn handle(event: &Event, targets: &Targets) -> Outcome {
    let line = event.line();
    let mut out = Outcome::default();
    if let Some(path) = &targets.log_path {
        out.log = Some(append_log(PathBuf::from(path), line.clone(), LOG_TIMEOUT));
    }
    if let Some(target) = &targets.remote_syslog {
        out.syslog = Some(send_syslog(target, &line, SYSLOG_TIMEOUT));
    }
    if event.wants_capture() {
        if let Some(cmd) = &targets.capture_cmd {
            out.capture = Some(run_capture(cmd, CAPTURE_TIMEOUT));
        }
    }
    out
}

/// Run `f` on a thread, giving up after `timeout`; a hung `f` is left behind.
fn bounded<F>(timeout: Duration, what: &str, f: F) -> Result<(), String>
where
    F: FnOnce() -> std::io::Result<()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(f().map_err(|e| e.to_string())).ok();
    });
    rx.recv_timeout(timeout)
        .map_err(|_| format!("{what} timed out"))?
}

/// Append to the event log. The log can sit on a stalled NFS/flash mount, so
/// the write is bounded by `timeout`.
fn append_log(path: PathBuf, line: String, timeout: Duration) -> Result<(), String> {
    bounded(timeout, "event log write", move || {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{ts} {line}")?;
        f.sync_data()
    })
}

/// RFC 3164 datagram (facility daemon, severity warning) over UDP to the first
/// resolved address that accepts it. Name resolution can hang, so the whole
/// send is bounded by `timeout`. The `.invalid` default placeholder is skipped.
fn send_syslog(target: &str, line: &str, timeout: Duration) -> Result<(), String> {
    let host = target
        .rsplit_once(':')
        .map_or(target, |(h, _)| h)
        .trim_matches(['[', ']']);
    if host.ends_with(".invalid") {
        return Ok(());
    }
    let target = target.to_string();
    let payload = format!("<28>{HANDLER_NAME}: {line}");
    bounded(timeout, "syslog send", move || {
        let mut last = std::io::Error::other("no address");
        for addr in target.to_socket_addrs()? {
            let bind = if addr.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            match UdpSocket::bind(bind).and_then(|s| s.send_to(payload.as_bytes(), addr)) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    })
}

/// Run the capture command through `sh -c` in its own process group, killing
/// the whole group (grandchildren included) past `timeout`.
fn run_capture(cmd: &str, timeout: Duration) -> Result<(), String> {
    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("capture exited with {status}")),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                // The child leads its own group, so its pid is the pgid.
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                child.wait().ok();
                return Err("capture timed out".to_string());
            }
        }
    }
}

/// Whether `path` resolves to an executable regular file.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Point `path` at `exe` with a symlink, atomically. `Ok(false)` = already in place.
pub fn install_handler(path: &Path, exe: &Path) -> Result<bool, String> {
    let dirs: Vec<&Path> = HANDLER_DIRS.iter().map(Path::new).collect();
    install_handler_in(path, exe, &dirs)
}

/// [`install_handler`] restricted to `allowed` parent directories. Only an
/// existing symlink is ever replaced, and the parent must already exist.
fn install_handler_in(path: &Path, exe: &Path, allowed: &[&Path]) -> Result<bool, String> {
    let shown = path.display();
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err(format!(
            "{shown}: handler path must be absolute with no `..`"
        ));
    }
    let parent = path
        .parent()
        .filter(|p| allowed.contains(p))
        .ok_or_else(|| {
            let dirs: Vec<String> = allowed.iter().map(|d| d.display().to_string()).collect();
            format!("{shown}: handler must live in one of {}", dirs.join(", "))
        })?;
    if !parent.is_dir() {
        return Err(format!("{}: directory does not exist", parent.display()));
    }
    match fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_symlink() => {
            return Err(format!(
                "{shown} exists and is not a symlink; refusing to replace it"
            ));
        }
        Ok(_) if fs::read_link(path).ok().as_deref() == Some(exe) => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat {shown}: {e}")),
    }
    let tmp = parent.join(format!(".{HANDLER_NAME}.{}.tmp", random_suffix()?));
    // `symlink` fails on an existing path, so a pre-planted tmp is never followed.
    std::os::unix::fs::symlink(exe, &tmp).map_err(|e| format!("symlink {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        fs::remove_file(&tmp).ok();
        format!("install {shown}: {e}")
    })?;
    Ok(true)
}

fn random_suffix() -> Result<String, String> {
    let mut b = [0u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// The running binary, minus the " (deleted)" suffix Linux appends once it is
/// replaced on disk (the path then names the new binary).
fn plugin_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locate plugin binary: {e}"))?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(p) => PathBuf::from(p),
        None => exe,
    })
}

/// Install the handler at `notify_cmd` when it names [`HANDLER_NAME`]; an
/// operator's own command is left alone (`Ok(None)`).
pub fn ensure_installed(notify_cmd: &str) -> Result<Option<bool>, String> {
    if !invoked_as_handler(notify_cmd) {
        return Ok(None);
    }
    install_handler(Path::new(notify_cmd), &plugin_exe()?).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_handler_argv0() {
        assert!(invoked_as_handler("/usr/local/bin/orca-nut-notify"));
        assert!(invoked_as_handler("orca-nut-notify"));
        assert!(!invoked_as_handler("/usr/lib/orca/plugins/nut"));
    }

    #[test]
    fn decodes_upsmon_and_upssched_calls() {
        let e = Event::from_parts(
            &args(&["UPS ups@localhost on battery"]),
            Some("ONBATT".into()),
            Some("ups@localhost".into()),
        );
        assert_eq!(e.kind, "ONBATT");
        assert!(e.wants_capture());
        assert_eq!(
            e.line(),
            "event=ONBATT ups=ups@localhost msg=UPS ups@localhost on battery"
        );
        let e = Event::from_parts(&args(&["log-online"]), None, None);
        assert_eq!(e.kind, "log-online");
        assert!(!e.wants_capture());
        assert!(Event::from_parts(&args(&["onbattery-capture"]), None, None).wants_capture());
    }

    #[test]
    fn upssched_command_wins_over_inherited_notifytype() {
        let e = Event::from_parts(&args(&["log-onbatt"]), Some("ONBATT".into()), None);
        assert_eq!(e.kind, "log-onbatt");
        assert!(!e.wants_capture());
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("captured");
        let out = handle(
            &e,
            &Targets {
                capture_cmd: Some(format!("touch {}", marker.display())),
                ..Targets::default()
            },
        );
        assert!(out.capture.is_none());
        assert!(!marker.exists());
    }

    #[test]
    fn reads_markers_from_rendered_upssched() {
        let sched = "# orca-log-path /var/log/nut/events.log\n\
                     # orca-remote-syslog 10.0.0.9:514\n\
                     # orca-onbattery-capture /usr/local/bin/orca diagnostics diagnose >> /x.json\n";
        let t = Targets::from_upssched(sched);
        assert_eq!(t.log_path.as_deref(), Some("/var/log/nut/events.log"));
        assert_eq!(t.remote_syslog.as_deref(), Some("10.0.0.9:514"));
        assert_eq!(
            t.capture_cmd.as_deref(),
            Some("/usr/local/bin/orca diagnostics diagnose >> /x.json")
        );
        assert_eq!(Targets::from_upssched(""), Targets::default());
    }

    #[test]
    fn handle_logs_forwards_and_captures() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sub/events.log");
        let marker = dir.path().join("captured");
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let targets = Targets {
            log_path: Some(log.display().to_string()),
            remote_syslog: Some(sock.local_addr().unwrap().to_string()),
            capture_cmd: Some(format!("touch {}", marker.display())),
        };
        handle(
            &Event::from_parts(&args(&["on battery"]), Some("ONBATT".into()), None),
            &targets,
        );
        assert!(fs::read_to_string(&log).unwrap().contains("event=ONBATT"));
        let mut buf = [0u8; 512];
        let n = sock.recv(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("<28>orca-nut-notify: event=ONBATT"));
        assert!(marker.exists());
    }

    #[test]
    fn handle_survives_unusable_targets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let targets = Targets {
            log_path: Some(file.join("events.log").display().to_string()),
            remote_syslog: Some("not a host".into()),
            capture_cmd: Some("exit 3".into()),
        };
        let out = handle(
            &Event::from_parts(&args(&["onbattery-capture"]), None, None),
            &targets,
        );
        assert!(matches!(out.log, Some(Err(_))));
        assert!(matches!(out.syslog, Some(Err(_))));
        assert!(matches!(out.capture, Some(Err(_))));
    }

    #[test]
    fn syslog_skips_invalid_placeholder() {
        assert_eq!(
            send_syslog("syslog-collector.example.invalid:514", "x", SYSLOG_TIMEOUT),
            Ok(())
        );
    }

    #[test]
    fn append_log_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        // Opening a FIFO with no reader blocks indefinitely.
        assert!(append_log(fifo, "x".into(), Duration::from_millis(200)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn capture_is_bounded() {
        let start = Instant::now();
        assert!(run_capture("sleep 10", Duration::from_millis(200)).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn capture_timeout_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let cmd = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        assert!(run_capture(&cmd, Duration::from_millis(500)).is_err());
        let pid: libc::pid_t = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let start = Instant::now();
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "grandchild {pid} survived"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn exe_in(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("nut");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    #[test]
    fn install_symlinks_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let link = bin.join("orca-nut-notify");
        assert!(!is_executable(&link));
        assert_eq!(install_handler_in(&link, &exe, &[&bin]), Ok(true));
        assert_eq!(install_handler_in(&link, &exe, &[&bin]), Ok(false));
        assert!(is_executable(&link));
        assert_eq!(fs::read_link(&link).unwrap(), exe);
        let names: Vec<_> = fs::read_dir(&bin)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temp file left behind: {names:?}");
    }

    #[test]
    fn install_refuses_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let link = dir.path().join("orca-nut-notify");
        fs::write(&link, "operator script").unwrap();
        assert!(install_handler_in(&link, &exe, &[dir.path()]).is_err());
        assert_eq!(fs::read_to_string(&link).unwrap(), "operator script");
    }

    #[test]
    fn install_refuses_unlisted_relative_and_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let other = dir.path().join("elsewhere");
        fs::create_dir(&other).unwrap();
        let err = install_handler_in(&other.join("orca-nut-notify"), &exe, &[dir.path()]);
        assert!(err.unwrap_err().contains("must live in"));
        assert!(!other.join("orca-nut-notify").exists());
        let dotted = dir.path().join("elsewhere/../orca-nut-notify");
        assert!(install_handler_in(&dotted, &exe, &[dir.path()]).is_err());
        assert!(install_handler_in(Path::new("orca-nut-notify"), &exe, &[dir.path()]).is_err());
        let missing = dir.path().join("missing");
        assert!(install_handler_in(&missing.join("orca-nut-notify"), &exe, &[&missing]).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn ensure_installed_skips_foreign_and_refuses_unlisted() {
        assert_eq!(ensure_installed("/usr/sbin/upssched"), Ok(None));
        let dir = tempfile::tempdir().unwrap();
        let cmd = dir.path().join("orca-nut-notify");
        assert!(ensure_installed(&cmd.display().to_string()).is_err());
        assert!(fs::symlink_metadata(&cmd).is_err());
    }
}
