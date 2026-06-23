use chrono::{Local, TimeZone};
use rusqlite::Connection;

pub(crate) fn iso_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ConversationLastTime {
    pub(crate) rows: i64,
    pub(crate) last_time: i64,
    pub(crate) raw_last_time: i64,
    pub(crate) time_anomaly_count: i64,
}

fn plausible_message_time(ts: i64) -> bool {
    let lower = 946684800i64; // 2000-01-01 UTC
    let upper = Local::now().timestamp().saturating_add(2 * 24 * 60 * 60);
    (lower..=upper).contains(&ts)
}

pub(crate) fn conversation_last_time(
    con: &Connection,
    quoted_table: &str,
) -> anyhow::Result<ConversationLastTime> {
    let mut stmt = con.prepare(&format!(
        "select rowid, Time from {quoted_table} order by rowid desc limit 20"
    ))?;
    let mut rows = stmt.query([])?;
    let mut meta = ConversationLastTime::default();
    let mut saw_first = false;
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let time: i64 = row.get(1)?;
        if !saw_first {
            meta.rows = rowid;
            meta.raw_last_time = time;
            meta.last_time = time;
            saw_first = true;
        }
        if plausible_message_time(time) {
            meta.last_time = time;
            break;
        } else {
            meta.time_anomaly_count += 1;
        }
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_time_formats_valid_timestamp() {
        let value = iso_time(1_600_000_000);
        assert_eq!(value.len(), "2020-09-13 21:26:40".len());
        assert!(value.contains('-'));
        assert!(value.contains(':'));
    }

    #[test]
    fn plausible_message_time_accepts_modern_timestamp() {
        assert!(plausible_message_time(1_600_000_000));
        assert!(!plausible_message_time(1));
    }
}
