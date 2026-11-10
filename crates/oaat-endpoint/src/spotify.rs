//! Spotify Connect control (spotifyd systemd unit) for the endpoint UI.
//! Linux-only; on other platforms reports "not active".

#[cfg(target_os = "linux")]
use std::process::Command;

const UNIT: &str = "spotifyd";

#[cfg(target_os = "linux")]
fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let mut full = vec![cmd];
    full.extend_from_slice(args);
    let out = Command::new(full[0]).args(&full[1..]).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(target_os = "linux")]
fn is_active() -> bool {
    sh("systemctl", &["is-active", UNIT]) == "active"
}

#[cfg(not(target_os = "linux"))]
fn is_active() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn unit_exists() -> bool {
    !sh("systemctl", &["cat", UNIT]).is_empty()
}

#[cfg(not(target_os = "linux"))]
fn unit_exists() -> bool {
    false
}

/// Extract `--device-name "..."` from the spotifyd unit file.
#[cfg(target_os = "linux")]
fn device_name() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/systemd/system/spotifyd.service") {
        for line in text.lines() {
            if let Some(idx) = line.find("--device-name")
                && let Some(q1) = line[idx..].find('"')
                && let Some(q2) = line[idx + q1 + 1..].find('"')
            {
                return line[idx + q1 + 1..idx + q1 + 1 + q2].to_string();
            }
        }
    }
    String::new()
}

#[cfg(not(target_os = "linux"))]
fn device_name() -> String {
    String::new()
}

pub fn spotify_status_json() -> String {
    format!(
        r#"{{"active":{},"unit_exists":{},"device_name":"{}"}}"#,
        is_active(),
        unit_exists(),
        device_name().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

#[cfg(target_os = "linux")]
pub fn spotify_start() -> Result<(), String> {
    run("sudo", &["-n", "systemctl", "start", UNIT])
}

#[cfg(not(target_os = "linux"))]
pub fn spotify_start() -> Result<(), String> {
    Err("not supported on this platform".into())
}

#[cfg(target_os = "linux")]
pub fn spotify_stop() -> Result<(), String> {
    run("sudo", &["-n", "systemctl", "stop", UNIT])
}

#[cfg(not(target_os = "linux"))]
pub fn spotify_stop() -> Result<(), String> {
    Err("not supported on this platform".into())
}
