pub(crate) fn normalize_sender_show_name(
    text: &str,
    sender_uin: &str,
    account: &str,
    conv_id: &str,
) -> String {
    let mut value = text.trim().to_string();
    let suffix = format!("({sender_uin})");
    if value.ends_with(&suffix) {
        value.truncate(value.len() - suffix.len());
        value = value.trim().to_string();
    }
    if usable_name(&value, conv_id, account) && value != account {
        value
    } else {
        String::new()
    }
}

pub(crate) fn usable_name(text: &str, ident: &str, account: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t == ident || t == account {
        return false;
    }
    if t.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if t.starts_with('(')
        && t.ends_with(')')
        && t[1..t.len() - 1].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    true
}

pub(crate) fn first_nonempty(values: &[&String]) -> String {
    values
        .iter()
        .find(|s| !s.is_empty())
        .map(|s| (*s).clone())
        .unwrap_or_default()
}

pub(crate) fn merge_member_label_with_message_info(member: &str, row_sender_name: &str) -> String {
    if member.is_empty() {
        return String::new();
    }
    if let Some(start) = row_sender_name.find('<') {
        if let Some(end) = row_sender_name[start..].find('>') {
            let email = &row_sender_name[start..start + end + 1];
            if email.contains('@') && !member.contains(email) {
                return format!("{member}{email}");
            }
        }
    }
    member.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_name_filters_current_invalid_forms() {
        assert!(!usable_name("", "123", "999"));
        assert!(!usable_name("123", "123", "999"));
        assert!(!usable_name("999", "123", "999"));
        assert!(!usable_name("456", "123", "999"));
        assert!(!usable_name("(456)", "123", "999"));
        assert!(usable_name("红烧杂鱼", "123", "999"));
    }

    #[test]
    fn normalize_sender_removes_uin_suffix() {
        assert_eq!(
            normalize_sender_show_name("红烧杂鱼(12345)", "12345", "999", "888"),
            "红烧杂鱼"
        );
        assert_eq!(
            normalize_sender_show_name("12345", "12345", "999", "888"),
            ""
        );
    }

    #[test]
    fn merge_member_preserves_email_from_message_info() {
        assert_eq!(
            merge_member_label_with_message_info("user", "user<user@example.invalid>"),
            "user<user@example.invalid>"
        );
        assert_eq!(
            merge_member_label_with_message_info(
                "user<user@example.invalid>",
                "user<user@example.invalid>"
            ),
            "user<user@example.invalid>"
        );
    }
}
