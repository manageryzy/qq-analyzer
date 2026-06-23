use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize)]
pub(crate) struct ConversationLight {
    pub(crate) table: String,
    pub(crate) key: String,
    #[serde(rename = "type")]
    pub(crate) conv_type: String,
    pub(crate) id: String,
    pub(crate) rows: i64,
    pub(crate) rows_is_estimate: bool,
    pub(crate) last_time: i64,
    pub(crate) last_datetime: String,
    pub(crate) label: String,
    pub(crate) label_source_basic: String,
    pub(crate) group_avatar_url: String,
    pub(crate) group_avatar_candidates: Vec<String>,
    pub(crate) group_avatar_reason: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct Conversation {
    pub(crate) table: String,
    pub(crate) key: String,
    #[serde(rename = "type")]
    pub(crate) conv_type: String,
    pub(crate) id: String,
    pub(crate) rows: i64,
    pub(crate) last_time: i64,
    pub(crate) last_datetime: String,
    pub(crate) label: String,
    pub(crate) raw_last_time: i64,
    pub(crate) raw_last_datetime: String,
    pub(crate) time_anomaly_count: i64,
    pub(crate) label_source: String,
    pub(crate) group_memo: String,
    pub(crate) group_avatar_url: String,
    pub(crate) group_avatar_candidates: Vec<String>,
    pub(crate) group_avatar_reason: String,
    pub(crate) group_profile: Value,
}

impl Conversation {
    pub(crate) fn clone_for_item(&self) -> Self {
        self.clone()
    }
}

#[derive(Serialize)]
pub(crate) struct MessageItem {
    pub(crate) table: String,
    pub(crate) rowid: i64,
    pub(crate) time: i64,
    pub(crate) datetime: String,
    pub(crate) time_is_plausible: bool,
    pub(crate) rand: i64,
    pub(crate) sender_uin: String,
    pub(crate) sender_name: String,
    pub(crate) receiver_name: String,
    pub(crate) is_self: bool,
    pub(crate) display_sender: String,
    pub(crate) display_sender_line: String,
    pub(crate) sender_role: String,
    pub(crate) sender_identity_source: String,
    pub(crate) sender_identity_note: String,
    pub(crate) member_profile: Value,
    pub(crate) contact_profile: Value,
    pub(crate) avatar_url: String,
    pub(crate) avatar_candidates: Vec<String>,
    pub(crate) avatar_reason: String,
    pub(crate) conversation: Conversation,
    pub(crate) indexed_text: String,
    pub(crate) text: String,
    pub(crate) display_text: String,
    pub(crate) rich_nodes: Value,
    pub(crate) rich_html: String,
    pub(crate) style_meta: Value,
    pub(crate) elements: Value,
    pub(crate) elements_json: String,
    pub(crate) assets: Vec<Value>,
    pub(crate) media_kind: String,
    pub(crate) media_label: String,
    pub(crate) candidate_paths: Vec<String>,
    pub(crate) candidate_path_count: i64,
    pub(crate) candidate_path_hit_count: i64,
    pub(crate) asset_scan_pending: bool,
    pub(crate) unmatched_reason: String,
    pub(crate) info_json: Value,
    pub(crate) info_summary: Value,
    pub(crate) msgcontent_len: usize,
    pub(crate) info_len: usize,
}
