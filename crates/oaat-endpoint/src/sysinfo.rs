//! System info for the endpoint settings UI (host, uptime, load, memory,
//! disk, temperature, IP). Linux-only; on other platforms returns empty
//! placeholders so the web UI still renders.

#[cfg(target_os = "linux")]
use std::process::Command;

#[derive(Default, Clone)]
pub struct SystemInfo {
    pub hostname: String,
    pub ip: String,
    pub uptime_sec: f64,
    pub load_1: f32,
    pub load_5: f32,
    pub load_15: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub temp_c: Option<f32>,
}

#[cfg(target_os = "linux")]
fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(target_os = "linux")]
fn parse_u64_kb(s: &str) -> u64 {
    s.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0)
}

#[cfg(target_os = "linux")]
pub fn collect() -> SystemInfo {
    let hostname = read("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let uptime_sec = read("/proc/uptime")
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0);

    let (load_1, load_5, load_15) = read("/proc/loadavg")
        .map(|la| {
            let mut it = la.split_whitespace();
            (
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0, 0.0));

    let (mem_total, mem_used) = read("/proc/meminfo")
        .map(|mem| {
            let mut total = 0u64;
            let mut avail = 0u64;
            for line in mem.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    total = parse_u64_kb(rest);
                } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    avail = parse_u64_kb(rest);
                }
            }
            (total * 1024, total.saturating_sub(avail) * 1024)
        })
        .unwrap_or((0, 0));

    // Raspberry Pi: millidegrees in /sys/class/thermal/thermal_zoneN/temp
    let temp_c = (0..4).find_map(|i| {
        read(&format!("/sys/class/thermal/thermal_zone{i}/temp"))
            .and_then(|t| t.trim().parse::<f32>().ok())
            .map(|milli| milli / 1000.0)
    });

    let (disk_total, disk_used) = Command::new("df")
        .args(["-B1", "--output=size,used", "/"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            let line = text.lines().nth(1)?;
            let mut it = line.split_whitespace();
            Some((
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            ))
        })
        .unwrap_or((0, 0));

    // Outbound IP: the interface holding the default route.
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            sock.connect("8.8.8.8:80")?;
            sock.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_default();

    SystemInfo {
        hostname,
        ip,
        uptime_sec,
        load_1,
        load_5,
        load_15,
        mem_total,
        mem_used,
        disk_total,
        disk_used,
        temp_c,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn collect() -> SystemInfo {
    SystemInfo {
        hostname: "unknown".into(),
        ..Default::default()
    }
}

fn jstr(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn system_json(info: &SystemInfo) -> String {
    format!(
        r#"{{"hostname":{},"ip":{},"uptime_sec":{},"load_1":{},"load_5":{},"load_15":{},"mem_total":{},"mem_used":{},"disk_total":{},"disk_used":{},"temp_c":{},"os":{},"arch":{}}}"#,
        jstr(&info.hostname),
        jstr(&info.ip),
        info.uptime_sec,
        info.load_1,
        info.load_5,
        info.load_15,
        info.mem_total,
        info.mem_used,
        info.disk_total,
        info.disk_used,
        info.temp_c.map(|t| t.to_string()).unwrap_or_else(|| "null".into()),
        jstr(std::env::consts::OS),
        jstr(std::env::consts::ARCH),
    )
}
