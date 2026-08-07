use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub sender_email: String,
    pub sender_id: i64,
    pub sender_full_name: String,
    pub content: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub message: Option<Message>,
}

#[derive(Debug, Clone)]
pub struct Submission {
    pub id: Option<i64>,
    pub user_id: i64,
    pub user_email: String,
    pub user_full_name: String,
    pub submission_name: String,
    pub timestamp: String,
    pub file_checksum: String,
    pub file_path: String,
    pub expected_gain: f64,
    pub actual_gain: f64,
    pub tp: i32,
    pub tn: i32,
    pub fp: i32,
    pub fn_: i32,
    pub positives_predicted: i32,
    pub threshold_category: String,
    pub after_deadline: bool,
    /// Set when this submission was made with the `reveal` command, which
    /// spends one of the competitor's golden bullets to see the actual gain
    /// immediately instead of waiting for `results_reveal_date`.
    pub used_golden_bullet: bool,
    /// Kaggle mode only: groups every candidate CSV of one `submit` submission.
    /// `None` in blind mode, where a submission is always a single row.
    pub batch_id: Option<String>,
    /// Kaggle mode only: this candidate's gain against `MasterData::public_ids`.
    pub public_gain: Option<f64>,
    /// Kaggle mode only: this candidate's gain against `MasterData::private_ids`.
    /// Only becomes the competitor's grade-determining value if this batch
    /// turns out to be their LAST pre-deadline submission -- there is no
    /// separate pick step, see `Database::get_leaderboard`.
    pub private_gain: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct GainResult {
    pub gain: f64,
    pub tp: i32,
    pub tn: i32,
    pub fp: i32,
    pub fn_: i32,
}

#[derive(Debug, Deserialize)]
pub struct ZulipEventsResponse {
    pub events: Vec<Event>,
}
