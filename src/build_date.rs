//! The build date Settings > About shows ("25 Oct 2026"). `build.rs` works it out once per build
//! with this file and hands it to the app as `TRUFFLE_BUILD_DATE`; the app's tests check it here.

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "25 Oct 2026" for a day counted from 1 January 1970.
pub fn date_text(days: i64) -> String {
    let (year, month, day) = civil(days);
    format!("{day} {} {year}", MONTHS[month as usize - 1])
}

/// The year, month (1-12) and day (1-31) of a day counted from 1 January 1970, in the calendar we
/// use (Howard Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_become_dates() {
        assert_eq!(date_text(0), "1 Jan 1970");
        assert_eq!(date_text(-1), "31 Dec 1969");
        assert_eq!(date_text(11_017), "1 Mar 2000");
        assert_eq!(date_text(19_782), "29 Feb 2024");
        assert_eq!(date_text(20_751), "25 Oct 2026");
    }

    /// What this build says: a day without a leading zero, a short month and the year.
    #[test]
    fn the_build_date_reads_like_25_oct_2026() {
        let date = env!("TRUFFLE_BUILD_DATE");
        let parts: Vec<&str> = date.split(' ').collect();
        assert_eq!(parts.len(), 3, "{date}");
        let day: u32 = parts[0].parse().expect(date);
        assert!((1..=31).contains(&day) && !parts[0].starts_with('0'), "{date}");
        assert!(MONTHS.contains(&parts[1]), "{date}");
        assert!(parts[2].len() == 4 && parts[2].parse::<u32>().is_ok_and(|y| y >= 2026), "{date}");
    }
}
