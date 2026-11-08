//! Time zones, as POSIX `TZ` rules ("EST5EDT,M3.2.0,M11.1.0": a standard
//! name and offset, then optionally a summer one and when it starts and
//! ends).
//!
//! The system's zone is `/etc/localtime`, a TZif file (RFC 8536) like
//! every Linux system's, so programs built for Linux (Firefox, through
//! musl or glibc) show the same time as ours. TZif files end with exactly
//! such a rule for the times after their table of past changes, and that
//! rule is all we read. `eli` writes the file from system.conf's
//! `timezone` setting: the real one from Alpine's `tzdata` when that's
//! installed, otherwise one it makes from [`rule_for`], a short list of
//! common zones' current rules. Past changes don't matter for a clock.
//!
//! No system calls here: [`crate::time`] finds the zone.

use alloc::string::String;
use alloc::vec::Vec;

/// When summer time starts or ends: a day of the year, and a time of day
/// (seconds, local time; may be negative or past 24 hours).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Day {
    /// `Mm.w.d`: weekday `d` (0 = Sunday) of week `w` (1-4; 5 = the last)
    /// of month `m`.
    Weekday { month: u32, week: u32, weekday: u32 },
    /// `Jn`: day 1-365, never counting February 29.
    Julian(u32),
    /// `n`: day 0-365, counting February 29.
    Zero(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Summer {
    name: String,
    /// Seconds east of UTC.
    offset: i64,
    start: (Day, i64),
    end: (Day, i64),
}

/// A parsed rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    name: String,
    /// Seconds east of UTC (POSIX writes west: "EST5" is -5 hours).
    offset: i64,
    summer: Option<Summer>,
}

impl Zone {
    pub fn utc() -> Self {
        Zone { name: "UTC".into(), offset: 0, summer: None }
    }

    /// Parses a POSIX rule; `None` if it isn't one.
    pub fn parse(rule: &str) -> Option<Self> {
        let mut rest = rule.trim().as_bytes();
        let name = abbreviation(&mut rest)?;
        let offset = -signed_time(&mut rest)?;
        if rest.is_empty() {
            return Some(Zone { name, offset, summer: None });
        }
        let summer_name = abbreviation(&mut rest)?;
        let summer_offset =
            if rest.first().is_some_and(|&c| c != b',') { -signed_time(&mut rest)? } else { offset + 3600 };
        // Without dates, POSIX leaves them to the implementation; US rules
        // are what glibc and musl use.
        let (start, end) = if rest.is_empty() {
            (
                (Day::Weekday { month: 3, week: 2, weekday: 0 }, 7200),
                (Day::Weekday { month: 11, week: 1, weekday: 0 }, 7200),
            )
        } else {
            rest = rest.strip_prefix(b",")?;
            let start = change(&mut rest)?;
            rest = rest.strip_prefix(b",")?;
            let end = change(&mut rest)?;
            (start, end)
        };
        if !rest.is_empty() {
            return None;
        }
        Some(Zone { name, offset, summer: Some(Summer { name: summer_name, offset: summer_offset, start, end }) })
    }

    /// The offset from UTC (seconds east) and abbreviation in force at
    /// `time` (seconds since 1970, UTC).
    pub fn at(&self, time: i64) -> (i64, &str) {
        let Some(summer) = &self.summer else { return (self.offset, &self.name) };
        let year = civil_from_days((time + self.offset).div_euclid(86_400)).0;
        // The start is given in standard time, the end in summer time.
        // Look at this year's and the neighbouring years' changes, for
        // times near New Year (and southern summers spanning it).
        let mut latest: Option<(i64, bool)> = None;
        for year in year - 1..=year + 1 {
            let starts = start_of_day(year, summer.start.0) + summer.start.1 - self.offset;
            let ends = start_of_day(year, summer.end.0) + summer.end.1 - summer.offset;
            for (at, is_summer) in [(starts, true), (ends, false)] {
                if at <= time && latest.is_none_or(|(when, _)| at > when) {
                    latest = Some((at, is_summer));
                }
            }
        }
        if latest.is_some_and(|(_, is_summer)| is_summer) {
            (summer.offset, &summer.name)
        } else {
            (self.offset, &self.name)
        }
    }

