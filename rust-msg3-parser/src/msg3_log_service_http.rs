use std::collections::HashMap;

pub(crate) type QueryParams = HashMap<String, String>;

pub(crate) fn split_url(url: &str) -> (&str, &str) {
    if let Some((path, query)) = url.split_once('?') {
        (path, query)
    } else {
        (url, "")
    }
}

pub(crate) fn parse_query(query: &str) -> QueryParams {
    let mut out = HashMap::new();
    for part in query.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.insert(
            urlencoding::decode(k).unwrap_or_default().to_string(),
            urlencoding::decode(v).unwrap_or_default().to_string(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_url_separates_first_question_mark() {
        assert_eq!(
            split_url("/api/messages?table=group_1"),
            ("/api/messages", "table=group_1")
        );
        assert_eq!(split_url("/api/status"), ("/api/status", ""));
    }

    #[test]
    fn parse_query_matches_service_behavior() {
        let q = parse_query("a=1&empty&encoded=%E7%BE%A4&dup=old&dup=new&&");
        assert_eq!(q.get("a").map(String::as_str), Some("1"));
        assert_eq!(q.get("empty").map(String::as_str), Some(""));
        assert_eq!(q.get("encoded").map(String::as_str), Some("群"));
        assert_eq!(q.get("dup").map(String::as_str), Some("new"));
        assert!(!q.contains_key(""));
    }
}
