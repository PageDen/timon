// Adapted for Timon. Not derived from Prodex source.
//! Turning a report into something a person or a spreadsheet can read.
//!
//! Every rendering carries the same two caveats as the numbers themselves. A
//! total that travels without them invites being read as a bill.

use crate::recorder::db::Report;

/// Renders a report as RFC 4180 CSV.
///
/// The caveats lead as comment lines. A spreadsheet shows them as a first
/// column rather than hiding them, which is the point: they should be hard to
/// drop, not tidy.
pub fn csv(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n", field(&report.basis)));
    out.push_str(&format!("# {}\n", field(&report.quota_note)));
    out.push_str(&format!(
        "# window_start_utc,{},window_end_utc,{}\n",
        field(&report.since.map(utc).unwrap_or_else(|| "-".into())),
        field(&report.until.map(utc).unwrap_or_else(|| "-".into())),
    ));
    out.push_str(
        "uid,username,names_seen,events,total_tokens,input_tokens,output_tokens,\
events_with_unknown_usage,events_with_partial_usage,late_events,corrections_applied,\
first_occurred_utc,last_occurred_utc\n",
    );
    for p in &report.principals {
        let row = [
            p.peer_uid.to_string(),
            p.username.clone().unwrap_or_default(),
            p.names_seen.to_string(),
            p.events.to_string(),
            p.total_tokens.to_string(),
            p.input_tokens.to_string(),
            p.output_tokens.to_string(),
            p.events_with_unknown_usage.to_string(),
            p.events_with_partial_usage.to_string(),
            p.late_events.to_string(),
            p.corrections_applied.to_string(),
            p.first_occurred_at.map(utc).unwrap_or_default(),
            p.last_occurred_at.map(utc).unwrap_or_default(),
        ];
        out.push_str(&row.iter().map(|v| field(v)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

/// Renders a report for a terminal.
pub fn text(report: &Report) -> String {
    let mut out = String::new();
    let window = match (report.since, report.until) {
        (Some(from), Some(to)) => format!("{} to {}", utc(from), utc(to)),
        (Some(from), None) => format!("from {}", utc(from)),
        (None, Some(to)) => format!("up to {}", utc(to)),
        (None, None) => "all recorded time".to_string(),
    };
    out.push_str(&format!("Reported usage, {window}\n"));
    match report.scope_uid {
        Some(uid) => out.push_str(&format!("Principal: uid {uid}\n")),
        None => out.push_str("Principals: all\n"),
    }
    out.push('\n');

    if report.principals.is_empty() {
        out.push_str("  no usage recorded in this window\n");
    }
    for p in &report.principals {
        let name = p.username.clone().unwrap_or_else(|| "(unknown)".into());
        out.push_str(&format!("  {name} (uid {})\n", p.peer_uid));
        out.push_str(&format!(
            "    {} events, {} tokens reported ({} in, {} out)\n",
            p.events, p.total_tokens, p.input_tokens, p.output_tokens
        ));
        if p.events_with_unknown_usage > 0 {
            out.push_str(&format!(
                "    {} event(s) reported no usage: those tokens are unknown, not zero, \
and are not in the total above\n",
                p.events_with_unknown_usage
            ));
        }
        if p.events_with_partial_usage > 0 {
            out.push_str(&format!(
                "    {} event(s) reported partial usage: the total is a lower bound\n",
                p.events_with_partial_usage
            ));
        }
        if p.late_events > 0 {
            out.push_str(&format!(
                "    {} event(s) arrived late, so an earlier report of this window \
may have shown less\n",
                p.late_events
            ));
        }
        if p.corrections_applied > 0 {
            out.push_str(&format!(
                "    {} correction(s) applied, each replacing what it corrects\n",
                p.corrections_applied
            ));
        }
        if p.names_seen > 1 {
            out.push_str(&format!(
                "    {} different names seen for this uid: a rename, or a recycled uid \
merging two histories\n",
                p.names_seen
            ));
        }
    }
    if report.superseded_by_corrections > 0 {
        out.push_str(&format!(
            "\n  {} row(s) excluded as superseded by a correction\n",
            report.superseded_by_corrections
        ));
    }
    out.push_str(&format!("\n{}\n{}\n", report.basis, report.quota_note));
    out
}

/// Quotes a CSV field when it would otherwise change the shape of the row.
fn field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Unix seconds as an ISO-8601 UTC instant.
pub fn utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Start of a UTC day, as Unix seconds.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    // Howard Hinnant's civil-from-days, inverted. Implemented here rather than
    // taking a date dependency for two conversions.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Resolves `YYYY-MM` or `YYYY-MM-DD` to a UTC window, inclusive of both ends.
///
/// Explicitly UTC: a report whose boundaries moved with the reader's timezone
/// would attribute the same work to different days for different people.
pub fn period(spec: &str) -> Result<(i64, i64), String> {
    let parts: Vec<&str> = spec.split('-').collect();
    let parse = |value: &str, what: &str| -> Result<i64, String> {
        value
            .parse::<i64>()
            .map_err(|_| format!("{what} in {spec:?} is not a number"))
    };
    match parts.as_slice() {
        [year, month] => {
            let (year, month) = (parse(year, "year")?, parse(month, "month")?);
            if !(1..=12).contains(&month) {
                return Err(format!("month {month} is out of range"));
            }
            let start = days_from_civil(year, month as u32, 1) * 86_400;
            let (next_year, next_month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
            let end = days_from_civil(next_year, next_month as u32, 1) * 86_400 - 1;
            Ok((start, end))
        }
        [year, month, day] => {
            let (year, month, day) = (
                parse(year, "year")?,
                parse(month, "month")?,
                parse(day, "day")?,
            );
            if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                return Err(format!("{spec:?} is not a valid date"));
            }
            let start = days_from_civil(year, month as u32, day as u32) * 86_400;
            Ok((start, start + 86_399))
        }
        _ => Err(format!("{spec:?} is not YYYY-MM or YYYY-MM-DD")),
    }
}
