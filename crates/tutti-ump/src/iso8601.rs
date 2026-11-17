//! The ISO 8601 date with an optional time that a Recording/Concert Date
//! carries (M2-104 section 7.5.9.2).

/// Whether `text` is an ISO 8601 date with an optional time: see
/// [`Iso8601Date`](crate::Iso8601Date)'s `FromStr` for the forms.
pub(crate) fn is_date_with_optional_time(text: &str) -> bool {
    let b = text.as_bytes();
    let Some(year) = digits(b, 0, 4) else {
        return false;
    };
    if b.len() == 4 {
        return true;
    }
    let extended = b[4] == b'-';
    let at = if extended { 5 } else { 4 };
    let Some((end, to_the_day)) = date(b, at, extended, year) else {
        return false;
    };
    end == b.len() || (to_the_day && time(&b[end..], extended))
}

/// The date after the year, from `at`: where it ends, and whether it is to
/// the day, which a time may follow.
fn date(b: &[u8], at: usize, extended: bool, year: u32) -> Option<(usize, bool)> {
    if b.get(at) == Some(&b'W') {
        let week = digits(b, at + 1, 2)?;
        if !(1..=weeks_in(year)).contains(&week) {
            return None;
        }
        let day_at = at + 3 + usize::from(extended);
        if extended && b.get(at + 3) != Some(&b'-') {
            return Some((at + 3, false));
        }
        return match digits(b, day_at, 1) {
            Some(day) if (1..=7).contains(&day) => Some((day_at + 1, true)),
            Some(_) => None,
            None if !extended => Some((at + 3, false)),
            None => None,
        };
    }
    let run = b[at..].iter().take_while(|d| d.is_ascii_digit()).count();
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match (extended, run) {
        (true, 3) | (false, 3) => {
            let day = digits(b, at, 3)?;
            (1..=365 + u32::from(leap))
                .contains(&day)
                .then_some((at + 3, true))
        }
        (true, 2) if b.get(at + 2) != Some(&b'-') => {
            let month = digits(b, at, 2)?;
            (1..=12).contains(&month).then_some((at + 2, false))
        }
        (true, 2) => {
            let (month, day) = (digits(b, at, 2)?, digits(b, at + 3, 2)?);
            day_in_month(leap, month, day).then_some((at + 5, true))
        }
        (false, 4) => {
            let (month, day) = (digits(b, at, 2)?, digits(b, at + 2, 2)?);
            day_in_month(leap, month, day).then_some((at + 4, true))
        }
        _ => None,
    }
}

fn day_in_month(leap: bool, month: u32, day: u32) -> bool {
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

/// The weeks of the ISO 8601 week year `year`: 53 when it starts on a
/// Thursday, or on a Wednesday in a leap year, else 52.
fn weeks_in(year: u32) -> u32 {
    let p = |y: u32| (y + y / 4 - y / 100 + y / 400) % 7;
    if p(year) == 4 || (year > 0 && p(year - 1) == 3) {
        53
    } else {
        52
    }
}

/// `n` ASCII digits of `b` from `at`, as a number.
fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let part = b.get(at..at + n)?;
    part.iter()
        .all(u8::is_ascii_digit)
        .then(|| part.iter().fold(0, |v, d| v * 10 + u32::from(d - b'0')))
}

/// `T` then a time of day and an optional offset, to the end of `b`. Hour 24
/// is the end of the day, with nothing after it but zeros.
fn time(b: &[u8], extended: bool) -> bool {
    let [b'T', b @ ..] = b else {
        return false;
    };
    let Some(hour) = digits(b, 0, 2).filter(|&h| h <= 24) else {
        return false;
    };
    let mut at = 2;
    let mut rest_is_zero = true;
    for max in [59, 60] {
        match component(b, at, extended, max) {
            Some((next, value)) => {
                at = next;
                rest_is_zero &= value == 0;
            }
            None => break,
        }
    }
    if matches!(b.get(at), Some(b'.' | b',')) {
        let fraction = &b[at + 1..];
        let count = fraction.iter().take_while(|d| d.is_ascii_digit()).count();
        if count == 0 {
            return false;
        }
        rest_is_zero &= fraction[..count].iter().all(|&d| d == b'0');
        at += 1 + count;
    }
    (hour < 24 || rest_is_zero) && offset(&b[at..], extended)
}

/// The two-digit component of `b` at `at` (behind a colon in the extended
/// form) that is at most `max`: where it ends and its value.
fn component(b: &[u8], at: usize, extended: bool, max: u32) -> Option<(usize, u32)> {
    let start = at + usize::from(extended);
    if extended && b.get(at) != Some(&b':') {
        return None;
    }
    digits(b, start, 2)
        .filter(|&v| v <= max)
        .map(|v| (start + 2, v))
}

/// Nothing, `Z`, or `+hh`, `+hh:mm`, `+hhmm` (or `-`), to the end.
fn offset(b: &[u8], extended: bool) -> bool {
    match b {
        [] | [b'Z'] => true,
        [b'+' | b'-', rest @ ..] => {
            if !digits(rest, 0, 2).is_some_and(|hour| hour <= 23) {
                return false;
            }
            match &rest[2..] {
                [] => true,
                [b':', ..] if extended => {
                    digits(rest, 3, 2).is_some_and(|m| m < 60) && rest.len() == 5
                }
                _ if !extended => digits(rest, 2, 2).is_some_and(|m| m < 60) && rest.len() == 4,
                _ => false,
            }
        }
        _ => false,
    }
}
