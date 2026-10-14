//! When the phone is quiet, and whom it will answer.
//!
//! Three decisions that are pure functions of the clock and the configuration:
//! the ringing lock's window, the caller's number against the whitelist, and the
//! next wake-up moment. None of them touch the hardware or the SIP stack, which
//! is why they live here on their own rather than inside the state machine - and
//! why they can be pinned down by tests that run anywhere.

use chrono::{Datelike, Local, NaiveTime};

/// Is the ringing lock active at `hour`, given the configured `from`/`to` hour?
///
/// A zero on either side switches the lock off - that is what the schema says
/// about both options, and what `--ignore-dnd` writes into them.
///
/// Otherwise the window runs from `from` to `to`. The usual case is `from > to`:
/// the lock starts in the evening and runs past midnight into the morning, so
/// the two hours belong to different days and both ends of the day count. A
/// window that does *not* cross midnight is taken as a range inside one day.
///
/// The hour comparison has to include zero for that first case. The old form
/// spelled it `0 < hour && hour <= to || 0 < from && from <= hour`, where the
/// `0 < hour` guard was there to keep `to = 0` switched off - and it left the
/// hour between midnight and one o'clock outside an evening-to-morning window,
/// so the phone rang at half past midnight while the lock was supposed to be on.
/// Telling "switched off" apart by the bounds above leaves the hour free to be
/// compared as it is.
pub fn dnd_window_active(hour: i64, from: i64, to: i64) -> bool {
    if from == 0 || to == 0 {
        return false;
    }

    if from <= to {
        (from..=to).contains(&hour)
    } else {
        hour >= from || hour <= to
    }
}

/// Is `caller` among the configured numbers, in any of the usual spellings?
///
/// A caller is accepted if their own number is listed, or if the one other
/// spelling we can derive from it is. The same German number reaches the phone
/// as `0891234`, `+49891234` or `0049891234`, depending on the provider, and the
/// user should not have to list all three.
pub fn known_caller(caller: &str, configured: &[String]) -> bool {
    let alternative = if let Some(rest) = caller.strip_prefix("00") {
        // International format without plus sign: 0049891234 => +49891234
        format!("+{}", rest)
    } else if let Some(rest) = caller.strip_prefix('+') {
        // International format with plus sign: +49891234 => 0049891234
        format!("00{}", rest)
    } else if let Some(rest) = caller.strip_prefix('0') {
        // National format: 0891234 => +49891234. The country code is not
        // configurable, so it is fixed here.
        format!("+49{}", rest)
    } else {
        // Unknown format: no additional check.
        String::new()
    };

    configured.iter().any(|number| number == caller)
        || (!alternative.is_empty() && configured.iter().any(|number| *number == alternative))
}

/// Next wake-up time: today if it is still ahead, otherwise tomorrow.
///
/// `times` holds one entry per weekday, Monday first. A week that is not
/// complete is no schedule at all: without all seven days there is no next
/// wake-up to report, whatever day of the week it happens to be.
pub fn next_wake_up(times: &[String]) -> Option<chrono::NaiveDateTime> {
    if times.len() != 7 {
        return None;
    }

    let now = Local::now();
    let today = now.date_naive();

    let time_for = |date: chrono::NaiveDate| -> Option<NaiveTime> {
        let index = date.weekday().num_days_from_monday() as usize;
        times.get(index)?.parse::<NaiveTime>().ok()
    };

    if let Some(time) = time_for(today) {
        let candidate = today.and_time(time);
        if now.naive_local() < candidate {
            return Some(candidate);
        }
    }

    let tomorrow = today + chrono::Duration::days(1);
    Some(tomorrow.and_time(time_for(tomorrow)?))
}

