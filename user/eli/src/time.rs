//! Clocks and sleeping.

use crate::sys;
pub use crate::zone::Zone;

fn clock(id: usize) -> u64 {
    let mut timespec = [0u64; 2];
    // SAFETY: room for a timespec.
    unsafe { sys::syscall(sys::CLOCK_GETTIME, [id, timespec.as_mut_ptr() as usize, 0, 0, 0, 0]) };
    timespec[0] * 1_000_000_000 + timespec[1]
}

/// Nanoseconds on a clock that never goes backwards (time since boot).
pub fn monotonic_nanos() -> u64 {
    clock(1)
}

/// Seconds since the Unix epoch.
pub fn unix_time() -> u64 {
    clock(0) / 1_000_000_000
}

pub fn sleep_ms(ms: u64) {
    let timespec = [ms / 1000, ms % 1000 * 1_000_000];
    // SAFETY: a valid timespec.
    unsafe { sys::syscall(sys::NANOSLEEP, [timespec.as_ptr() as usize, 0, 0, 0, 0, 0]) };
}

/// System statistics from `sysinfo`.
pub struct SystemInfo {
    pub uptime_seconds: u64,
    pub total_memory: u64,
    pub free_memory: u64,
    pub kernel_heap: u64,
    pub processes: u16,
}

pub fn system_info() -> SystemInfo {
    let mut info = [0u64; 14];
    // SAFETY: room for a `struct sysinfo`.
    unsafe { sys::syscall(sys::SYSINFO, [info.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) };
    SystemInfo {
        uptime_seconds: info[0],
        total_memory: info[4],
        free_memory: info[5],
        kernel_heap: info[7],
        processes: info[10] as u16,
    }
}

/// The system's time zone: `TZ` if it's set (a rule, or `:path` or a path
/// to a TZif file, as musl and glibc take it), else /etc/localtime; UTC if
/// neither says. Programs showing the time for long should look again now
/// and then, so a changed setting shows.
pub fn local_zone() -> Zone {
    let from_file = |path: &str| {
        let file = crate::fs::read(path).ok()?;
        Zone::parse(crate::zone::rule_of_tzif(&file)?)
    };
    if let Some(tz) = crate::env::var("TZ").filter(|tz| !tz.is_empty()) {
        let path = tz.strip_prefix(':').unwrap_or(&tz);
        let zone = if path.starts_with('/') {
            from_file(path)
        } else {
            Zone::parse(&tz).or_else(|| from_file(&alloc::format!("/usr/share/zoneinfo/{path}")))
        };
        if let Some(zone) = zone {
            return zone;
        }
    }
    from_file("/etc/localtime").unwrap_or_else(Zone::utc)
}

/// A calendar date and time.
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday.
    pub weekday: u32,
}

impl DateTime {
    /// In UTC.
    pub fn from_unix(seconds: u64) -> Self {
        Self::from_seconds(seconds as i64)
    }

    /// In the system's time zone ([`local_zone`]).
    pub fn local(seconds: i64) -> Self {
        Self::in_zone(seconds, &local_zone()).0
    }

    /// In a time zone, with the zone's abbreviation ("EST").
    pub fn in_zone(seconds: i64, zone: &Zone) -> (Self, alloc::string::String) {
        let (offset, name) = zone.at(seconds);
        (Self::from_seconds(seconds + offset), name.into())
    }

    fn from_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        let (year, month, day) = crate::zone::civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (rest / 3600) as u32,
            minute: (rest / 60 % 60) as u32,
            second: (rest % 60) as u32,
            weekday: ((days + 4).rem_euclid(7)) as u32,
        }
    }
}
