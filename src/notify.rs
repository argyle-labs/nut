//! `orca-nut-notify` — the NUT event handler `upsmon` invokes as its
//! `NOTIFYCMD`.
//!
//! It is the plugin binary itself, reached through a symlink (a copy on Unraid)
//! named [`HANDLER_NAME`]; `main` dispatches here when `argv[0]` carries that name.
//! It reads its targets from the `# orca-*` markers in `upssched.conf`, appends the event to
//! `log_path`, forwards it to the remote syslog target, and on `ONBATT` runs the
//! capture command. Every step is bounded and failures are swallowed: the
//! handler always exits 0 so it can never block or crash `upsmon`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{ToSocketAddrs, UdpSocket};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{detect_conf_dir, detect_init, marker_raw, marker_value, InitSystem};

/// File name of the handler symlink; `main` dispatches on it.
pub const HANDLER_NAME: &str = "orca-nut-notify";

/// Directories the handler may be installed into.
const HANDLER_DIRS: [&str; 2] = ["/usr/local/bin", "/usr/local/sbin"];

/// Unraid's flash copy of the handler. `/usr/local/bin` is RAM there and the
/// plugin binary sits on the array (mounted after NUT starts), so the handler is
/// a real copy, restored at boot from this file by orca's `.plg`.
const UNRAID_PERSIST: &str = "/boot/config/plugins/orca/orca-nut-notify";

const LOG_TIMEOUT: Duration = Duration::from_secs(3);
const SYSLOG_TIMEOUT: Duration = Duration::from_secs(3);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether this process was started as the handler (via the symlink).
pub fn invoked_as_handler(argv0: &str) -> bool {
    Path::new(argv0).file_name().and_then(|n| n.to_str()) == Some(HANDLER_NAME)
}

/// One handler invocation, decoded from argv + environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// `NOTIFYTYPE`, or argv[1] when upsmon set none.
    pub kind: String,
    pub ups: String,
    pub message: String,
}

