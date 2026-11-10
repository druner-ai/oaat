//! Wi-Fi control via NetworkManager (`nmcli`) for the endpoint settings UI.
//! Linux-only; on other platforms returns empty/unsupported.

#[cfg(target_os = "linux")]
use std::process::Command;

#[cfg(target_os = "linux")]
fn jstr(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Run a command, return trimmed stdout (empty on failure).
#[cfg(target_os = "linux")]
fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
pub fn network_json(rescan: bool) -> String {
    // current connection: ACTIVE,SSID,SIGNAL from `nmcli device wifi list`
    let mut ssid = String::new();
    let mut signal: Option<i32> = None;
    let list = sh(
        "nmcli",
        &["-t", "-f", "ACTIVE,SSID,SIGNAL", "device", "wifi", "list"],
    );
    for line in list.lines() {
        if line.starts_with("yes:") {
            let mut it = line.splitn(3, ':');
            let _ = it.next();
            ssid = it.next().unwrap_or("").to_string();
            signal = it.next().and_then(|v| v.trim().parse().ok());
            break;
        }
    }

    // current IP
    let ip = sh("hostname", &["-I"])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    // available networks: SSID,SIGNAL,SECURITY; дедупликация по SSID
    // (nmcli перечисляет каждый BSSID отдельно — оставляем сильнейший)
    let rescan_flag = if rescan { "yes" } else { "no" };
    let avail = sh(
        "nmcli",
        &[
            "-t",
            "-f",
            "SSID,SIGNAL,SECURITY",
            "device",
            "wifi",
            "list",
            "--rescan",
            rescan_flag,
        ],
    );
    let mut best: std::collections::BTreeMap<String, (i32, String)> =
        std::collections::BTreeMap::new();
    for line in avail.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut it = line.splitn(3, ':');
        let s = it.next().unwrap_or("").to_string();
        let sig = it
            .next()
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let sec = it.next().unwrap_or("").to_string();
        if s.is_empty() {
            continue;
        }
        best.entry(s)
            .and_modify(|e| {
                if sig > e.0 {
                    e.0 = sig;
                    e.1 = sec.clone();
                }
            })
            .or_insert((sig, sec));
    }
    let available = best
        .iter()
        .map(|(s, (sig, sec))| {
            format!(
                r#"{{"ssid":{},"signal":{},"security":{}}}"#,
                jstr(s),
                sig,
                jstr(sec)
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let connected = !ssid.is_empty();
    format!(
        r#"{{"current":{{"ssid":{},"signal":{},"ip":{},"connected":{}}},"available":[{}]}}"#,
        jstr(&ssid),
        signal.unwrap_or(0),
        jstr(&ip),
        connected,
        available
    )
}

#[cfg(not(target_os = "linux"))]
pub fn network_json(_rescan: bool) -> String {
    r#"{"current":{"ssid":"","signal":0,"ip":"","connected":false},"available":[]}"#.into()
}

/// Connect to a Wi-Fi network (open or WPA). Requires passwordless `sudo`
/// for `nmcli` on the endpoint.
#[cfg(target_os = "linux")]
pub fn connect_wifi(ssid: &str, password: &str) -> Result<(), String> {
    if ssid.is_empty() {
        return Err("empty SSID".into());
    }
    let mut args = vec!["-n", "nmcli", "device", "wifi", "connect", ssid];
    if !password.is_empty() {
        args.push("password");
        args.push(password);
    }
    let out = Command::new("sudo")
        .args(&args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn connect_wifi(_ssid: &str, _password: &str) -> Result<(), String> {
    Err("not supported on this platform".into())
}
