use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub zulip: ZulipConfig,
    pub database: DatabaseConfig,
    pub logs: LogsConfig,
    pub teachers: Vec<String>,
    pub master_data: MasterDataConfig,
    pub submissions: SubmissionsConfig,
    /// Required, not optional: the roster is the sole authorization list for
    /// students. A config without one refuses to load, the same as a config
    /// pointing at a missing master_data.csv.
    pub roster: RosterConfig,
    pub gain_matrix: GainMatrix,
    pub gain_thresholds: Vec<GainThreshold>,
    pub competition: CompetitionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZulipConfig {
    pub email: String,
    pub api_key: String,
    pub site: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterDataConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmissionsConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GainMatrix {
    pub tp: f64,
    pub tn: f64,
    pub fp: f64,
    pub fn_: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GainThreshold {
    pub min_gain: f64,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub gifs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompetitionConfig {
    pub name: String,
    pub description: String,
    pub deadline: String,
    pub results_reveal_date: String,
    /// Minutes to ADD to UTC to get local wall-clock time (e.g. -180 for
    /// Argentina). Governs both how naive datetime strings in this struct are
    /// interpreted AND the calendar-day boundary for the daily submit quota --
    /// one clock, not two. Defaults to 0 (UTC), so configs written before this
    /// field existed keep behaving exactly as before.
    #[serde(default)]
    pub timezone_offset_minutes: i32,
    /// Defaults to `Blind` (the original behaviour) so configs written
    /// before kaggle mode existed keep meaning exactly what they meant.
    #[serde(default)]
    pub mode: CompetitionMode,
}

/// `Blind` (`"blind"` in config): a submit reveals nothing but its threshold
/// category; ranking and grading use the single `actual_gain` computed
/// against the whole dataset. `Kaggle`: a submit is a batch of one or more
/// candidate CSVs, scored against a public/private split of
/// `master_data.csv` -- see `MasterData::has_split`. Requires
/// `master_data.csv` to carry a `split` column; checked at startup, not
/// here, since master data isn't loaded yet when config validation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompetitionMode {
    #[default]
    Blind,
    Kaggle,
}

/// Parses the two datetime formats the config accepts: RFC3339 (which carries
/// its own offset and ignores `offset_minutes`), or a naive
/// `%Y-%m-%dT%H:%M:%S`, read as local wall-clock time at UTC+`offset_minutes`.
pub fn parse_config_datetime(value: &str, offset_minutes: i32) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").with_context(|| {
        format!(
            "Invalid datetime '{}': expected RFC3339 (2025-12-31T23:59:59Z) \
             or %Y-%m-%dT%H:%M:%S",
            value
        )
    })?;

    // naive's digits are local wall-clock time; UTC = local - offset.
    let utc_naive = naive - Duration::minutes(offset_minutes as i64);
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(utc_naive, Utc))
}

impl CompetitionConfig {
    pub fn deadline_utc(&self) -> Result<DateTime<Utc>> {
        parse_config_datetime(&self.deadline, self.timezone_offset_minutes)
            .context("competition.deadline")
    }

    pub fn results_reveal_utc(&self) -> Result<DateTime<Utc>> {
        parse_config_datetime(&self.results_reveal_date, self.timezone_offset_minutes)
            .context("competition.results_reveal_date")
    }

    /// The [start, end) of the local calendar day containing `at`, in UTC.
    /// Used to delimit the daily submit quota under the same clock as the
    /// deadline and reveal date.
    pub fn local_day_bounds_utc(&self, at: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let offset = Duration::minutes(self.timezone_offset_minutes as i64);
        let local_naive = at.naive_utc() + offset;
        let local_midnight = local_naive
            .date()
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time");
        let start = DateTime::<Utc>::from_naive_utc_and_offset(local_midnight - offset, Utc);
        (start, start + Duration::days(1))
    }
}

