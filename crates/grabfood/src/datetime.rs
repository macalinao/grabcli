//! Tiny UTC→PHT (UTC+8) date math, no external date crate.
//!
//! Enough to turn an order's RFC3339 `createdAt` into Philippine local date,
//! time, year, month, hour and weekday for note naming and the index stats.

/// A civil date-time in Philippine local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pht {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

// Days from civil (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Pht {
    /// Parse `YYYY-MM-DDTHH:MM:SSZ` (UTC) and shift to PHT.
    pub fn from_rfc3339_utc(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() < 19 {
            return None;
        }
        let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
        let y = num(0, 4)?;
        let mo = num(5, 7)? as u32;
        let d = num(8, 10)? as u32;
        let h = num(11, 13)? as u32;
        let mi = num(14, 16)? as u32;
        let sec = num(17, 19)? as u32;

        // To epoch seconds, add 8h, back to civil.
        let epoch = days_from_civil(y, mo, d) * 86400 + (h * 3600 + mi * 60 + sec) as i64;
        let pht = epoch + 8 * 3600;
        let days = pht.div_euclid(86400);
        let rem = pht.rem_euclid(86400);
        let (yy, mm, dd) = civil_from_days(days);
        Some(Pht {
            year: yy,
            month: mm,
            day: dd,
            hour: (rem / 3600) as u32,
            minute: (rem % 3600 / 60) as u32,
        })
    }

    /// `HH:MM`.
    pub fn hhmm(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// `YYYY-MM-DD`.
    pub fn ymd(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `YYYY-MM-DD-HHMM` for note filenames.
    pub fn file_stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}-{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }

    /// `Mon D, YYYY` (abbreviated month, no zero padding) for titles.
    pub fn pretty(&self) -> String {
        const MON: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let m = MON.get((self.month - 1) as usize).copied().unwrap_or("?");
        format!("{m} {}, {:04}", self.day, self.year)
    }

    /// Weekday abbreviation (Mon..Sun).
    pub fn weekday(&self) -> &'static str {
        const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        // 1970-01-01 was a Thursday (day 0).
        let days = days_from_civil(self.year, self.month, self.day);
        WD[days.rem_euclid(7) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_to_pht_same_day() {
        let p = Pht::from_rfc3339_utc("2026-10-14T11:56:21Z").expect("parse");
        assert_eq!(p.ymd(), "2026-10-14");
        assert_eq!(p.hhmm(), "19:56");
        assert_eq!(p.file_stamp(), "2026-10-14-1956");
        assert_eq!(p.pretty(), "Oct 14, 2026");
    }

    #[test]
    fn utc_to_pht_day_rollover() {
        // 18:30 UTC -> 02:30 next day PHT.
        let p = Pht::from_rfc3339_utc("2026-01-31T18:30:00Z").expect("parse");
        assert_eq!(p.ymd(), "2026-02-01");
        assert_eq!(p.hhmm(), "02:30");
    }

    #[test]
    fn weekday_known() {
        // 2026-10-14 is a Wednesday.
        let p = Pht::from_rfc3339_utc("2026-10-14T01:00:00Z").expect("parse");
        assert_eq!(p.weekday(), "Wed");
    }
}