    /// A TZif file with no past changes, only this rule (which musl and
    /// glibc follow for every time after the file's one change, at 1970).
    pub fn tzif(&self, rule: &str) -> Vec<u8> {
        let mut file = Vec::new();
        let mut names = Vec::new();
        names.extend_from_slice(self.name.as_bytes());
        names.push(0);
        // Version 1 data (32-bit times), then version 2 (64-bit): both say
        // one change, at 0, to the standard offset.
        for size in [4, 8] {
            file.extend_from_slice(b"TZif2");
            file.extend_from_slice(&[0; 15]);
            for count in [0, 0, 0, 1, 1, names.len() as u32] {
                file.extend_from_slice(&count.to_be_bytes());
            }
            file.extend_from_slice(&[0; 8][..size]);
            file.push(0);
            file.extend_from_slice(&(self.offset as i32).to_be_bytes());
            file.extend_from_slice(&[0, 0]);
            file.extend_from_slice(&names);
        }
        file.push(b'\n');
        file.extend_from_slice(rule.trim().as_bytes());
        file.push(b'\n');
        file
    }
}

/// The rule at the end of a TZif file (version 2 and later have one).
pub fn rule_of_tzif(file: &[u8]) -> Option<&str> {
    if !file.starts_with(b"TZif") || file.last() != Some(&b'\n') {
        return None;
    }
    let body = &file[..file.len() - 1];
    let start = body.iter().rposition(|&c| c == b'\n')? + 1;
    core::str::from_utf8(&body[start..]).ok()
}

/// Current rules of common zones, for when Alpine's `tzdata` isn't
/// installed. Names are the IANA database's.
const ZONES: &[(&str, &str)] = &[
    ("UTC", "UTC0"),
    ("Etc/UTC", "UTC0"),
    ("America/New_York", "EST5EDT,M3.2.0,M11.1.0"),
    ("America/Detroit", "EST5EDT,M3.2.0,M11.1.0"),
    ("America/Indiana/Indianapolis", "EST5EDT,M3.2.0,M11.1.0"),
    ("America/Kentucky/Louisville", "EST5EDT,M3.2.0,M11.1.0"),
    ("America/Toronto", "EST5EDT,M3.2.0,M11.1.0"),
    ("America/Chicago", "CST6CDT,M3.2.0,M11.1.0"),
    ("America/Denver", "MST7MDT,M3.2.0,M11.1.0"),
    ("America/Phoenix", "MST7"),
    ("America/Los_Angeles", "PST8PDT,M3.2.0,M11.1.0"),
    ("America/Anchorage", "AKST9AKDT,M3.2.0,M11.1.0"),
    ("America/Halifax", "AST4ADT,M3.2.0,M11.1.0"),
    ("America/Mexico_City", "CST6"),
    ("America/Bogota", "<-05>5"),
    ("America/Lima", "<-05>5"),
    ("America/Sao_Paulo", "<-03>3"),
    ("America/Argentina/Buenos_Aires", "<-03>3"),
    ("Pacific/Honolulu", "HST10"),
    ("Europe/London", "GMT0BST,M3.5.0/1,M10.5.0"),
    ("Europe/Dublin", "GMT0IST,M3.5.0/1,M10.5.0"),
    ("Europe/Lisbon", "WET0WEST,M3.5.0/1,M10.5.0"),
    ("Europe/Paris", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Berlin", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Madrid", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Rome", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Amsterdam", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Brussels", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Vienna", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Zurich", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Stockholm", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Oslo", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Copenhagen", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Prague", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Warsaw", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Athens", "EET-2EEST,M3.5.0/3,M10.5.0/4"),
    ("Europe/Helsinki", "EET-2EEST,M3.5.0/3,M10.5.0/4"),
    ("Europe/Kyiv", "EET-2EEST,M3.5.0/3,M10.5.0/4"),
    ("Europe/Bucharest", "EET-2EEST,M3.5.0/3,M10.5.0/4"),
    ("Europe/Istanbul", "<+03>-3"),
    ("Europe/Moscow", "MSK-3"),
    ("Africa/Cairo", "EET-2EEST,M4.5.5/0,M10.5.4/24"),
    ("Africa/Lagos", "WAT-1"),
    ("Africa/Johannesburg", "SAST-2"),
    ("Africa/Nairobi", "EAT-3"),
    ("Asia/Jerusalem", "IST-2IDT,M3.4.4/26,M10.5.0"),
    ("Asia/Dubai", "<+04>-4"),
    ("Asia/Karachi", "PKT-5"),
    ("Asia/Kolkata", "IST-5:30"),
    ("Asia/Dhaka", "<+06>-6"),
    ("Asia/Bangkok", "<+07>-7"),
    ("Asia/Jakarta", "WIB-7"),
    ("Asia/Shanghai", "CST-8"),
    ("Asia/Hong_Kong", "HKT-8"),
    ("Asia/Taipei", "CST-8"),
    ("Asia/Singapore", "<+08>-8"),
    ("Asia/Manila", "PST-8"),
    ("Asia/Seoul", "KST-9"),
    ("Asia/Tokyo", "JST-9"),
    ("Australia/Perth", "AWST-8"),
    ("Australia/Adelaide", "ACST-9:30ACDT,M10.1.0,M4.1.0/3"),
    ("Australia/Brisbane", "AEST-10"),
    ("Australia/Sydney", "AEST-10AEDT,M10.1.0,M4.1.0/3"),
    ("Australia/Melbourne", "AEST-10AEDT,M10.1.0,M4.1.0/3"),
    ("Pacific/Auckland", "NZST-12NZDT,M9.5.0,M4.1.0/3"),
];

/// The zones [`rule_for`] knows, for choosing from.
pub fn known_zones() -> impl Iterator<Item = &'static str> {
    ZONES.iter().map(|&(name, _)| name).filter(|name| !name.starts_with("Etc/"))
}

