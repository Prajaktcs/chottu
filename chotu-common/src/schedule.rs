//! Agent wall-clock schedules from `config.yaml`.
//!
//! Times are civil `HH:MM` in the configured IANA timezone. Instants persisted
//! to the database remain RFC3339 UTC.

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// IANA tz database name used when `timezone` is omitted or invalid.
pub const DEFAULT_TIMEZONE: &str = "America/Toronto";

/// Parsed 24-hour clock time (`HH:MM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockTime {
    pub hour: u32,
    pub minute: u32,
}

impl ClockTime {
    pub fn matches(self, now: DateTime<Tz>) -> bool {
        now.hour() == self.hour && now.minute() == self.minute
    }
}

/// Proactive jobs. Omit a key or leave it blank (`""` / `~`) to disable that job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentSchedules {
    #[serde(default)]
    pub morning_brief: Option<String>,
    #[serde(default)]
    pub portfolio: Option<String>,
    #[serde(default)]
    pub reflection: Option<String>,
    #[serde(default)]
    pub health_evening_sync: Option<String>,
    #[serde(default)]
    pub health_late_steps: Option<String>,
}

impl AgentSchedules {
    pub fn morning_brief(&self) -> Option<ClockTime> {
        parse_schedule_slot("morning_brief", self.morning_brief.as_deref())
    }

    pub fn portfolio(&self) -> Option<ClockTime> {
        parse_schedule_slot("portfolio", self.portfolio.as_deref())
    }

    pub fn reflection(&self) -> Option<ClockTime> {
        parse_schedule_slot("reflection", self.reflection.as_deref())
    }

    pub fn health_evening_sync(&self) -> Option<ClockTime> {
        parse_schedule_slot("health_evening_sync", self.health_evening_sync.as_deref())
    }

    pub fn health_late_steps(&self) -> Option<ClockTime> {
        parse_schedule_slot("health_late_steps", self.health_late_steps.as_deref())
    }

    pub fn validation_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, raw) in [
            ("morning_brief", self.morning_brief.as_deref()),
            ("portfolio", self.portfolio.as_deref()),
            ("reflection", self.reflection.as_deref()),
            ("health_evening_sync", self.health_evening_sync.as_deref()),
            ("health_late_steps", self.health_late_steps.as_deref()),
        ] {
            if let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) {
                if parse_hhmm(s).is_none() {
                    out.push(format!(
                        "schedules.{name} `{s}` is not a valid 24-hour HH:MM; job disabled"
                    ));
                }
            }
        }
        out
    }
}

/// Parse `HH:MM` (24-hour). Empty / whitespace → `None` (job not scheduled).
pub fn parse_hhmm(raw: &str) -> Option<ClockTime> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (h, m) = raw.split_once(':')?;
    if m.contains(':') {
        return None;
    }
    let hour: u32 = h.parse().ok()?;
    let minute: u32 = m.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(ClockTime { hour, minute })
}

fn parse_schedule_slot(name: &str, raw: Option<&str>) -> Option<ClockTime> {
    let s = raw.map(str::trim).filter(|s| !s.is_empty())?;
    match parse_hhmm(s) {
        Some(t) => Some(t),
        None => {
            eprintln!(
                "Config warning: schedules.{name} `{s}` is not a valid 24-hour HH:MM; job disabled"
            );
            None
        }
    }
}

/// Parse an IANA tz database name (`America/Toronto`, `UTC`, …).
pub fn parse_iana_timezone(name: &str) -> Option<Tz> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    name.parse::<Tz>().ok()
}

/// Resolve agent timezone: config `timezone`, else `CHOTU_TIMEZONE`, else America/Toronto.
pub fn resolve_timezone_name(config_timezone: Option<&str>) -> String {
    if let Some(name) = config_timezone.map(str::trim).filter(|s| !s.is_empty()) {
        if parse_iana_timezone(name).is_some() {
            return name.to_string();
        }
        eprintln!(
            "Config warning: timezone `{name}` is not a valid IANA tz database name; \
             falling back"
        );
    }
    if let Ok(env_name) = std::env::var("CHOTU_TIMEZONE") {
        let env_name = env_name.trim();
        if parse_iana_timezone(env_name).is_some() {
            return env_name.to_string();
        }
        eprintln!(
            "Config warning: CHOTU_TIMEZONE `{env_name}` is not a valid IANA tz database name; \
             using {DEFAULT_TIMEZONE}"
        );
    }
    DEFAULT_TIMEZONE.to_string()
}

pub fn resolve_tz(config_timezone: Option<&str>) -> Tz {
    parse_iana_timezone(&resolve_timezone_name(config_timezone))
        .unwrap_or(chrono_tz::America::Toronto)
}

pub fn now_in_tz(tz: Tz) -> DateTime<Tz> {
    Utc::now().with_timezone(&tz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn blank_or_missing_clock_disables_job() {
        let empty = AgentSchedules::default();
        assert!(empty.morning_brief().is_none());
        let blank: AgentSchedules = serde_yaml::from_str(
            r#"
morning_brief: ""
portfolio: ~
reflection: "   "
"#,
        )
        .unwrap();
        assert!(blank.morning_brief().is_none());
        assert!(blank.portfolio().is_none());
        assert!(blank.reflection().is_none());
    }

    #[test]
    fn parses_hhmm() {
        let t = parse_hhmm("07:00").unwrap();
        assert_eq!(t, ClockTime { hour: 7, minute: 0 });
        assert!(parse_hhmm("24:00").is_none());
        assert_eq!(parse_hhmm("7:00").unwrap().hour, 7);
        let s: AgentSchedules = serde_yaml::from_str("health_evening_sync: \"20:45\"\n").unwrap();
        assert_eq!(
            s.health_evening_sync(),
            Some(ClockTime {
                hour: 20,
                minute: 45
            })
        );
    }

    #[test]
    fn iana_timezone_america_toronto() {
        assert_eq!(
            parse_iana_timezone("America/Toronto"),
            Some(chrono_tz::America::Toronto)
        );
        assert!(parse_iana_timezone("Americas/Toronto").is_none());
        assert!(parse_iana_timezone("Not/AZone").is_none());
        assert_eq!(resolve_timezone_name(Some("America/Toronto")), "America/Toronto");
        assert_eq!(resolve_timezone_name(Some("")), DEFAULT_TIMEZONE);
    }

    #[test]
    fn clock_matches_in_configured_zone() {
        let tz = chrono_tz::America::Toronto;
        let noon = tz.with_ymd_and_hms(2026, 8, 22, 12, 0, 0).unwrap();
        assert!(ClockTime {
            hour: 12,
            minute: 0
        }
        .matches(noon));
        assert!(!ClockTime {
            hour: 12,
            minute: 1
        }
        .matches(noon));
        let utc = noon.with_timezone(&Utc);
        assert_eq!(utc.hour(), 16); // EDT
    }
}
