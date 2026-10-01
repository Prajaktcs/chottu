//! Configured civil dates mapped to half-open UTC database windows.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;

pub fn civil_day_bounds_utc(date: &str, timezone: Tz) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let next = day
        .succ_opt()
        .ok_or_else(|| anyhow!("Civil date overflow"))?;
    let boundary = |day: NaiveDate| -> Result<DateTime<Utc>> {
        let midnight = day.and_hms_opt(0, 0, 0).unwrap();
        // Midnight gaps start at the first valid instant; a skipped date is empty.
        for second in 0..=86_400 {
            if let Some(local) = timezone
                .from_local_datetime(&(midnight + Duration::seconds(second)))
                .earliest()
            {
                return Ok(local.with_timezone(&Utc));
            }
        }
        Err(anyhow!("No valid civil day boundary"))
    };
    Ok((boundary(day)?, boundary(next)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_windows_cover_dst_and_skipped_dates() {
        let toronto = chrono_tz::America::Toronto;
        for (date, hours) in [("2026-03-08", 23), ("2026-11-01", 25)] {
            let (start, end) = civil_day_bounds_utc(date, toronto).unwrap();
            assert_eq!((end - start).num_hours(), hours);
        }
        let (start, end) =
            civil_day_bounds_utc("2026-09-06", chrono_tz::America::Santiago).unwrap();
        assert_eq!(start.to_rfc3339(), "2026-09-06T04:00:00+00:00");
        assert_eq!((end - start).num_hours(), 23);
        let (start, end) = civil_day_bounds_utc("2011-12-30", chrono_tz::Pacific::Apia).unwrap();
        assert_eq!(start, end);
    }
}