/// The rule for a zone name, or the setting itself if it's already a
/// rule ("EST5EDT", "<+03>-3").
pub fn rule_for(setting: &str) -> Option<&str> {
    let setting = setting.trim();
    if let Some(&(_, rule)) = ZONES.iter().find(|(name, _)| *name == setting) {
        return Some(rule);
    }
    (!setting.contains('/') && Zone::parse(setting).is_some()).then_some(setting)
}

/// A zone abbreviation: letters, or anything between `<` and `>`.
fn abbreviation(rest: &mut &[u8]) -> Option<String> {
    let name = if let Some(quoted) = rest.strip_prefix(b"<") {
        let end = quoted.iter().position(|&c| c == b'>')?;
        let name = &quoted[..end];
        *rest = &quoted[end + 1..];
        name
    } else {
        let end = rest.iter().position(|c| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        let name = &rest[..end];
        *rest = &rest[end..];
        name
    };
    (name.len() >= 3).then(|| String::from_utf8_lossy(name).into_owned())
}

/// `[+-]hh[:mm[:ss]]`, in seconds.
fn signed_time(rest: &mut &[u8]) -> Option<i64> {
    let sign = match rest.first() {
        Some(b'-') => -1,
        _ => 1,
    };
    if matches!(rest.first(), Some(b'-' | b'+')) {
        *rest = &rest[1..];
    }
    let mut seconds = 0;
    for (index, scale) in [3600, 60, 1].into_iter().enumerate() {
        if index > 0 {
            match rest.strip_prefix(b":") {
                Some(after) => *rest = after,
                None => break,
            }
        }
        seconds += number(rest)? * scale;
    }
    Some(sign * seconds)
}

fn number(rest: &mut &[u8]) -> Option<i64> {
    let end = rest.iter().position(|c| !c.is_ascii_digit()).unwrap_or(rest.len());
    if end == 0 || end > 3 {
        return None;
    }
    let value = core::str::from_utf8(&rest[..end]).ok()?.parse().ok()?;
    *rest = &rest[end..];
    Some(value)
}

/// A change: a day, then optionally `/time` (default 02:00).
fn change(rest: &mut &[u8]) -> Option<(Day, i64)> {
    let day = if let Some(after) = rest.strip_prefix(b"M") {
        *rest = after;
        let month = number(rest)? as u32;
        *rest = rest.strip_prefix(b".")?;
        let week = number(rest)? as u32;
        *rest = rest.strip_prefix(b".")?;
        let weekday = number(rest)? as u32;
        if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
            return None;
        }
        Day::Weekday { month, week, weekday }
    } else if let Some(after) = rest.strip_prefix(b"J") {
        *rest = after;
        let day = number(rest)? as u32;
        (1..=365).contains(&day).then_some(Day::Julian(day))?
    } else {
        let day = number(rest)? as u32;
        (day <= 365).then_some(Day::Zero(day))?
    };
    let time = match rest.strip_prefix(b"/") {
        Some(after) => {
            *rest = after;
            signed_time(rest)?
        }
        None => 7200,
    };
    Some((day, time))
}

/// Seconds from 1970 to local midnight starting `day` of `year`.
fn start_of_day(year: i64, day: Day) -> i64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match day {
        Day::Weekday { month, week, weekday } => {
            let first = days_from_civil(year, month, 1);
            let first_weekday = (first + 4).rem_euclid(7);
            let mut date = first + (i64::from(weekday) - first_weekday).rem_euclid(7) + 7 * (i64::from(week) - 1);
            let next_month =
                if month == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, month + 1, 1) };
            while date >= next_month {
                date -= 7;
            }
            date
        }
        Day::Julian(n) => days_from_civil(year, 1, 1) + i64::from(n) - 1 + i64::from(leap && n >= 60),
        Day::Zero(n) => days_from_civil(year, 1, 1) + i64::from(n),
    };
    days * 86_400
}

/// Days since 1970 of a date (Howard Hinnant's days_from_civil).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day since 1970: (year, month, day).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}
