//! Clocks and sleeping.

use crate::sys;

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

/// A calendar date and time (UTC).
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
    pub fn from_unix(seconds: u64) -> Self {
        let days = (seconds / 86_400) as i64;
        let rest = seconds % 86_400;
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z - era * 146_097;
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_index = (5 * day_of_year + 2) / 153;
        let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
        let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
        let year = year_of_era + era * 400 + i64::from(month <= 2);
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
