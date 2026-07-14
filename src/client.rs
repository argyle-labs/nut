//! Pure-Rust NUT (`upsd`) TCP client.
//!
//! Speaks just enough of the [NUT network protocol] to read UPS state for
//! `service.status` and the `ups-comms` diagnostics check: `LIST UPS` to
//! enumerate configured UPSes and `LIST VAR <ups>` to read their variables
//! (`ups.status`, `battery.charge`, `battery.runtime`, `input.voltage`, …).
//!
//! No new dependencies: the connection is a `tokio::net::TcpStream` (already
//! available via the toolkit) with a plain line-oriented reader. The protocol
//! is line-based ASCII terminated by `\n`, so a hand-rolled reader is simpler
//! (and lighter) than pulling in a framed codec.
//!
//! [NUT network protocol]: https://networkupstools.org/docs/developer-guide.chunked/ar01s09.html

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Default `upsd` port.
pub const DEFAULT_PORT: u16 = 3493;

/// How long any single network step may take before the probe gives up. Kept
/// short: `status`/`ups-comms` must fail fast when `upsd` is unreachable rather
/// than stall the whole diagnose fan-out.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// One UPS as reported by `LIST UPS`, with its variables from `LIST VAR`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ups {
    /// UPS name as `upsd` knows it (the `[section]` in `ups.conf`).
    pub name: String,
    /// Human description from `LIST UPS` (the trailing quoted field).
    pub description: String,
    /// All `VAR` values, keyed by variable name (`ups.status`, …).
    pub vars: BTreeMap<String, String>,
}

impl Ups {
    /// `ups.status` — the space-separated status flags (`OL`, `OB`, `LB`, …).
    pub fn status(&self) -> Option<&str> {
        self.vars.get("ups.status").map(String::as_str)
    }

    /// `battery.charge` as a percentage, if numeric.
    pub fn battery_charge(&self) -> Option<f64> {
        self.vars.get("battery.charge").and_then(|v| v.parse().ok())
    }

    /// `battery.runtime` in seconds, if numeric.
    pub fn battery_runtime(&self) -> Option<f64> {
        self.vars
            .get("battery.runtime")
            .and_then(|v| v.parse().ok())
    }

    /// `input.voltage`, if numeric.
    pub fn input_voltage(&self) -> Option<f64> {
        self.vars.get("input.voltage").and_then(|v| v.parse().ok())
    }

    /// True when the UPS is running on battery (`OB` present, `OL` absent).
    pub fn on_battery(&self) -> bool {
        self.status()
            .map(|s| s.split_whitespace().any(|f| f == "OB"))
            .unwrap_or(false)
    }
}

/// A connected `upsd` session.
pub struct NutClient {
    reader: BufReader<TcpStream>,
}

impl NutClient {
    /// Open a TCP session to `upsd` at `host:port`.
    pub async fn connect(host: &str, port: u16) -> Result<Self, String> {
        let stream = timeout(IO_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| format!("connect {host}:{port}: timed out"))?
            .map_err(|e| format!("connect {host}:{port}: {e}"))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// Enumerate every configured UPS and read all of its variables.
    pub async fn list_upses(&mut self) -> Result<Vec<Ups>, String> {
        let names = self.list_ups_names().await?;
        let mut out = Vec::with_capacity(names.len());
        for (name, description) in names {
            let vars = self.list_vars(&name).await?;
            out.push(Ups {
                name,
                description,
                vars,
            });
        }
        Ok(out)
    }

    /// `LIST UPS` → `[(name, description)]`.
    async fn list_ups_names(&mut self) -> Result<Vec<(String, String)>, String> {
        let body = self.list("LIST UPS").await?;
        Ok(body.iter().filter_map(|l| parse_ups_line(l)).collect())
    }

    /// `LIST VAR <ups>` → all variables for that UPS.
    async fn list_vars(&mut self, ups: &str) -> Result<BTreeMap<String, String>, String> {
        let body = self.list(&format!("LIST VAR {ups}")).await?;
        Ok(body.iter().filter_map(|l| parse_var_line(l)).collect())
    }

    /// Run a `LIST …` command and return the lines between `BEGIN LIST …` and
    /// `END LIST …`. An `ERR …` reply is surfaced as an error.
    async fn list(&mut self, cmd: &str) -> Result<Vec<String>, String> {
        self.send(cmd).await?;
        let mut body = Vec::new();
        loop {
            let line = self.read_line().await?;
            let line = line.trim_end();
            if let Some(err) = line.strip_prefix("ERR ") {
                return Err(format!("{cmd}: {}", err.trim()));
            }
            if line.starts_with("BEGIN LIST") {
                continue;
            }
            if line.starts_with("END LIST") {
                break;
            }
            body.push(line.to_string());
        }
        Ok(body)
    }

    async fn send(&mut self, cmd: &str) -> Result<(), String> {
        let line = format!("{cmd}\n");
        timeout(IO_TIMEOUT, self.reader.get_mut().write_all(line.as_bytes()))
            .await
            .map_err(|_| format!("send `{cmd}`: timed out"))?
            .map_err(|e| format!("send `{cmd}`: {e}"))
    }

    async fn read_line(&mut self) -> Result<String, String> {
        let mut buf = Vec::new();
        loop {
            let byte = timeout(IO_TIMEOUT, self.reader.read_u8())
                .await
                .map_err(|_| "read: timed out".to_string())?
                .map_err(|e| format!("read: {e}"))?;
            if byte == b'\n' {
                break;
            }
            buf.push(byte);
        }
        String::from_utf8(buf).map_err(|e| format!("non-utf8 reply: {e}"))
    }
}

/// Parse one `LIST UPS` body line: `UPS <name> "<description>"`.
fn parse_ups_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("UPS ")?;
    let (name, desc) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((name.to_string(), unquote(desc)))
}