/// Seconds from now until the given moment, never negative.
pub fn seconds_until(target: chrono::NaiveDateTime) -> u64 {
    let now = Local::now().naive_local();
    (target - now).num_seconds().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_todays_wake_up_when_still_ahead() {
        let now = Local::now();
        let later = now.format("%H:%M").to_string();
        let times: Vec<String> = std::iter::repeat(later).take(7).collect();

        match next_wake_up(&times) {
            Some(target) => assert!(target >= now.naive_local()),
            None => panic!("Uhrzeit sollte gültig sein"),
        }
    }

    #[test]
    fn moves_to_tomorrow_once_the_time_has_passed() {
        let times: Vec<String> = std::iter::repeat("00:00".to_string()).take(7).collect();
        let target = next_wake_up(&times).expect("gültige Uhrzeit");
        assert!(target > Local::now().naive_local());
    }

    #[test]
    fn rejects_incomplete_week() {
        // Every length used to be accepted on most days, because only today
        // and tomorrow were looked up: on a Sunday the Monday entry was still
        // in a one-element week and hid the missing weekdays.
        for len in [0, 1, 6, 8] {
            let times: Vec<String> = std::iter::repeat("06:30".to_string()).take(len).collect();
            assert!(next_wake_up(&times).is_none(), "{} Einträge sind keine Woche", len);
        }
    }

    #[test]
    fn seconds_until_is_never_negative() {
        let past = Local::now().naive_local() - chrono::Duration::hours(1);
        assert_eq!(seconds_until(past), 0);
    }

    // ---------------------------------------------------------------- ringing lock

    #[test]
    fn the_ringing_lock_is_off_when_either_end_is_zero() {
        // 0 means "switched off" on both options, and --ignore-dnd writes both.
        for (from, to) in [(0, 0), (0, 8), (20, 0)] {
            for hour in 0..24 {
                assert!(
                    !dnd_window_active(hour, from, to),
                    "Stunde {hour} darf bei from={from} to={to} nicht gesperrt sein"
                );
            }
        }
    }

    #[test]
    fn an_evening_window_covers_the_hours_around_midnight() {
        // The shipped example is 20 -> 8. Half past midnight used to fall through
        // both halves of the old condition and rang anyway.
        for hour in [20, 21, 23] {
            assert!(dnd_window_active(hour, 20, 8), "Stunde {hour} gehört zur Sperre");
        }
        for hour in [0, 1, 7, 8] {
            assert!(dnd_window_active(hour, 20, 8), "Stunde {hour} gehört zur Sperre");
        }
        for hour in [9, 12, 19] {
            assert!(!dnd_window_active(hour, 20, 8), "Stunde {hour} gehört nicht zur Sperre");
        }
    }

    #[test]
    fn a_window_inside_one_day_does_not_run_into_the_evening() {
        // from <= to means one day, so the second half of the old condition -
        // "from <= hour" on its own - must not reach past the window.
        for hour in [8, 12, 17] {
            assert!(dnd_window_active(hour, 8, 17), "Stunde {hour} gehört zur Sperre");
        }
        for hour in [0, 7, 18, 23] {
            assert!(!dnd_window_active(hour, 8, 17), "Stunde {hour} gehört nicht zur Sperre");
        }
    }

    #[test]
    fn a_window_of_one_hour_is_that_hour() {
        assert!(dnd_window_active(3, 3, 3));
        assert!(!dnd_window_active(2, 3, 3));
        assert!(!dnd_window_active(4, 3, 3));
    }

    // ---------------------------------------------------------------- whitelist

    /// The three spellings of a German number, as they arrive from a SIP server.
    const NATIONAL: &str = "0891234";
    const INTERNATIONAL: &str = "+49891234";
    const INTERNATIONAL_PLAIN: &str = "0049891234";

    fn known(caller: &str, configured: &[&str]) -> bool {
        let owned: Vec<String> = configured.iter().map(|n| n.to_string()).collect();
        known_caller(caller, &owned)
    }

    #[test]
    fn whitelist_accepts_an_exact_number() {
        assert!(known(NATIONAL, &[NATIONAL, INTERNATIONAL]));
    }

    #[test]
    fn whitelist_accepts_the_derived_spelling() {
        // Exactly one alternative is derived, and it depends on the caller's
        // own format: 00 and + are each other's alternative, while the national
        // format derives the international one.
        assert!(known(NATIONAL, &[INTERNATIONAL]));
        assert!(known(INTERNATIONAL, &[INTERNATIONAL_PLAIN]));
        assert!(known(INTERNATIONAL_PLAIN, &[INTERNATIONAL]));
    }

    #[test]
    fn whitelist_is_asymmetric_by_design() {
        // Only one alternative is derived, so a caller in international format
        // does not match a number stored nationally. Pinning this so the
        // behaviour is a decision rather than an accident.
        assert!(!known(INTERNATIONAL, &[NATIONAL]));
        assert!(!known(INTERNATIONAL_PLAIN, &[NATIONAL]));
    }

    #[test]
    fn whitelist_rejects_unknown_callers() {
        assert!(!known("+49899999", &[NATIONAL, INTERNATIONAL]));
        assert!(!known("12345", &[NATIONAL]));
    }

    #[test]
    fn whitelist_rejects_everything_when_empty() {
        for caller in [NATIONAL, INTERNATIONAL, INTERNATIONAL_PLAIN, "12345", ""] {
            assert!(!known(caller, &[]), "{} dürfte nicht durchkommen", caller);
        }
    }
}