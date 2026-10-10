//! The CMOS real-time clock, read once at boot for the wall-clock time.

use crate::arch::{inb, outb};

fn read_register(register: u8) -> u8 {
    // SAFETY: CMOS registers are read by selecting one on port 0x70 and
    // reading port 0x71.
    unsafe {
        outb(0x70, register);
        inb(0x71)
    }
}

fn update_in_progress() -> bool {
    read_register(0x0A) & 0x80 != 0
}

fn read_raw() -> [u8; 6] {
    while update_in_progress() {
        core::hint::spin_loop();
    }
    [0x00, 0x02, 0x04, 0x07, 0x08, 0x09].map(read_register)
}

/// The current time as seconds since the Unix epoch (the clock is assumed
/// to be set to UTC, as on most systems that aren't Windows).
pub fn read_unix_time() -> u64 {
    // Read until two reads agree, so we don't catch it mid-update.
    let mut values = read_raw();
    loop {
        let again = read_raw();
        if again == values {
            break;
        }
        values = again;
    }
    let status_b = read_register(0x0B);
    let binary = status_b & 0x04 != 0;
    let decode = |value: u8| if binary { value } else { (value & 0x0F) + (value >> 4) * 10 };
    let [second, minute, hour, day, month, year] = values;
    let mut hour_value = decode(hour & 0x7F);
    // 12-hour mode keeps PM in the top bit.
    if status_b & 0x02 == 0 && hour & 0x80 != 0 {
        hour_value = (hour_value % 12) + 12;
    }
    let year = 2000 + i64::from(decode(year));
    let days = days_from_civil(year, i64::from(decode(month)), i64::from(decode(day)));
    (days * 86_400 + i64::from(hour_value) * 3600 + i64::from(decode(minute)) * 60 + i64::from(decode(second))) as u64
}

/// Days since 1970-01-01 for a date in the proleptic Gregorian calendar
/// (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn civil_dates_convert_to_unix_days() {
            assert_eq!(days_from_civil(1970, 1, 1), 0);
            assert_eq!(days_from_civil(2000, 3, 1), 11_017);
            assert_eq!(days_from_civil(2026, 10, 10), 20_736);
        }

        fn the_clock_reads_a_plausible_time() {
            // After 2024 and before 2100.
            let now = read_unix_time();
            assert!((1_704_067_200..4_102_444_800).contains(&now), "{now}");
        }
    }
}
