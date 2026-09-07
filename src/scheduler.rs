use std::time::Duration;

use chrono::{DateTime, Local, Timelike};

pub fn next_boundary(now: DateTime<Local>, interval_minutes: u32) -> DateTime<Local> {
    let seconds_into_interval = (now.minute() % interval_minutes) * 60 + now.second();
    let nanos = now.nanosecond();
    let mut seconds = i64::from(interval_minutes * 60 - seconds_into_interval);
    if nanos == 0 && seconds_into_interval == 0 {
        seconds = i64::from(interval_minutes * 60);
    }
    now + chrono::Duration::seconds(seconds) - chrono::Duration::nanoseconds(i64::from(nanos))
}

pub fn duration_until(instant: DateTime<Local>) -> Duration {
    (instant - Local::now()).to_std().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::next_boundary;
    use chrono::{Local, TimeZone, Timelike};

    #[test]
    fn advances_to_next_half_hour() {
        let now = Local.with_ymd_and_hms(2026, 9, 7, 20, 17, 42).unwrap();
        let next = next_boundary(now, 30);
        assert_eq!((next.hour(), next.minute(), next.second()), (20, 30, 0));
    }

    #[test]
    fn an_exact_boundary_advances() {
        let now = Local.with_ymd_and_hms(2026, 9, 7, 21, 30, 0).unwrap();
        let next = next_boundary(now, 30);
        assert_eq!((next.hour(), next.minute(), next.second()), (22, 0, 0));
    }

    #[test]
    fn supports_configured_wall_clock_intervals() {
        let now = Local.with_ymd_and_hms(2026, 9, 7, 20, 17, 42).unwrap();
        let next = next_boundary(now, 15);
        assert_eq!((next.hour(), next.minute(), next.second()), (20, 30, 0));
    }
}
