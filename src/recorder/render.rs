// Adapted for Timon. Not derived from Prodex source.
//! Turning a report into something a person or a spreadsheet can read.
//!
//! Every rendering carries the same two caveats as the numbers themselves. A
//! total that travels without them invites being read as a bill.

use crate::recorder::db::{MonthlyTotals, Report, SHARED_QUOTA_LABEL, VISIBILITY_LABEL};
use crate::recorder::retain::RetentionReport;

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
    if let Some(from) = report.detail_from {
        out.push_str(&format!(
            "# detail_complete_from_utc,{},detail_incomplete_for_this_window,{}\n",
            field(&utc(from)),
            report.detail_incomplete,
        ));
    }
    out.push_str(
        "uid,generation,username,names_seen,events,total_tokens,input_tokens,output_tokens,\
events_with_unknown_usage,events_with_partial_usage,late_events,corrections_applied,\
first_occurred_utc,last_occurred_utc\n",
    );
    for p in &report.principals {
        let row = [
            p.peer_uid.to_string(),
            p.principal_generation.to_string(),
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
        // Labelled whenever the generation is not the first, and also whenever
        // the same uid appears more than once: two unlabelled groups under one
        // uid read as a duplicated row rather than as two different people.
        let several = report
            .principals
            .iter()
            .filter(|other| other.peer_uid == p.peer_uid)
            .count()
            > 1;
        if p.principal_generation == 0 && !several {
            out.push_str(&format!("  {name} (uid {})\n", p.peer_uid));
        } else {
            out.push_str(&format!(
                "  {name} (uid {}, generation {})\n",
                p.peer_uid, p.principal_generation
            ));
        }
        if several && p.principal_generation == 0 {
            out.push_str(
                "    an earlier holder of this uid: the account was retired and the number \
reused. These figures are not the current holder's\n",
            );
        }
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
                "    {} different names seen within this generation: usually a rename. \
If the uid was reused without recording a retirement, these are two people's \
figures added together\n",
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
    if report.detail_incomplete {
        let from = report
            .detail_from
            .map(utc)
            .unwrap_or_else(|| "an earlier date".to_string());
        out.push_str(&format!(
            "\n  Detail before {from} has been rolled up by retention, so this window is \
answered only in part by the events above. A low total here may be missing \
detail rather than quiet usage; see `timon usage monthly` for the totals that \
cover it.\n"
        ));
    }
    out.push_str(&format!("\n{}\n{}\n", report.basis, report.quota_note));
    out
}

/// Renders rolled-up months for a terminal.
pub fn monthly_text(months: &[MonthlyTotals], scope_uid: Option<u32>) -> String {
    let mut out = String::new();
    out.push_str("Rolled-up monthly usage\n");
    match scope_uid {
        Some(uid) => out.push_str(&format!("Principal: uid {uid}\n")),
        None => out.push_str("Principals: all\n"),
    }
    out.push('\n');
    if months.is_empty() {
        out.push_str(
            "  nothing rolled up yet: every recorded event is still held in full detail\n",
        );
    }
    for m in months {
        let name = m
            .peer_username
            .clone()
            .unwrap_or_else(|| "(unknown)".into());
        let generation = if m.principal_generation == 0 {
            String::new()
        } else {
            format!(", generation {}", m.principal_generation)
        };
        out.push_str(&format!(
            "  {} {name} (uid {}{generation}): {} events, {} tokens ({} in, {} out)\n",
            m.month, m.peer_uid, m.events, m.total_tokens, m.input_tokens, m.output_tokens
        ));
        if m.events_with_unknown_usage > 0 {
            out.push_str(&format!(
                "      {} event(s) reported no usage: unknown, not zero, and not in the \
total above\n",
                m.events_with_unknown_usage
            ));
        }
        if m.events_with_partial_usage > 0 {
            out.push_str(&format!(
                "      {} event(s) reported partial usage: the total is a lower bound\n",
                m.events_with_partial_usage
            ));
        }
    }
    out.push_str(&format!(
        "\nThese are totals only. The individual events behind them were rolled up by \
retention and are no longer recorded.\n{}\n{}\n",
        VISIBILITY_LABEL, SHARED_QUOTA_LABEL
    ));
    out
}

/// Renders rolled-up months as CSV.
pub fn monthly_csv(months: &[MonthlyTotals]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n", field(VISIBILITY_LABEL)));
    out.push_str(&format!("# {}\n", field(SHARED_QUOTA_LABEL)));
    out.push_str("# totals only; the events behind them were rolled up by retention\n");
    out.push_str(
        "month,uid,generation,username,events,total_tokens,input_tokens,output_tokens,\
events_with_unknown_usage,events_with_partial_usage,first_occurred_utc,last_occurred_utc,\
rolled_up_utc\n",
    );
    for m in months {
        let row = [
            m.month.clone(),
            m.peer_uid.to_string(),
            m.principal_generation.to_string(),
            m.peer_username.clone().unwrap_or_default(),
            m.events.to_string(),
            m.total_tokens.to_string(),
            m.input_tokens.to_string(),
            m.output_tokens.to_string(),
            m.events_with_unknown_usage.to_string(),
            m.events_with_partial_usage.to_string(),
            m.first_occurred_at.map(utc).unwrap_or_default(),
            m.last_occurred_at.map(utc).unwrap_or_default(),
            utc(m.rolled_up_at),
        ];
        out.push_str(&row.iter().map(|v| field(v)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

/// Describes what retention did, or would do.
pub fn retention_text(report: &RetentionReport, dry_run: bool) -> String {
    let plan = &report.plan;
    let mut out = String::new();
    out.push_str(&format!(
        "{} detail older than {} days (before {})\n",
        if dry_run {
            "Would roll up"
        } else {
            "Rolled up"
        },
        plan.keep_days,
        utc(plan.cutoff)
    ));
    out.push_str(&format!(
        "  {} event(s) leave detail, of which {} were counted toward a total\n",
        plan.events_rolled_up, plan.live_events_rolled_up
    ));
    out.push_str(&format!(
        "  {} token(s) move into monthly totals: still reported, no longer attributable \
to one attempt\n",
        plan.tokens_rolled_up
    ));
    if plan.held_back_by_corrections > 0 {
        out.push_str(&format!(
            "  {} older event(s) kept as detail anyway, because a correction links them \
to something inside the window\n",
            plan.held_back_by_corrections
        ));
    }
    if !plan.months.is_empty() {
        out.push_str(&format!(
            "  month(s) holding totals: {}\n",
            plan.months.join(", ")
        ));
    }
    out.push_str(&format!(
        "  detail is complete from {} onwards\n",
        utc(plan.detail_from)
    ));
    if dry_run {
        out.push_str("\nNothing was changed.\n");
        return out;
    }
    out.push_str(&format!(
        "\n  database: {}\n  previous file kept at: {}\n  {} bytes before, {} after\n",
        report.database.display(),
        report.previous.display(),
        report.bytes_before,
        report.bytes_after
    ));
    out.push_str(
        "\nThe previous file is the only remaining copy of the detail just rolled up. \
Remove it once you are satisfied with the result.\n",
    );
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