impl BotConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;

        let config: BotConfig =
            serde_json::from_str(&content).with_context(|| "Failed to parse config file")?;

        config.validate()?;

        Ok(config)
    }

    /// Fails fast on configuration that would otherwise panic or silently
    /// misbehave at runtime.
    pub fn validate(&self) -> Result<()> {
        // An empty threshold list panics when scoring a submission, since every
        // gain must map to some category.
        if self.gain_thresholds.is_empty() {
            bail!("gain_thresholds must not be empty: every gain needs a category");
        }

        // Categories are looked up by name to build the reply, so duplicates
        // make the response message ambiguous.
        let mut seen = HashSet::new();
        for threshold in &self.gain_thresholds {
            if !seen.insert(&threshold.category) {
                bail!(
                    "duplicate gain_thresholds category '{}'",
                    threshold.category
                );
            }
        }

        // The log directory is created at startup; an empty value fails there
        // with an opaque error from create_dir_all.
        if self.logs.path.trim().is_empty() {
            bail!("logs.path must not be empty");
        }

        // Roster::load happens later (main.rs), same as master_data -- but an
        // empty path is a config typo, not a missing file, and belongs here.
        if self.roster.path.trim().is_empty() {
            bail!("roster.path must not be empty");
        }

        // Real UTC offsets run from -12:00 to +14:00; anything outside that is
        // certainly a typo (e.g. hours instead of minutes).
        if !(-720..=840).contains(&self.competition.timezone_offset_minutes) {
            bail!(
                "competition.timezone_offset_minutes ({}) is outside the range of real UTC offsets (-720..=840)",
                self.competition.timezone_offset_minutes
            );
        }

        // Both dates gate real behaviour (what counts, what students can see),
        // so an unparseable value must not be silently tolerated.
        self.competition.deadline_utc()?;
        self.competition.results_reveal_utc()?;

        Ok(())
    }
}

pub fn create_config_template() -> Result<()> {
    let config = BotConfig {
        zulip: ZulipConfig {
            email: "competition-bot@example.com".to_string(),
            api_key: "your-api-key-here".to_string(),
            site: "https://your-org.zulipchat.com".to_string(),
        },
        database: DatabaseConfig {
            path: "zulip_competition.db".to_string(),
        },
        logs: LogsConfig {
            path: "logs".to_string(),
        },
        teachers: vec![
            "teacher1@example.com".to_string(),
            "teacher2@example.com".to_string(),
        ],
        master_data: MasterDataConfig {
            path: "master_data.csv".to_string(),
        },
        submissions: SubmissionsConfig {
            path: "./submissions".to_string(),
        },
        roster: RosterConfig {
            path: "roster.csv".to_string(),
        },
        gain_matrix: GainMatrix {
            tp: 1.0,
            tn: 0.5,
            fp: -0.1,
            fn_: -0.5,
        },
        gain_thresholds: vec![
            GainThreshold {
                min_gain: 100.0,
                category: "excellent".to_string(),
                message: "Outstanding model!".to_string(),
                gifs: vec![
                    "https://media.giphy.com/media/v1.Y2lkPTc5MGI3NjExYWJj/giphy.gif".to_string(),
                    "https://media.giphy.com/media/v1.Y2lkPTc5MGI3NjExZGVm/giphy.gif".to_string(),
                ],
            },
            GainThreshold {
                min_gain: 50.0,
                category: "good".to_string(),
                message: "Good job".to_string(),
                gifs: vec![
                    "https://media.giphy.com/media/v1.Y2lkPTc5MGI3NjExZ2hp/giphy.gif".to_string(),
                ],
            },
            GainThreshold {
                min_gain: 0.0,
                category: "basic".to_string(),
                message: "Keep trying".to_string(),
                gifs: vec![
                    "https://media.giphy.com/media/v1.Y2lkPTc5MGI3NjExamp/giphy.gif".to_string(),
                ],
            },
        ],
        competition: CompetitionConfig {
            name: "ML Competition".to_string(),
            description: "Machine learning competition".to_string(),
            deadline: "2025-12-31T23:59:59".to_string(),
            results_reveal_date: "2026-01-01T23:59:59".to_string(),
            // Argentina (UTC-3). Set to 0 if deadline/results_reveal_date are
            // already written as UTC wall-clock times.
            timezone_offset_minutes: -180,
            mode: CompetitionMode::Blind,
        },
    };

    let json = serde_json::to_string_pretty(&config)?;
    fs::write("config.json", json)?;

    Ok(())
}
