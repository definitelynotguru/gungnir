use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use super::{Candidate, Query};
use crate::EntryId;

struct DateIndex {
    by_date: HashMap<NaiveDate, Vec<EntryId>>,
}

impl DateIndex {
    fn build(pool: &[Candidate]) -> Self {
        let mut by_date: HashMap<NaiveDate, Vec<EntryId>> = HashMap::new();
        for candidate in pool {
            for text in [&candidate.entry.summary, &candidate.entry.body] {
                for date in dates_in_text(text, Some(candidate.entry.timestamp)) {
                    let ids = by_date.entry(date).or_default();
                    if !ids.contains(&candidate.entry.id) {
                        ids.push(candidate.entry.id);
                    }
                }
            }
        }
        Self { by_date }
    }

    fn matches(&self, requested: &BTreeSet<NaiveDate>) -> HashSet<EntryId> {
        requested
            .iter()
            .filter_map(|date| self.by_date.get(date))
            .flatten()
            .copied()
            .collect()
    }
}

fn dates_in_text(text: &str, anchor: Option<DateTime<Utc>>) -> BTreeSet<NaiveDate> {
    let mut dates = absolute_dates(text)
        .into_iter()
        .map(|(_, date)| date)
        .collect::<BTreeSet<_>>();
    if let Some(anchor) = anchor {
        dates.extend(relative_dates(text, anchor.date_naive()));
    }
    dates
}

fn absolute_dates(text: &str) -> Vec<(Range<usize>, NaiveDate)> {
    let bytes = text.as_bytes();
    let mut dates = Vec::new();
    let mut start = 0;
    while start + 10 <= bytes.len() {
        if !text.is_char_boundary(start) {
            start += 1;
            continue;
        }
        let end = start + 10;
        let shape = bytes[start..end]
            .iter()
            .enumerate()
            .all(|(offset, byte)| match offset {
                4 | 7 => *byte == b'-',
                _ => byte.is_ascii_digit(),
            });
        if shape {
            let bounded = !text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
                && !text[end..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            if bounded {
                if let Ok(date) = NaiveDate::parse_from_str(&text[start..end], "%Y-%m-%d") {
                    dates.push((start..end, date));
                    start = end;
                    continue;
                }
            }
        }
        start += 1;
    }
    dates
}

fn relative_dates(text: &str, anchor: NaiveDate) -> BTreeSet<NaiveDate> {
    let words = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '-')
                .to_lowercase()
        })
        .collect::<Vec<_>>();
    words
        .windows(3)
        .filter_map(|phrase| {
            let count = relative_count(&phrase[0])?;
            let days = match (phrase[1].as_str(), count) {
                ("day", 1) | ("days", 2..) => count,
                ("week", 1) | ("weeks", 2..) => count.checked_mul(7)?,
                _ => return None,
            };
            if phrase[2] != "ago" {
                return None;
            }
            anchor.checked_sub_signed(Duration::try_days(days)?)
        })
        .collect()
}

fn relative_count(word: &str) -> Option<i64> {
    match word {
        "a" | "an" | "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        "ten" => Some(10),
        "eleven" => Some(11),
        "twelve" => Some(12),
        _ => word
            .parse::<i64>()
            .ok()
            .filter(|count| *count > 0 && word.chars().all(|c| c.is_ascii_digit())),
    }
}

pub(super) fn select_date_matches(pool: &[Candidate], query: &Query) -> Option<HashSet<EntryId>> {
    let requested = dates_in_text(&query.text, query.question_time);
    if requested.is_empty() {
        return None;
    }
    let matched = DateIndex::build(pool).matches(&requested);
    (!matched.is_empty()).then_some(matched)
}

pub(super) fn mask_absolute_dates(text: &str) -> String {
    let spans = absolute_dates(text);
    let mut masked = String::with_capacity(text.len());
    let mut copied = 0;
    for (span, _) in spans {
        masked.push_str(&text[copied..span.start]);
        masked.push_str(&" ".repeat(span.len()));
        copied = span.end;
    }
    masked.push_str(&text[copied..]);
    masked
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn anchor() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap()
    }

    #[test]
    fn absolute_dates_validate_calendars_and_boundaries() {
        for (text, expected) in [
            ("2024-02-29", &["2024-02-29"][..]),
            ("on (2026-11-08).", &["2026-11-08"][..]),
            (
                "2026-01-01 and 2027-02-03",
                &["2026-01-01", "2027-02-03"][..],
            ),
        ] {
            let actual = absolute_dates(text)
                .into_iter()
                .map(|(_, date)| date.to_string())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{text}");
        }
        for text in [
            "2026-02-30",
            "2026-1-01",
            "x2026-01-01",
            "2026-01-01x",
            "2026-01-01T12:00:00Z",
        ] {
            assert!(absolute_dates(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn relative_dates_accept_counts_words_articles_and_agreement() {
        for (text, expected) in [
            ("10 days ago", "2026-11-08"),
            ("Ten Days Ago", "2026-11-08"),
            ("a week ago", "2026-11-11"),
            ("an day ago", "2026-11-17"),
            ("one week ago", "2026-11-11"),
            ("four weeks ago", "2026-10-21"),
            ("1 day ago", "2026-11-17"),
            ("2 days ago", "2026-11-16"),
            ("twelve weeks ago", "2026-08-26"),
            ("bought it 10 days ago.", "2026-11-08"),
        ] {
            let actual = relative_dates(text, anchor().date_naive())
                .into_iter()
                .map(|date| date.to_string())
                .collect::<Vec<_>>();
            assert_eq!(actual, [expected], "{text}");
        }
    }

    #[test]
    fn relative_dates_reject_unsupported_invalid_and_disagreeing_forms() {
        for text in [
            "0 days ago",
            "1 days ago",
            "2 day ago",
            "a weeks ago",
            "one weeks ago",
            "two week ago",
            "ten days",
            "10 days later",
            "10 months ago",
            "thirteen days ago",
            "yesterday",
            "next week",
            "-1 days ago",
            "+1 days ago",
            "999999999999999999999 days ago",
            "9223372036854775807 weeks ago",
        ] {
            assert!(
                relative_dates(text, anchor().date_naive()).is_empty(),
                "{text}"
            );
        }
    }

    #[test]
    fn query_dates_resolve_relatives_only_with_an_explicit_anchor() {
        let text = "2026-11-08 or ten days ago";
        assert_eq!(dates_in_text(text, None).len(), 1);
        assert_eq!(dates_in_text(text, Some(anchor())).len(), 1);
        assert!(dates_in_text("ten days ago", None).is_empty());
    }

    #[test]
    fn absolute_date_masking_preserves_other_numbers() {
        assert_eq!(
            mask_absolute_dates("version 42 on 2026-11-08 at 9"),
            "version 42 on            at 9"
        );
    }
}
