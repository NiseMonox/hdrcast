//! Local wall-clock time for the log.

use std::mem::MaybeUninit;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hours, minutes, seconds and milliseconds of the local time of day (UTC when the time zone cannot be
/// applied).
pub(crate) fn local_time_of_day() -> (u32, u32, u32, u32) {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = since_epoch.subsec_millis();
    let seconds = since_epoch.as_secs();

    let time = seconds as libc::time_t;
    let mut local = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are valid for the call; localtime_r is the re-entrant variant and writes only
    // into the structure it is given.
    let converted = unsafe { !libc::localtime_r(&time, local.as_mut_ptr()).is_null() };
    if converted {
        // SAFETY: localtime_r succeeded, so it filled the structure (and an all-zero `tm` is valid anyway).
        let local = unsafe { local.assume_init() };
        return (
            local.tm_hour.clamp(0, 23) as u32,
            local.tm_min.clamp(0, 59) as u32,
            // a leap second is shown as :59
            local.tm_sec.clamp(0, 59) as u32,
            millis,
        );
    }
    let of_day = seconds % 86400;
    (
        (of_day / 3600) as u32,
        (of_day / 60 % 60) as u32,
        (of_day % 60) as u32,
        millis,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_of_day_is_in_range() {
        let (h, m, s, ms) = local_time_of_day();
        assert!(h < 24 && m < 60 && s < 60 && ms < 1000, "{h}:{m}:{s}.{ms}");
    }
}