impl Event {
    /// upsmon passes the message as argv[1] with `NOTIFYTYPE`/`UPSNAME` set.
    pub fn from_parts(
        args: &[String],
        notifytype: Option<String>,
        upsname: Option<String>,
    ) -> Self {
        let arg = args.first().cloned().unwrap_or_default();
        let ups = upsname.unwrap_or_default();
        match notifytype.filter(|t| !t.is_empty()) {
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
        self.kind.eq_ignore_ascii_case("ONBATT")
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
    // Capture runs last: it can take up to CAPTURE_TIMEOUT and must not delay the log/syslog record.
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

/// Install the handler at `path`: a symlink to `exe`, or with `persist` set a
/// 0755 copy of `exe` plus a refreshed copy at `persist`. `Ok(false)` = already in place.
pub fn install_handler(path: &Path, exe: &Path, persist: Option<&Path>) -> Result<bool, String> {
    let dirs: Vec<&Path> = HANDLER_DIRS.iter().map(Path::new).collect();
    install_handler_in(path, exe, &dirs, persist)
}

/// [`install_handler`] restricted to `allowed` parent directories; the parent
/// must already exist. Only a symlink, or a regular file we own, is replaced.
fn install_handler_in(
    path: &Path,
    exe: &Path,
    allowed: &[&Path],
    persist: Option<&Path>,
) -> Result<bool, String> {
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
    let _lock = lock_dir(parent)?;
    match persist {
        None => install_symlink(path, parent, exe),
        Some(persist) => install_copy(path, parent, exe, persist),
    }
}

/// Exclusive `flock` serializing installs into `dir`, which share one temp name.
/// Released when the returned file is dropped.
fn lock_dir(dir: &Path) -> Result<fs::File, String> {
    use std::os::unix::io::AsRawFd;
    let path = dir.join(format!(".{HANDLER_NAME}.lock"));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    // SAFETY: `file` owns a valid open descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

fn install_symlink(path: &Path, parent: &Path, exe: &Path) -> Result<bool, String> {
    let shown = path.display();
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
    let tmp = tmp_path(parent);
    fs::remove_file(&tmp).ok();
    // `symlink` fails on an existing path, so a pre-planted tmp is never followed.
    std::os::unix::fs::symlink(exe, &tmp).map_err(|e| format!("symlink {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        fs::remove_file(&tmp).ok();
        format!("install {shown}: {e}")
    })?;
    sync_dir(parent);
    Ok(true)
}

fn install_copy(path: &Path, parent: &Path, exe: &Path, persist: &Path) -> Result<bool, String> {
    let shown = path.display();
    let want = fs::read(exe).map_err(|e| format!("read {}: {e}", exe.display()))?;
    let prior = fs::read(persist).ok();
    let mut changed = false;
    // Ownership: a regular file is ours iff its bytes equal the current binary or
    // the flash copy. `path` is written before the flash copy, so a failed flash
    // write still leaves `path` equal to the binary and recognised next run.
    let current = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => None,
        Ok(m) if m.is_file() => {
            let len = m.len() as usize;
            let candidate = len == want.len() || prior.as_ref().is_some_and(|p| p.len() == len);
            let have = if candidate {
                fs::read(path).map_err(|e| format!("read {shown}: {e}"))?
            } else {
                Vec::new()
            };
            if !candidate || (have != want && prior.as_ref() != Some(&have)) {
                return Err(match prior {
                    None => format!(
                        "{shown} differs from the plugin binary and the flash copy {} is \
                         missing, so it cannot be recognised as orca's handler; refusing to \
                         replace it",
                        persist.display()
                    ),
                    Some(_) => {
                        format!("{shown} exists and is not orca's handler; refusing to replace it")
                    }
                });
            }
            Some(have)
        }
        Ok(_) => {
            return Err(format!(
                "{shown} exists and is not a file; refusing to replace it"
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("stat {shown}: {e}")),
    };
    if current.as_ref() != Some(&want) || !is_executable(path) {
        write_atomic(path, parent, &want)?;
        changed = true;
    }
    if prior.as_ref() != Some(&want) {
        let dir = persist
            .parent()
            .ok_or_else(|| format!("{}: no parent directory", persist.display()))?;
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        write_atomic(persist, dir, &want)?;
        changed = true;
    }
    Ok(changed)
}

/// Fixed temp name in `dir`, so a crashed run's leftover is reused, not accumulated.
fn tmp_path(dir: &Path) -> PathBuf {
    dir.join(format!(".{HANDLER_NAME}.tmp"))
}

/// Persist a rename by fsyncing its directory. The rename has already landed,
/// so a failure is only reported.
fn sync_dir(dir: &Path) {
    if let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all()) {
        eprintln!("warning: sync {}: {e}", dir.display());
    }
}

/// Write `bytes` to a fresh 0755 temp file in `dir`, then rename it over `path`.
fn write_atomic(path: &Path, dir: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = tmp_path(dir);
    // Unlinking first means `create_new` never follows a pre-planted symlink.
    fs::remove_file(&tmp).ok();
    let res = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()))
        .and_then(|_| fs::rename(&tmp, path));
    res.map_err(|e| {
        fs::remove_file(&tmp).ok();
        format!("install {}: {e}", path.display())
    })?;
    sync_dir(dir);
    Ok(())
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
    let unraid = detect_init(Path::new("/")) == InitSystem::Unraid;
    let persist = unraid.then_some(Path::new(UNRAID_PERSIST));
    install_handler(Path::new(notify_cmd), &plugin_exe()?, persist).map(Some)
}

/// Startup safety net: re-install the handler the on-disk `upsmon.conf`
/// `NOTIFYCMD` names. Never fails; a problem is only reported on stderr.
pub fn ensure_installed_at_startup() {
    let Ok(upsmon) = fs::read_to_string(detect_conf_dir().join("upsmon.conf")) else {
        return;
    };
    let Some(cmd) = crate::checks::notify_cmd(&upsmon) else {
        return;
    };
    if let Err(e) = ensure_installed(&cmd) {
        eprintln!(
            "warning: nut notify handler not installed: {e}{}",
            crate::config::privilege_hint(&e)
        );
    }
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
    fn decodes_upsmon_calls() {
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
        let e = Event::from_parts(&args(&["ONLINE"]), None, None);
        assert_eq!(e.kind, "ONLINE");
        assert!(!e.wants_capture());
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
        let out = handle(&Event::from_parts(&args(&["ONBATT"]), None, None), &targets);
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
        // A killed grandchild stays a zombie where nothing reaps orphans
        // (CI containers without an init), and kill(pid, 0) still succeeds on it.
        let zombie = |pid: libc::pid_t| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')
                        .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                })
                .unwrap_or(false)
        };
        while unsafe { libc::kill(pid, 0) } == 0 && !zombie(pid) {
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
        assert_eq!(install_handler_in(&link, &exe, &[&bin], None), Ok(true));
        assert_eq!(install_handler_in(&link, &exe, &[&bin], None), Ok(false));
        assert!(is_executable(&link));
        assert_eq!(fs::read_link(&link).unwrap(), exe);
        let names: Vec<_> = fs::read_dir(&bin)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "only handler and lock remain: {names:?}");
    }

    #[test]
    fn install_refuses_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let link = dir.path().join("orca-nut-notify");
        fs::write(&link, "operator script").unwrap();
        assert!(install_handler_in(&link, &exe, &[dir.path()], None).is_err());
        assert_eq!(fs::read_to_string(&link).unwrap(), "operator script");
    }

    #[test]
    fn concurrent_installs_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let bin = dir.path().join("bin");
        let flash = dir.path().join("flash");
        fs::create_dir(&bin).unwrap();
        let target = bin.join("orca-nut-notify");
        let persist = flash.join("orca-nut-notify");
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let (target, exe, bin, persist) = (&target, &exe, &bin, &persist);
                    s.spawn(move || {
                        let p = (i % 2 == 0).then_some(persist.as_path());
                        install_handler_in(target, exe, &[bin.as_path()], p)
                    })
                })
                .collect();
            for h in handles {
                let res = h.join().unwrap();
                // A copy and a symlink each refuse to replace the other kind.
                if let Err(e) = res {
                    assert!(e.contains("refusing to replace"), "{e}");
                }
            }
        });
        assert_eq!(fs::read(&target).unwrap(), fs::read(&exe).unwrap());
        assert!(!tmp_path(&bin).exists());
    }

    #[test]
    fn install_refuses_unlisted_relative_and_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let other = dir.path().join("elsewhere");
        fs::create_dir(&other).unwrap();
        let err = install_handler_in(&other.join("orca-nut-notify"), &exe, &[dir.path()], None);
        assert!(err.unwrap_err().contains("must live in"));
        assert!(!other.join("orca-nut-notify").exists());
        let dotted = dir.path().join("elsewhere/../orca-nut-notify");
        assert!(install_handler_in(&dotted, &exe, &[dir.path()], None).is_err());
        assert!(
            install_handler_in(Path::new("orca-nut-notify"), &exe, &[dir.path()], None).is_err()
        );
        let missing = dir.path().join("missing");
        assert!(
            install_handler_in(&missing.join("orca-nut-notify"), &exe, &[&missing], None).is_err()
        );
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

    #[test]
    fn unraid_installs_a_persisted_copy_and_refreshes_on_upgrade() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let exe = exe_in(root.path());
        let bin = root.path().join("usr/local/bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(HANDLER_NAME);
        let persist = root
            .path()
            .join("boot/config/plugins/orca")
            .join(HANDLER_NAME);
        assert_eq!(
            install_handler_in(&path, &exe, &[&bin], Some(&persist)),
            Ok(true)
        );
        assert!(!fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read(&persist).unwrap(), fs::read(&exe).unwrap());
        assert_eq!(
            install_handler_in(&path, &exe, &[&bin], Some(&persist)),
            Ok(false)
        );
        fs::write(&exe, "#!/bin/sh\n# v2\n").unwrap();
        assert_eq!(
            install_handler_in(&path, &exe, &[&bin], Some(&persist)),
            Ok(true)
        );
        assert_eq!(fs::read(&path).unwrap(), fs::read(&exe).unwrap());
        assert_eq!(fs::read(&persist).unwrap(), fs::read(&exe).unwrap());
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 2);
    }

    #[test]
    fn unraid_copy_refuses_a_foreign_file_and_replaces_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let exe = exe_in(root.path());
        let persist = root.path().join("flash").join(HANDLER_NAME);
        let path = root.path().join(HANDLER_NAME);
        fs::write(&path, "operator script").unwrap();
        assert!(install_handler_in(&path, &exe, &[root.path()], Some(&persist)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "operator script");
        assert!(!persist.exists());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&exe, &path).unwrap();
        assert_eq!(
            install_handler_in(&path, &exe, &[root.path()], Some(&persist)),
            Ok(true)
        );
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
    }
    #[test]
    fn unraid_copy_recovers_after_a_failed_flash_write() {
        let root = tempfile::tempdir().unwrap();
        let exe = exe_in(root.path());
        let path = root.path().join(HANDLER_NAME);
        let flash = root.path().join("flash");
        fs::write(&flash, "blocks create_dir_all").unwrap();
        let persist = flash.join(HANDLER_NAME);
        assert!(install_handler_in(&path, &exe, &[root.path()], Some(&persist)).is_err());
        assert_eq!(fs::read(&path).unwrap(), fs::read(&exe).unwrap());
        fs::remove_file(&flash).unwrap();
        assert_eq!(
            install_handler_in(&path, &exe, &[root.path()], Some(&persist)),
            Ok(true)
        );
        assert_eq!(fs::read(&persist).unwrap(), fs::read(&exe).unwrap());
    }

    #[test]
    fn unraid_copy_names_a_missing_flash_copy() {
        let root = tempfile::tempdir().unwrap();
        let exe = exe_in(root.path());
        let path = root.path().join(HANDLER_NAME);
        fs::write(&path, "older handler").unwrap();
        let persist = root.path().join("flash").join(HANDLER_NAME);
        let err = install_handler_in(&path, &exe, &[root.path()], Some(&persist)).unwrap_err();
        assert!(
            err.contains("flash copy") && err.contains("missing"),
            "{err}"
        );
    }

    #[test]
    fn install_reuses_a_stale_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path());
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(tmp_path(&bin), "leftover").unwrap();
        let path = bin.join(HANDLER_NAME);
        let persist = dir.path().join("flash").join(HANDLER_NAME);
        assert_eq!(
            install_handler_in(&path, &exe, &[&bin], Some(&persist)),
            Ok(true)
        );
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 2);
    }
}