/// Parse one `LIST VAR` body line: `VAR <ups> <name> "<value>"`.
fn parse_var_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("VAR ")?;
    // Drop the ups token, then split var-name from its quoted value.
    let (_ups, rest) = rest.split_once(' ')?;
    let (name, value) = rest.split_once(' ')?;
    Some((name.to_string(), unquote(value)))
}

/// Strip surrounding double-quotes and unescape `\"` / `\\`, per the protocol.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let inner = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_ups_line() {
        let (name, desc) = parse_ups_line(r#"UPS serverups "Rack UPS in closet""#).expect("parses");
        assert_eq!(name, "serverups");
        assert_eq!(desc, "Rack UPS in closet");
    }

    #[test]
    fn parses_var_line() {
        let (name, value) =
            parse_var_line(r#"VAR serverups battery.charge "100""#).expect("parses");
        assert_eq!(name, "battery.charge");
        assert_eq!(value, "100");
    }

    #[test]
    fn unquotes_escapes() {
        assert_eq!(unquote(r#""a \"b\" c""#), r#"a "b" c"#);
        assert_eq!(unquote("bare"), "bare");
    }

    /// A captured `LIST UPS` / `LIST VAR` session parses into a typed `Ups`.
    #[test]
    fn parses_captured_session_fixture() {
        // Body lines as `upsd` returns them between BEGIN/END markers.
        let ups_body = [r#"UPS serverups "Rack UPS""#];
        let var_body = [
            r#"VAR serverups ups.status "OB LB""#,
            r#"VAR serverups battery.charge "42""#,
            r#"VAR serverups battery.runtime "180""#,
            r#"VAR serverups input.voltage "0.0""#,
            r#"VAR serverups ups.model "Smart-UPS 1500""#,
        ];
        let names: Vec<(String, String)> =
            ups_body.iter().filter_map(|l| parse_ups_line(l)).collect();
        assert_eq!(names.len(), 1);
        let vars: BTreeMap<String, String> =
            var_body.iter().filter_map(|l| parse_var_line(l)).collect();
        let ups = Ups {
            name: names[0].0.clone(),
            description: names[0].1.clone(),
            vars,
        };
        assert_eq!(ups.status(), Some("OB LB"));
        assert_eq!(ups.battery_charge(), Some(42.0));
        assert_eq!(ups.battery_runtime(), Some(180.0));
        assert_eq!(ups.input_voltage(), Some(0.0));
        assert!(ups.on_battery());
    }

    #[test]
    fn on_battery_false_when_online() {
        let mut vars = BTreeMap::new();
        vars.insert("ups.status".to_string(), "OL".to_string());
        let ups = Ups {
            name: "u".into(),
            description: String::new(),
            vars,
        };
        assert!(!ups.on_battery());
    }
}
