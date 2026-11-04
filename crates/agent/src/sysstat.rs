//! Best-effort Linux resource sampling for the `Stat` heartbeat.
//!
//! Everything here is read from /proc and statvfs and nothing is fatal: a
//! container with a masked /proc reports zeros, which the hub and the panel
//! treat as "no data" rather than as a lie.

pub struct Sample {
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub procs: u32,
    pub uptime_sec: u64,
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Total cpu jiffies from /proc/stat: (busy, total).
fn cpu_jiffies() -> Option<(u64, u64)> {
    let s = read("/proc/stat")?;
    let line = s.lines().next()?;
    let mut it = line.split_whitespace().skip(1).filter_map(|v| v.parse::<u64>().ok());
    let user = it.next()?;
    let nice = it.next()?;
    let system = it.next()?;
    let idle = it.next()?;
    let iowait = it.next().unwrap_or(0);
    let irq = it.next().unwrap_or(0);
    let softirq = it.next().unwrap_or(0);
    let steal = it.next().unwrap_or(0);
    let busy = user + nice + system + irq + softirq + steal;
    let total = busy + idle + iowait;
    Some((busy, total))
}

/// CPU busy fraction since the previous call, per process.
///
/// The first sample has no predecessor and reports 0 — a number that is wrong
/// for one interval is better than a spike to 100% on every restart.
pub fn cpu_pct(prev: &mut Option<(u64, u64)>) -> f32 {
    let Some((busy, total)) = cpu_jiffies() else {
        return 0.0;
    };
    let pct = match *prev {
        Some((pb, pt)) if total > pt && busy >= pb => {
            let db = (busy - pb) as f64;
            let dt = (total - pt) as f64;
            ((db / dt) * 100.0) as f32
        }
        _ => 0.0,
    };
    *prev = Some((busy, total));
    pct.clamp(0.0, 100.0)
}

fn meminfo() -> (u64, u64) {
    let Some(s) = read("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut avail = 0u64;
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(val) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        match key {
            "MemTotal:" => total = val * 1024,
            "MemAvailable:" => avail = val * 1024,
            _ => {}
        }
    }
    (total.saturating_sub(avail), total)
}

fn disk() -> (u64, u64) {
    // SAFETY: statvfs on a NUL-terminated literal; the struct is plain old data.
    unsafe {
        let mut buf: libc::statvfs = std::mem::zeroed();
        let c = std::ffi::CString::new("/").unwrap();
        if libc::statvfs(c.as_ptr(), &mut buf) != 0 {
            return (0, 0);
        }
        let bsize = buf.f_frsize as u64;
        let total = bsize * buf.f_blocks as u64;
        let free = bsize * buf.f_bavail as u64;
        (total.saturating_sub(free), total)
    }
}

fn procs() -> u32 {
    let Some(s) = read("/proc/loadavg") else {
        return 0;
    };
    // field 4 is "running/total"; the running half is the useful one.
    s.split_whitespace()
        .nth(3)
        .and_then(|v| v.split('/').next())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
}

fn uptime() -> u64 {
    read("/proc/uptime")
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        .map(|v| v as u64)
        .unwrap_or(0)
}

pub fn collect_with(prev_cpu: &mut Option<(u64, u64)>) -> Sample {
    let (mem_used, mem_total) = meminfo();
    let (disk_used, disk_total) = disk();
    Sample {
        cpu_pct: cpu_pct(prev_cpu),
        mem_used,
        mem_total,
        disk_used,
        disk_total,
        procs: procs(),
        uptime_sec: uptime(),
    }
}
