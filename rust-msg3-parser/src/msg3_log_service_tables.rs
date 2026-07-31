use rusqlite::Connection;

pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn split_table(table: &str) -> (String, String) {
    for prefix in ["buddy_", "group_", "discuss_", "system_"] {
        if let Some(id) = table.strip_prefix(prefix) {
            return (prefix.trim_end_matches('_').to_string(), id.to_string());
        }
    }
    ("unknown".to_string(), table.to_string())
}

pub(crate) fn table_names(con: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = con.prepare(
        "select name from sqlite_master where type='table' and (name glob 'buddy_*' or name glob 'group_*' or name glob 'discuss_*' or name glob 'system_*') order by name",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_escapes_double_quotes() {
        assert_eq!(quote_ident("group_1"), "\"group_1\"");
        assert_eq!(quote_ident("bad\"name"), "\"bad\"\"name\"");
    }

    #[test]
    fn split_table_matches_current_service_prefix_rules() {
        assert_eq!(
            split_table("buddy_123"),
            ("buddy".to_string(), "123".to_string())
        );
        assert_eq!(
            split_table("group_456"),
            ("group".to_string(), "456".to_string())
        );
        assert_eq!(
            split_table("discuss_789"),
            ("discuss".to_string(), "789".to_string())
        );
        assert_eq!(
            split_table("system_0"),
            ("system".to_string(), "0".to_string())
        );
        assert_eq!(
            split_table("buddy$123"),
            ("unknown".to_string(), "buddy$123".to_string())
        );
    }

    #[test]
    fn table_names_returns_every_supported_conversation_table() {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(
            r#"create table buddy_1(value integer);
               create table group_2(value integer);
               create table discuss_3(value integer);
               create table system_0(value integer);
               create table metadata(value integer);"#,
        )
        .unwrap();
        assert_eq!(
            table_names(&con).unwrap(),
            vec!["buddy_1", "discuss_3", "group_2", "system_0"]
        );
    }
}
