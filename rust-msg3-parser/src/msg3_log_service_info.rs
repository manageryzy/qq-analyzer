use std::collections::HashMap;

use serde_json::{json, Value};

use crate::msg3_parser as parser;

pub(crate) fn info_summary(info: &[u8]) -> HashMap<String, String> {
    let parsed: Value =
        serde_json::from_str(&parser::parse_info_json(info)).unwrap_or_else(|_| json!({}));
    let mut out = HashMap::new();
    for rec in parsed["td_ta_records"].as_array().into_iter().flatten() {
        for field in rec["txdata_fields"].as_array().into_iter().flatten() {
            let name = field["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let has_uint = if let Some(value) = field["value_uint"].as_u64() {
                out.insert(name.to_string(), value.to_string());
                true
            } else {
                false
            };
            if !has_uint {
                if let Some(text) = field["value_texts"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    let text = text.trim();
                    if !text.is_empty() {
                        out.insert(name.to_string(), text.to_string());
                    }
                }
            }
        }
    }
    out
}
