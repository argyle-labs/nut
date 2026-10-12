//! `orca-nut-notify` — the NUT event handler `upsmon` (`NOTIFYCMD`) and
//! `upssched` (`CMDSCRIPT`) invoke.
//!
//! It is the plugin binary itself, reached through a symlink named
//! [`HANDLER_NAME`]; `main` dispatches here when `argv[0]` carries that name.
//! It reads the `# orca-*` markers in `upssched.conf`, appends the event to
//! `log_path`, forwards it to the remote syslog target, and on `ONBATT` runs the
//! capture command. Every step is bounded and failures are swallowed: the
//! handler always exits 0 so it can never block or crash `upsmon`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{detect_conf_dir, marker_raw, marker_value};

/// File name of the handler symlink; `main` dispatches on it.
pub const HANDLER_NAME: &str = "orca-nut-notify";

const SYSLOG_TIMEOUT: Duration = Duration::from_secs(3);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether this process was started as the handler (via the symlink).
pub fn invoked_as_handler(argv0: &str) -> bool {
    Path::new(argv0).file_name().and_then(|n| n.to_str()) == Some(HANDLER_NAME)
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
    /// upsmon passes the message as argv[1] with `NOTIFYTYPE`/`UPSNAME` set;
    /// upssched passes only the command name as argv[1].
    pub fn from_parts(
        args: &[String],
        notifytype: Option<String>,
        upsname: Option<String>,
    ) -> Self {
        let arg = args.first().cloned().unwrap_or_default();
        match notifytype.filter(|t| !t.is_empty()) {
            Some(kind) => Self {
                kind,
                ups: upsname.unwrap_or_default(),
                message: arg,
            },
            None => Self {
                kind: arg,
                ups: upsname.unwrap_or_default(),
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
pub fn handle(event: &Event, targets: &Targets) {
    let line = event.line();
    if let Some(path) = &targets.log_path {
        append_log(Path::new(path), &line).ok();
    }
    if let Some(target) = &targets.remote_syslog {
        send_syslog(target, &line, SYSLOG_TIMEOUT).ok();
    }
    if event.wants_capture() {
        if let Some(cmd) = &targets.capture_cmd {
            run_capture(cmd, CAPTURE_TIMEOUT).ok();
        }
    }
}

fn append_log(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{ts} {line}")?;
    f.sync_data()
}

/// RFC 3164 datagram (facility daemon, severity warning) over UDP. Name
/// resolution can hang, so the whole send runs on a thread bounded by `timeout`.
fn send_syslog(target: &str, line: &str, timeout: Duration) -> Result<(), String> {
    let target = target.to_string();
    let payload = format!("<28>{HANDLER_NAME}: {line}");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = (|| -> std::io::Result<()> {
            let addr = target
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::other("no address"))?;
            let bind = if addr.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            UdpSocket::bind(bind)?.send_to(payload.as_bytes(), addr)?;
            Ok(())
        })();
        tx.send(r.map_err(|e| e.to_string())).ok();
    });
    rx.recv_timeout(timeout)
        .map_err(|_| "syslog send timed out".to_string())?
}

/// Run the capture command through `sh -c`, killing it past `timeout`.
fn run_capture(cmd: &str, timeout: Duration) -> Result<(), String> {
    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                child.kill().ok();
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
    if fs::read_link(path).ok().as_deref() == Some(exe) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp: PathBuf = path.with_extension("orca-tmp");
    fs::remove_file(&tmp).ok();
    std::os::unix::fs::symlink(exe, &tmp).map_err(|e| format!("symlink {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        fs::remove_file(&tmp).ok();
        format!("install {}: {e}", path.display())
    })?;
    Ok(true)
}

/// Install the handler at `notify_cmd` when it names [`HANDLER_NAME`]; an
/// operator's own command is left alone (`Ok(None)`).
pub fn ensure_installed(notify_cmd: &str) -> Result<Option<bool>, String> {
    if !invoked_as_handler(notify_cmd) {
        return Ok(None);
    }
    let exe = std::env::current_exe().map_err(|e| format!("locate plugin binary: {e}"))?;
    install_handler(Path::new(notify_cmd), &exe).map(Some)
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
        let targets = Targets {
            log_path: Some("/proc/nonexistent/denied/events.log".into()),
            remote_syslog: Some("not a host".into()),
            capture_cmd: Some("exit 3".into()),
        };
        handle(
            &Event::from_parts(&args(&["onbattery-capture"]), None, None),
            &targets,
        );
    }

    #[test]
    fn capture_is_bounded() {
        let start = Instant::now();
        assert!(run_capture("sleep 10", Duration::from_millis(200)).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn install_symlinks_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("nut");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("bin/orca-nut-notify");
        assert!(!is_executable(&link));
        assert_eq!(install_handler(&link, &exe), Ok(true));
        assert_eq!(install_handler(&link, &exe), Ok(false));
        assert!(is_executable(&link));
        assert_eq!(fs::read_link(&link).unwrap(), exe);
    }

    #[test]
    fn ensure_installed_skips_foreign_commands() {
        assert_eq!(ensure_installed("/usr/sbin/upssched"), Ok(None));
    }
}
