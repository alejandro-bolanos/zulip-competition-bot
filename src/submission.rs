use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use csv::ReaderBuilder;
use rand::seq::SliceRandom;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, HashMap};
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

use crate::config::BotConfig;
use crate::database::{BaselineSummary, Database};
use crate::master_data::MasterData;
use crate::models::{GainResult, Message, Submission};
use crate::roster::{Competitor, Roster};
use crate::zulip::ZulipClient;

pub async fn process_submit(
    message: &Message,
    config: &BotConfig,
    db: &Database,
    master_data: &MasterData,
    competitor: &Competitor,
    is_teacher: bool,
    use_golden_bullet: bool,
) -> String {
    let user_email = &message.sender_email;

    info!(
        "Processing submit from {} (teacher: {}, golden bullet: {})",
        user_email, is_teacher, use_golden_bullet
    );

    // Cheapest check first: reject before touching the network or the
    // filesystem. Quota is counted from stored (i.e. successfully validated)
    // submissions, so a rejected or malformed attempt never spends it -- see
    // count_submissions_today.
    let used_today = match count_submissions_today(message.sender_id, config, db) {
        Ok(used) if used >= competitor.daily_submit_limit => {
            let (_, day_end) = config.competition.local_day_bounds_utc(Utc::now());
            let remaining = day_end - Utc::now();
            return format!(
                "🚫 You've reached today's submission limit ({used}/{limit}). You can submit again in {h}h {m}m.",
                used = used,
                limit = competitor.daily_submit_limit,
                h = remaining.num_hours(),
                m = remaining.num_minutes() % 60,
            );
        }
        Ok(used) => used,
        Err(e) => {
            warn!("Could not check daily quota for {}: {}", user_email, e);
            return "❌ Internal error checking your daily quota. Please tell a teacher.".to_string();
        }
    };

    // Same principle as the daily quota: a bullet is only spent once the
    // submission it's attached to actually gets stored, so this never blocks
    // on rejected/malformed attempts, only on already-stored uses.
    let bullets_used = if use_golden_bullet {
        match db.get_golden_bullets_used(message.sender_id) {
            Ok(used) if used >= competitor.golden_bullets => {
                return format!(
                    "🚫 You have no golden bullets left ({used}/{limit} already used).",
                    used = used,
                    limit = competitor.golden_bullets,
                );
            }
            Ok(used) => Some(used),
            Err(e) => {
                warn!("Could not check golden bullets for {}: {}", user_email, e);
                return "❌ Internal error checking your golden bullets. Please tell a teacher.".to_string();
            }
        }
    } else {
        None
    };

    // Validated at startup by BotConfig::validate, so this cannot normally fail.
    let deadline = match config.competition.deadline_utc() {
        Ok(dt) => dt,
        Err(e) => {
            warn!("Refusing submission, deadline is unparseable: {:#}", e);
            return "❌ Competition configuration error. Please tell a teacher.".to_string();
        }
    };
    let after_deadline = Utc::now() > deadline;

    // Parse command
    let command_word = if use_golden_bullet { "reveal" } else { "submit" };
    let parts: Vec<&str> = message.content.split_whitespace().collect();
    if parts.len() < 3 {
        return format!(
            "❌ Incorrect format. Usage: `{} <submission_name> <expected_gain>` and attach the CSV file",
            command_word
        );
    }

    let submission_name = parts[1].to_string();
    let expected_gain: f64 = match parts[2].parse() {
        Ok(g) => g,
        Err(_) => return "❌ The expected gain must be a number".to_string(),
    };

    info!(
        "Submission name: {}, Expected gain: {}",
        submission_name, expected_gain
    );

    // Extract file from message
    let (filename, file_content) = match extract_file_from_message(&message.content, config).await {
        Ok(Some((f, c))) => (f, c),
        Ok(None) => {
            return "❌ You must attach a CSV file. Use the format: `submit <name> <expected_gain>` and attach the CSV file.".to_string();
        }
        Err(e) => {
            return format!("❌ Error downloading file: {}", e);
        }
    };

    if !filename.to_lowercase().ends_with(".csv") {
        return "❌ The file must be a CSV".to_string();
    }

    // Save file
    let file_path = match save_submission_file(
        &message.sender_full_name,
        &submission_name,
        &filename,
        &file_content,
        is_teacher,
        config,
    ) {
        Ok(p) => p,
        Err(e) => return format!("❌ Error saving file: {}", e),
    };

    // Calculate checksum
    let checksum = calculate_checksum(&file_content);
    info!(
        "File saved: {}, checksum: {}...",
        file_path,
        &checksum[..16]
    );

    // Read and validate CSV
    let predicted_ids = match read_csv_ids(&file_content) {
        Ok(ids) => ids,
        Err(e) => return format!("❌ Error reading CSV: {}", e),
    };

    // Validate IDs
    let invalid_ids = master_data.validate_ids(&predicted_ids);
    if !invalid_ids.is_empty() {
        warn!("Invalid IDs in submission from {}", user_email);
        return format!(
            "❌ Invalid IDs found: {} IDs do not exist in the dataset",
            invalid_ids.len()
        );
    }

    // Calculate gain
    info!("Calculating gain for {}", submission_name);
    let gain_result = calculate_gain(&predicted_ids, master_data, &config.gain_matrix);
    let threshold_category = get_threshold_category(gain_result.gain, config);
    let positives_predicted = predicted_ids.len() as i32;

    info!(
        "Gain calculated - Expected: {:.4}, Actual: {:.4}",
        expected_gain, gain_result.gain
    );

    // Create submission record
    let submission = Submission {
        id: None,
        user_id: message.sender_id,
        user_email: user_email.clone(),
        user_full_name: message.sender_full_name.clone(),
        submission_name: submission_name.clone(),
        timestamp: Utc::now().to_rfc3339(),
        file_checksum: checksum,
        file_path,
        expected_gain: Some(expected_gain),
        actual_gain: gain_result.gain,
        tp: gain_result.tp,
        tn: gain_result.tn,
        fp: gain_result.fp,
        fn_: gain_result.fn_,
        positives_predicted,
        threshold_category: threshold_category.clone(),
        after_deadline,
        used_golden_bullet: use_golden_bullet,
        batch_id: None,
        public_gain: None,
        private_gain: None,
        is_baseline: false,
        baseline_published: false,
    };

    // Save to database
    let submission_id = match db.save_submission(&submission) {
        Ok(id) => id,
        Err(e) => return format!("❌ Error saving submission: {}", e),
    };

    info!("Submission saved with ID: {}", submission_id);

    // Build response
    let threshold_config = config
        .gain_thresholds
        .iter()
        .find(|t| t.category == threshold_category)
        .unwrap();

    let mut response = format!("🎯 **{}**\n\n", threshold_config.message);
    response.push_str(&format!("🆔 **Submission ID:** {}\n", submission_id));
    response.push_str(&format!("📊 **Expected gain:** {:.4}\n", expected_gain));

    // Teachers see actual gain and the full confusion matrix. A golden
    // bullet reveals the gain and category too, but not the matrix -- that
    // level of detail is a teacher-only debugging aid, not part of what a
    // bullet buys.
    if is_teacher {
        response.push_str(&format!("✨ **Actual gain:** {:.4}\n", gain_result.gain));
        response.push_str(&format!(
            "📈 **Predicted positives:** {}\n",
            positives_predicted
        ));
        response.push_str(&format!(
            "🔢 **Confusion matrix:** TP={}, TN={}, FP={}, FN={}\n",
            gain_result.tp, gain_result.tn, gain_result.fp, gain_result.fn_
        ));
    } else if use_golden_bullet {
        response.push_str(&format!("✨ **Actual gain:** {:.4}\n", gain_result.gain));
        response.push_str(&format!("🎯 **Category:** {}\n", threshold_category));
        response.push_str("🌟 You used a golden bullet to see this result right away.\n");
    }

    // After deadline notification
    if after_deadline {
        response.push_str("\n⚠️ **LATE SUBMISSION** - Recorded but not competing\n");
    }

    // Add random GIF
    if !threshold_config.gifs.is_empty() {
        let mut rng = rand::thread_rng();
        if let Some(gif) = threshold_config.gifs.choose(&mut rng) {
            response.push_str(&format!("\n{}", gif));
        }
    }

    // used_today was read before this submission was inserted, so +1 reflects
    // it. Shown always, not just near the limit -- so nobody discovers the
    // quota exists by hitting it.
    response.push_str(&format!(
        "\n📅 Submissions today: {}/{}",
        used_today + 1,
        competitor.daily_submit_limit
    ));

    // Same +1 reasoning as the daily quota, and same "always shown" reasoning
    // as the "You used a golden bullet" line above -- only present at all when
    // this submission used one.
    if let Some(bullets_used) = bullets_used {
        response.push_str(&format!(
            "\n🌟 Golden bullets used: {}/{}",
            bullets_used + 1,
            competitor.golden_bullets
        ));
    }

    response
}

/// Kaggle mode's `submit`: one message, one or more candidate CSVs, one
/// submission. Each candidate is scored against both `MasterData::public_ids`
/// (shown to the student, as a batch mean/std -- never per-candidate) and
/// `private_ids` (stored, only surfaced if this turns out to be the
/// student's LAST pre-deadline submission when the leaderboard/grades are
/// read -- there is no explicit "choose" step, mirroring blind mode's own
/// "last submission wins" rule). No golden bullets here -- `reveal` is
/// blind-mode-only, rejected earlier in `main.rs`.
///
/// All candidates are downloaded and validated *before* anything is stored:
/// one bad CSV rejects the whole submission, so a batch is never partially saved.
pub async fn process_kaggle_submit(
    message: &Message,
    config: &BotConfig,
    db: &Database,
    master_data: &MasterData,
    competitor: &Competitor,
) -> String {
    let user_email = &message.sender_email;

    info!("Processing kaggle submit from {}", user_email);

    // Same "cheapest check first" ordering as process_submit: quota is
    // counted from stored submissions, so a rejected attempt spends nothing.
    let used_today = match count_submissions_today(message.sender_id, config, db) {
        Ok(used) if used >= competitor.daily_submit_limit => {
            let (_, day_end) = config.competition.local_day_bounds_utc(Utc::now());
            let remaining = day_end - Utc::now();
            return format!(
                "🚫 You've reached today's submission limit ({used}/{limit}). You can submit again in {h}h {m}m.",
                used = used,
                limit = competitor.daily_submit_limit,
                h = remaining.num_hours(),
                m = remaining.num_minutes() % 60,
            );
        }
        Ok(used) => used,
        Err(e) => {
            warn!("Could not check daily quota for {}: {}", user_email, e);
            return "❌ Internal error checking your daily quota. Please tell a teacher.".to_string();
        }
    };

    let deadline = match config.competition.deadline_utc() {
        Ok(dt) => dt,
        Err(e) => {
            warn!("Refusing submission, deadline is unparseable: {:#}", e);
            return "❌ Competition configuration error. Please tell a teacher.".to_string();
        }
    };
    let after_deadline = Utc::now() > deadline;

    // No `<expected_gain>` here, unlike blind mode's `submit`: the public
    // gain is already shown in this very reply, so asking the student to
    // also guess it ahead of time added nothing.
    let parts: Vec<&str> = message.content.split_whitespace().collect();
    if parts.len() < 2 {
        return "❌ Incorrect format. Usage: `submit <submission_name>` and attach one or more CSV files".to_string();
    }
    let submission_name = parts[1];

    let (batch_id, rows) = match build_kaggle_batch(
        message,
        config,
        master_data,
        submission_name,
        competitor.max_files_per_submission,
        "submit <name>",
        after_deadline,
        false,
    )
    .await
    {
        Ok(batch) => batch,
        Err(reply) => return reply,
    };

    if let Err(e) = db.save_batch(&rows) {
        return format!("❌ Error saving submission: {}", e);
    }

    let public_gains: Vec<f64> = rows.iter().filter_map(|r| r.public_gain).collect();
    let (mean, std_dev) = mean_and_std(&public_gains);
    let n = rows.len();

    let mut response = format!(
        "📦 **Submission received:** {} candidate model{}\n",
        n,
        if n == 1 { "" } else { "s" },
    );
    response.push_str(&format!(
        "📊 **Public gain** -> mean: {:.4}, std dev: {:.4}\n",
        mean, std_dev
    ));
    response.push_str(&format!("🆔 **Submission ID**: `{}`\n", batch_id));

    if after_deadline {
        response.push_str("\n⚠️ **LATE SUBMISSION** - Recorded but not competing\n");
    } else {
        response.push_str(
            "\nℹ️ This is now your final submission -- your LAST pre-deadline submit always is, \
             with no separate step needed.\n",
        );
    }

    response.push_str(&format!(
        "\n📅 Submissions today: {}/{}",
        used_today + 1,
        competitor.daily_submit_limit
    ));

    response
}

/// The kaggle pipeline shared by a student's `submit` and a teacher's
/// `baseline`: finds every CSV attached to `message`, downloads, validates
/// and scores each against both splits, and builds one row per candidate
/// sharing a fresh `batch_id` -- nothing is stored. Every candidate is
/// validated before any row exists, so one bad CSV rejects the whole batch.
/// On failure, the `Err` is the reply to send back. `usage` is the command
/// syntax named in "attach a CSV" errors.
#[allow(clippy::too_many_arguments)]
async fn build_kaggle_batch(
    message: &Message,
    config: &BotConfig,
    master_data: &MasterData,
    submission_name: &str,
    max_files: u32,
    usage: &str,
    after_deadline: bool,
    is_baseline: bool,
) -> Result<(String, Vec<Submission>), String> {
    let links = find_csv_links(&message.content)
        .map_err(|e| format!("❌ Error parsing the message: {}", e))?;
    if links.is_empty() {
        return Err(format!(
            "❌ You must attach at least one CSV file. Use the format: `{usage}` and attach the CSVs."
        ));
    }
    if links.len() as u32 > max_files {
        return Err(format!(
            "🚫 Too many files in this submission ({} attachments, max {} per submission).",
            links.len(),
            max_files
        ));
    }

    let mut candidates: Vec<(String, Vec<u8>, HashSet<i32>)> = Vec::with_capacity(links.len());
    for (filename, url) in links {
        if !filename.to_lowercase().ends_with(".csv") {
            return Err(format!("❌ The file '{}' must be a CSV", filename));
        }
        let content = download_attachment(&url, config)
            .await
            .map_err(|e| format!("❌ Error downloading '{}': {}", filename, e))?;
        let predicted_ids =
            read_csv_ids(&content).map_err(|e| format!("❌ Error reading '{}': {}", filename, e))?;
        let invalid_ids = master_data.validate_ids(&predicted_ids);
        if !invalid_ids.is_empty() {
            warn!("Invalid IDs in kaggle submission from {}", message.sender_email);
            return Err(format!(
                "❌ Invalid IDs in '{}': {} IDs do not exist in the dataset",
                filename,
                invalid_ids.len()
            ));
        }
        candidates.push((filename, content, predicted_ids));
    }

    let now = Utc::now();
    let batch_id = format!("{}-{}", message.sender_id, now.timestamp_micros());
    let timestamp = now.to_rfc3339();

    let mut rows = Vec::with_capacity(candidates.len());
    for (index, (filename, content, predicted_ids)) in candidates.into_iter().enumerate() {
        let public_result = calculate_gain_over(
            &predicted_ids,
            master_data.public_ids(),
            master_data.positive_ids(),
            &config.gain_matrix,
        );
        let private_result = calculate_gain_over(
            &predicted_ids,
            master_data.private_ids(),
            master_data.positive_ids(),
            &config.gain_matrix,
        );

        let checksum = calculate_checksum(&content);
        let file_path = save_submission_file(
            &message.sender_full_name,
            submission_name,
            &format!("{}_{}", index, filename),
            &content,
            is_baseline,
            config,
        )
        .map_err(|e| format!("❌ Error saving file '{}': {}", filename, e))?;

        rows.push(Submission {
            id: None,
            user_id: message.sender_id,
            user_email: message.sender_email.clone(),
            user_full_name: message.sender_full_name.clone(),
            submission_name: submission_name.to_string(),
            timestamp: timestamp.clone(),
            file_checksum: checksum,
            file_path,
            expected_gain: None,
            actual_gain: private_result.gain,
            tp: private_result.tp,
            tn: private_result.tn,
            fp: private_result.fp,
            fn_: private_result.fn_,
            positives_predicted: predicted_ids.len() as i32,
            threshold_category: "kaggle".to_string(),
            after_deadline,
            used_golden_bullet: false,
            batch_id: Some(batch_id.clone()),
            public_gain: Some(public_result.gain),
            private_gain: Some(private_result.gain),
            is_baseline,
            baseline_published: false,
        });
    }

    Ok((batch_id, rows))
}

/// The candidate a kaggle batch is scored by: best on public, ties going to
/// the highest id -- exactly the `ORDER BY public_gain DESC, id DESC` that
/// `get_leaderboard` and `get_baselines` use, so any view showing "its
/// private gain" shows the value grades actually use. Rows not yet saved
/// have no id; among those, the last one wins, which is the one `save_batch`
/// will give the highest id.
fn best_on_public<'a>(rows: impl IntoIterator<Item = &'a Submission>) -> Option<&'a Submission> {
    rows.into_iter().max_by(|a, b| {
        a.public_gain
            .partial_cmp(&b.public_gain)
            .expect("gains are never NaN")
            .then(a.id.cmp(&b.id))
    })
}

/// Max candidate CSVs in one `baseline` upload. Teachers aren't on the
/// roster, so there's no per-person `max_files_per_submission` to read; this
/// only guards against an accidental huge upload.
const MAX_BASELINE_FILES: u32 = 20;

/// A teacher's `baseline ...` command, parsed. Pure, so it's unit-testable.
#[derive(Debug, PartialEq)]
pub enum BaselineCommand<'a> {
    Upload(&'a str),
    Publish(&'a str),
    Hide(&'a str),
    List,
    Usage,
}

/// Parses everything after the `baseline` word, from the original
/// (not lowercased) message so the name and id keep their case. Subcommands
/// match case-insensitively; `list`, `publish` and `hide` therefore can't be
/// used as a baseline's name. A first word starting with `[` is an
/// attachment link, meaning the name was left out.
pub fn parse_baseline_command(args: &str) -> BaselineCommand<'_> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let Some(&first) = parts.first() else {
        return BaselineCommand::Usage;
    };
    match (first.to_lowercase().as_str(), &parts[1..]) {
        ("list", []) => BaselineCommand::List,
        ("publish", [id]) => BaselineCommand::Publish(id),
        ("hide", [id]) => BaselineCommand::Hide(id),
        ("list" | "publish" | "hide", _) => BaselineCommand::Usage,
        _ if first.starts_with('[') => BaselineCommand::Usage,
        _ => BaselineCommand::Upload(first),
    }
}

pub fn baseline_usage() -> String {
    "❌ Usage:\n\
     • `baseline <name>` and attach one or more CSVs -- upload a reference model (hidden until published)\n\
     • `baseline list` -- every baseline, with its id and whether it's published\n\
     • `baseline publish <id>` / `baseline hide <id>` -- show or hide it on the public leaderboard"
        .to_string()
}

/// A teacher's `baseline <name>`: the same kaggle pipeline as a student's
/// `submit`, stored with `is_baseline` set and hidden until published. No
/// quota and no deadline gate -- a baseline isn't competing -- but it's still
/// scored on both splits, so a teacher sees exactly where it would land.
pub async fn process_baseline_upload(
    message: &Message,
    config: &BotConfig,
    db: &Database,
    master_data: &MasterData,
    name: &str,
) -> String {
    info!("Processing baseline upload from {}", message.sender_email);

    let after_deadline = config
        .competition
        .deadline_utc()
        .map(|deadline| Utc::now() > deadline)
        .unwrap_or(false);

    let (batch_id, rows) = match build_kaggle_batch(
        message,
        config,
        master_data,
        name,
        MAX_BASELINE_FILES,
        "baseline <name>",
        after_deadline,
        true,
    )
    .await
    {
        Ok(batch) => batch,
        Err(reply) => return reply,
    };

    if let Err(e) = db.save_batch(&rows) {
        return format!("❌ Error saving baseline: {}", e);
    }

    // Same "best on public, then its private gain" rule as a competitor's batch.
    let best = best_on_public(&rows).expect("build_kaggle_batch never returns an empty batch");

    format!(
        "📐 **Baseline stored:** {name} ({n} candidate{s})\n\
         📊 Best public gain: {public:.4} -- its private gain: {private:.4}\n\
         🆔 **Baseline ID**: `{batch_id}`\n\n\
         🔒 Hidden from the public leaderboard. Publish it with `baseline publish {batch_id}`.\n\
         Baselines are never ranked or graded.",
        n = rows.len(),
        s = if rows.len() == 1 { "" } else { "s" },
        public = best.public_gain.unwrap_or_default(),
        private = best.private_gain.unwrap_or_default(),
    )
}

pub fn process_baseline_list(db: &Database) -> String {
    let baselines = match db.get_baselines() {
        Ok(b) => b,
        Err(e) => return format!("❌ Error retrieving baselines: {}", e),
    };
    if baselines.is_empty() {
        return "📐 No baselines yet. Upload one with `baseline <name>` and attach the CSVs.".to_string();
    }

    let mut response = "📐 **Baselines**\n\n".to_string();
    response.push_str("| Name | 📅 Date | 🔢 Candidates | 📊 Best public | 💰 Its private | Public board | 🆔 ID |\n");
    response.push_str("|---|---|---|---|---|---|---|\n");
    for b in &baselines {
        let ts: String = b.timestamp.chars().take(16).collect();
        response.push_str(&format!(
            "| {} | {} | {} | {:.2} | {:.2} | {} | `{}` |\n",
            b.name,
            ts,
            b.candidates,
            b.best_public_gain,
            b.private_gain,
            published_mark(b.published),
            b.batch_id
        ));
    }
    response
}

pub fn process_baseline_visibility(db: &Database, batch_id: &str, published: bool) -> String {
    match db.set_baseline_published(batch_id, published) {
        Ok(true) if published => format!(
            "🌐 Baseline `{batch_id}` is now shown on the public leaderboard."
        ),
        Ok(true) => format!("🔒 Baseline `{batch_id}` is now hidden from the public leaderboard."),
        Ok(false) => format!(
            "❌ No baseline with ID `{batch_id}`. See `baseline list` for the IDs."
        ),
        Err(e) => format!("❌ Error updating the baseline: {}", e),
    }
}

fn published_mark(published: bool) -> &'static str {
    if published {
        "🌐 shown"
    } else {
        "🔒 hidden"
    }
}

/// Formats an optional gain field for display -- `None` (a competitor with
/// no valid entry, or kaggle mode's now-unused `expected_gain`) shows "N/A"
/// rather than a fake number.
fn fmt_gain(g: Option<f64>) -> String {
    g.map(|v| format!("{:.2}", v)).unwrap_or_else(|| "N/A".to_string())
}

/// Population mean and standard deviation (denominator N, not N-1): this is
/// descriptive feedback about the student's own batch, not an estimate of
/// some larger population, and N-1 would divide by zero for a single
/// candidate. A batch of 1 has std 0, cleanly.
fn mean_and_std(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    (mean, variance.sqrt())
}

/// Checks whether the full results can already be revealed.
/// On an unreadable date, reveals nothing (fails safe).
fn results_revealed(config: &BotConfig) -> bool {
    match config.competition.results_reveal_utc() {
        Ok(reveal_dt) => Utc::now() >= reveal_dt,
        Err(e) => {
            warn!("Not revealing results, reveal date is unparseable: {:#}", e);
            false
        }
    }
}

/// The largest `text` `build_student_listing` will produce, in characters.
/// Zulip silently cuts a message at 10,000 characters (`[message truncated]`,
/// mid-row, and the bot is told the send succeeded), so the table is kept
/// well under that, with room for the CSV link appended afterwards.
const LISTING_BUDGET: usize = 9000;
/// Characters set aside inside the budget for footers and the CSV link line.
const LISTING_FOOTER_RESERVE: usize = 600;

/// A student's `list submits`, built but not yet sent.
pub struct StudentListing {
    /// The message: a heading, the table for the last two days, footnotes.
    /// Never contains the CSV link -- that needs an upload, see
    /// `process_list_submits`.
    pub text: String,
    /// Every submit, ever, as plain CSV: the same columns the table shows
    /// (nothing the table hides), but with no emoji, backticks or `±`.
    pub csv: Vec<u8>,
    /// Submits in the CSV (batches, in kaggle mode).
    pub total: usize,
}

/// One submit as `list submits` shows it, in both of its forms.
struct ListEntry {
    /// As stored (RFC3339), to decide whether it's in the two-day window.
    timestamp: String,
    /// The markdown table row, newline-terminated.
    row: String,
    /// The same submit as CSV fields.
    csv: Vec<String>,
}

/// Everything that differs between modes (and between before and after the
/// reveal date in blind mode): the columns, the entries, the footnote.
struct ListView {
    table_header: &'static str,
    csv_header: &'static [&'static str],
    entries: Vec<ListEntry>,
    footer: Option<String>,
}

/// The competition's clock -- the one `timezone_offset_minutes` governs for
/// the deadline and the daily quota -- so "the last two days" means the
/// student's own days, not UTC's. An out-of-range offset (rejected by
/// `validate()`, so unreachable in practice) falls back to UTC.
fn competition_tz(config: &BotConfig) -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(config.competition.timezone_offset_minutes * 60)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is valid"))
}

/// `ts` as local competition time in `format`; the raw stored text if it
/// somehow doesn't parse, rather than hiding a submit over a bad timestamp.
fn local_time(ts: &str, tz: chrono::FixedOffset, format: &str) -> String {
    match parse_timestamp(ts) {
        Some(t) => t.with_timezone(&tz).format(format).to_string(),
        None => ts.to_string(),
    }
}

fn yes_no(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

/// Builds a student's `list submits` from their stored rows (newest first,
/// as the DB returns them): a table of the last two local days, and a CSV of
/// the full history. `now` is a parameter so the window is testable.
pub fn build_student_listing(
    submissions: &[Submission],
    config: &BotConfig,
    now: DateTime<Utc>,
) -> Result<StudentListing> {
    let tz = competition_tz(config);
    let view = match config.competition.mode {
        crate::config::CompetitionMode::Kaggle => kaggle_view(submissions, tz),
        crate::config::CompetitionMode::Blind => blind_view(submissions, config, tz),
    };

    // Today and yesterday, by the competition's calendar.
    let window_start = config.competition.local_day_bounds_utc(now).0 - chrono::Duration::days(1);
    let recent: Vec<&ListEntry> = view
        .entries
        .iter()
        .filter(|e| parse_timestamp(&e.timestamp).is_none_or(|t| t >= window_start))
        .collect();

    let mut text = format!(
        "📋 **Your Submissions** -- last 2 days (since {}, times in {})\n\n",
        window_start.with_timezone(&tz).format("%Y-%m-%d"),
        now.with_timezone(&tz).format("UTC%:z"),
    );

    let header_chars = view.table_header.chars().count();
    let mut used = text.chars().count() + header_chars + LISTING_FOOTER_RESERVE;
    let fits = recent
        .iter()
        .take_while(|e| {
            used += e.row.chars().count();
            used <= LISTING_BUDGET
        })
        .count();

    if recent.is_empty() {
        text.push_str("No submits in the last 2 days.\n");
    } else {
        if fits > 0 {
            text.push_str(view.table_header);
            for entry in &recent[..fits] {
                text.push_str(&entry.row);
            }
        }
        if fits < recent.len() {
            text.push_str(&format!(
                "\n…and {} more from these two days, only in the CSV.\n",
                recent.len() - fits
            ));
        }
    }
    if let Some(footer) = &view.footer {
        text.push_str(&format!("\n{footer}"));
    }

    let mut writer = csv::Writer::from_writer(vec![]);
    writer.write_record(view.csv_header)?;
    for entry in &view.entries {
        writer.write_record(&entry.csv)?;
    }
    writer.flush()?;

    Ok(StudentListing {
        text,
        csv: writer.into_inner()?,
        total: view.entries.len(),
    })
}

/// Sends a student's `list submits`: the last two days in the message, and
/// the whole history as a CSV attachment (uploaded like `grades`' and
/// `all submits`'). If the upload fails the table is still sent, with a note.
pub async fn process_list_submits(
    user_id: i64,
    db: &Database,
    config: &BotConfig,
    client: &ZulipClient,
) -> String {
    let submissions = match db.get_user_submissions(user_id) {
        Ok(s) => s,
        Err(e) => return format!("❌ Error retrieving submissions: {}", e),
    };

    if submissions.is_empty() {
        return "📋 You have no recorded submissions".to_string();
    }

    let StudentListing { text, csv, total } =
        match build_student_listing(&submissions, config, Utc::now()) {
            Ok(listing) => listing,
            Err(e) => return format!("❌ Error preparing your submissions: {}", e),
        };

    let filename = format!("my_submissions_{}.csv", Utc::now().format("%Y%m%d_%H%M%S"));
    match client.upload_file(&filename, csv, "text/csv").await {
        Ok(url) => format!(
            "{text}\n\n📎 **Full history** ({total} submit{}, CSV): [{filename}]({url})",
            if total == 1 { "" } else { "s" },
        ),
        Err(e) => {
            warn!("Could not upload {}'s submissions CSV: {}", user_id, e);
            format!("{text}\n\n⚠️ Couldn't attach your full history as a CSV right now -- try again in a moment.")
        }
    }
}

/// Groups candidate rows into one entry per submit (`batch_id`, or the row's
/// own id when it has none), keeping `submissions`' order of first
/// appearance. Doesn't assume a batch's rows are adjacent -- only that every
/// row of one batch shares an identical timestamp (true by construction, see
/// `build_kaggle_batch`) and that two batches never share one (wall-clock,
/// effectively impossible to collide). So with `submissions` in timestamp
/// DESC order, as the DB returns it, the batches come out newest first with
/// no separate sort, and the order of candidates within a batch doesn't matter.
fn group_by_batch(submissions: &[Submission]) -> Vec<(String, Vec<&Submission>)> {
    let mut batches: HashMap<String, Vec<&Submission>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for sub in submissions {
        let key = sub
            .batch_id
            .clone()
            .unwrap_or_else(|| sub.id.unwrap_or(0).to_string());
        if !batches.contains_key(&key) {
            order.push(key.clone());
        }
        batches.entry(key).or_default().push(sub);
    }
    order
        .into_iter()
        .map(|key| {
            let rows = batches.remove(&key).expect("every ordered key was inserted");
            (key, rows)
        })
        .collect()
}

/// Kaggle mode: one row per BATCH (submit call), showing the mean/std of its
/// candidates' PUBLIC gain -- the same aggregate the submit reply itself
/// showed, never any individual candidate's score. Never `actual_gain`
/// either (which mirrors the reveal-gated PRIVATE gain for kaggle rows);
/// that value stays hidden until a teacher reads the leaderboard for this
/// student's last pre-deadline batch, and showing even one candidate's own
/// gain here -- in the table or the CSV -- would leak strictly more than the
/// aggregate already does. Not gated by the reveal date at all: the
/// aggregate was already disclosed at submit time, so repeating it isn't a
/// new secret.
fn kaggle_view(submissions: &[Submission], tz: chrono::FixedOffset) -> ListView {
    let entries = group_by_batch(submissions)
        .into_iter()
        .map(|(key, rows)| {
            let gains: Vec<f64> = rows.iter().filter_map(|s| s.public_gain).collect();
            let (mean, std_dev) = mean_and_std(&gains);
            let first = rows[0];
            ListEntry {
                timestamp: first.timestamp.clone(),
                row: format!(
                    "|{}|{}|{}|{:.2}|{:.2}|`{}`|{}|\n",
                    first.submission_name,
                    local_time(&first.timestamp, tz, "%Y-%m-%d %H:%M"),
                    rows.len(),
                    mean,
                    std_dev,
                    key,
                    if first.after_deadline { "⚠️" } else { "✅" },
                ),
                csv: vec![
                    first.submission_name.clone(),
                    local_time(&first.timestamp, tz, "%Y-%m-%d %H:%M:%S"),
                    rows.len().to_string(),
                    mean.to_string(),
                    std_dev.to_string(),
                    key,
                    yes_no(!first.after_deadline).to_string(),
                ],
            }
        })
        .collect();

    ListView {
        table_header: "| Name | 📅 Date | 🔢 Candidates | 📊 Public mean | 📉 Public std | 🆔 Batch | ⏰ |\n\
                       |---|---|---|---|---|---|---|\n",
        csv_header: &["name", "submitted_at", "candidates", "public_mean", "public_std", "batch_id", "on_time"],
        entries,
        footer: Some("ℹ️ The private gain is only known for your LAST pre-deadline submission.".to_string()),
    }
}

/// Blind mode: one row per submission. Before `results_reveal_date` the
/// actual gain is hidden -- from the table and the CSV alike, which show
/// exactly the same columns -- and a note says when it will be revealed.
fn blind_view(submissions: &[Submission], config: &BotConfig, tz: chrono::FixedOffset) -> ListView {
    let show_results = results_revealed(config);

    let entries = submissions
        .iter()
        .map(|sub| {
            let date = local_time(&sub.timestamp, tz, "%Y-%m-%d %H:%M");
            let mark = if sub.after_deadline { "⚠️" } else { "✅" };
            let mut csv = vec![
                sub.id.unwrap_or(0).to_string(),
                sub.submission_name.clone(),
                local_time(&sub.timestamp, tz, "%Y-%m-%d %H:%M:%S"),
                sub.expected_gain.map(|g| g.to_string()).unwrap_or_default(),
            ];
            if show_results {
                csv.push(sub.actual_gain.to_string());
            }
            csv.push(sub.threshold_category.clone());
            csv.push(yes_no(!sub.after_deadline).to_string());

            let row = if show_results {
                format!(
                    "|{}|{}|{}|{}|{:.2}|{}|{}|\n",
                    sub.id.unwrap_or(0),
                    sub.submission_name,
                    date,
                    fmt_gain(sub.expected_gain),
                    sub.actual_gain,
                    sub.threshold_category,
                    mark
                )
            } else {
                format!(
                    "|{}|{}|{}|{}|{}|{}|\n",
                    sub.id.unwrap_or(0),
                    sub.submission_name,
                    date,
                    fmt_gain(sub.expected_gain),
                    sub.threshold_category,
                    mark
                )
            };
            ListEntry {
                timestamp: sub.timestamp.clone(),
                row,
                csv,
            }
        })
        .collect();

    let (table_header, csv_header): (&'static str, &'static [&'static str]) = if show_results {
        (
            "| ID | Name | 📅 Date | 💰 Expected | ✨ Actual | 🎯 Category | ⏰ |\n\
             |---|---|---|---|---|---|---|\n",
            &["id", "name", "submitted_at", "expected_gain", "actual_gain", "category", "on_time"],
        )
    } else {
        (
            "| ID | Name | 📅 Date | 💰 Expected | 🎯 Category | ⏰ |\n\
             |---|---|---|---|---|---|\n",
            &["id", "name", "submitted_at", "expected_gain", "category", "on_time"],
        )
    };

    let footer = (!show_results).then(|| {
        let reveal_str: String = config.competition.results_reveal_date.chars().take(16).collect();
        format!("📊 *Full results will be revealed on {}*", reveal_str)
    });

    ListView {
        table_header,
        csv_header,
        entries,
        footer,
    }
}

pub fn process_duplicates(db: &Database) -> String {
    let duplicates = match db.get_duplicates() {
        Ok(d) => d,
        Err(e) => return format!("❌ Error retrieving duplicates: {}", e),
    };

    if duplicates.is_empty() {
        return "✅ No duplicate submissions found".to_string();
    }

    let mut response = "🔍 **Duplicate Submissions:**\n\n".to_string();
    for (checksum, _count, users, names) in duplicates {
        response.push_str(&format!("**Checksum:** `{}...`\n", &checksum[..16]));
        response.push_str(&format!("**Users:** {}\n", users));
        response.push_str(&format!("**Submissions:** {}\n\n", names));
    }

    response
}

pub fn process_leaderboard_full(db: &Database, config: &BotConfig, order_by: &str) -> String {
    let results = match db.get_leaderboard(order_by, config.competition.mode) {
        Ok(r) => r,
        Err(e) => return format!("❌ Error retrieving leaderboard: {}", e),
    };
    let baselines = match config.competition.mode {
        crate::config::CompetitionMode::Kaggle => match db.get_baselines() {
            Ok(b) => b,
            Err(e) => return format!("❌ Error retrieving baselines: {}", e),
        },
        crate::config::CompetitionMode::Blind => Vec::new(),
    };

    if results.is_empty() && baselines.is_empty() {
        return "📊 No submissions on the leaderboard".to_string();
    }

    let by_date = order_by == "datetime";
    let order_label = if by_date { "Sorted by date" } else { "Sorted by gain" };

    let mut response = format!(
        "🏆 **Full Leaderboard - {} ({})** \n\n",
        config.competition.name,
        order_label
    );
    response.push_str("| Pos | Name | TS | 💰 Final | 💰 Expected | 📊 Submissions | 📈 Max |\n");
    response.push_str("|---|---|---|---|---|---|---|\n");

    // Baselines are interleaved at the position their own score (or date)
    // would put them, in the same order the SQL sorted the competitors by --
    // but with no position number, so no competitor's position shifts.
    let mut baselines: Vec<&BaselineSummary> = baselines.iter().collect();
    let beats = |b: &BaselineSummary, gain: f64, ts: &str| -> bool {
        if by_date {
            parse_timestamp(&b.timestamp) > parse_timestamp(ts)
        } else {
            b.private_gain > gain
        }
    };
    baselines.sort_by(|a, b| {
        if by_date {
            parse_timestamp(&b.timestamp).cmp(&parse_timestamp(&a.timestamp))
        } else {
            b.private_gain
                .partial_cmp(&a.private_gain)
                .expect("gains are never NaN")
        }
    });
    let mut pending = baselines.into_iter().peekable();

    let mut position = 0;
    for (name, email, ts, best_gain, expected_gain, total, max_gain, used_bullet) in &results {
        if config.teachers.contains(email) {
            continue;
        }
        while let Some(b) = pending.next_if(|b| beats(b, *best_gain, ts)) {
            response.push_str(&baseline_leaderboard_line(b));
        }
        position += 1;
        let ts_str: String = ts.chars().take(16).collect();
        let bullet_mark = if *used_bullet { " 🌟" } else { "" };
        response.push_str(&format!(
            "| {} | {}{} | {} | {:.2} | {} | {} | {} |\n",
            position,
            name,
            bullet_mark,
            ts_str,
            best_gain,
            fmt_gain(*expected_gain),
            total,
            fmt_gain(*max_gain)
        ));
    }
    for b in pending {
        response.push_str(&baseline_leaderboard_line(b));
    }

    response
}

fn baseline_leaderboard_line(b: &BaselineSummary) -> String {
    let ts: String = b.timestamp.chars().take(16).collect();
    format!(
        "| — | 📐 **Baseline: {}** ({}) | {} | {:.2} | N/A | — | — |\n",
        b.name,
        published_mark(b.published),
        ts,
        b.private_gain
    )
}

/// Parses a stored RFC3339 timestamp for ordering. Comparing the strings
/// directly isn't safe: `to_rfc3339()`'s fractional-second width varies.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts).ok().map(|t| t.with_timezone(&Utc))
}

/// One row of the on-demand grade export. `gain` is the same "last valid
/// pre-deadline submission" value the leaderboard ranks by (`final_gain`),
/// not each competitor's best -- kept identical deliberately, so a grade and
/// the leaderboard row it came from never disagree. `grade` is just one more
/// field alongside it, not a separate report.
#[derive(Debug, Clone, PartialEq)]
pub struct GradeRow {
    pub email: String,
    pub full_name: String,
    pub gain: f64,
    pub grade: f64,
    /// Everything below mirrors the private leaderboard row this grade came
    /// from (`None`/`0`/`false` for a competitor with no valid entry there).
    pub expected_gain: Option<f64>,
    pub timestamp: Option<String>,
    pub total_submissions: i32,
    pub max_gain: Option<f64>,
    pub used_golden_bullet: bool,
}

struct LeaderboardEntry {
    expected_gain: Option<f64>,
    timestamp: String,
    gain: f64,
    total_submissions: i32,
    max_gain: Option<f64>,
    used_golden_bullet: bool,
}

/// `grade = 8 + 2*(gain - median) / (max - median)`, floored at 0.
/// Median and max are computed only over roster competitors who have a valid
/// pre-deadline submission (the same set the leaderboard ranks); anyone else
/// gets a flat 0. Pulls from `get_leaderboard`, so it can never drift from
/// what the leaderboard itself shows for the same competitor -- `GradeRow`
/// carries that leaderboard row along (timestamp, submissions, max, golden
/// bullet) rather than just the bare gain, so `grades` can show the same context
/// a teacher sees in `leaderboard` without a second query.
pub fn compute_grades(db: &Database, roster: &Roster, config: &BotConfig) -> Result<Vec<GradeRow>> {
    let leaderboard = db.get_leaderboard("gain", config.competition.mode)?;

    let mut by_email: HashMap<String, LeaderboardEntry> = HashMap::new();
    for (_, email, timestamp, actual_gain, expected_gain, total, max_gain, used_bullet) in &leaderboard {
        if config.teachers.contains(email) {
            continue;
        }
        by_email.insert(
            email.to_lowercase(),
            LeaderboardEntry {
                expected_gain: *expected_gain,
                timestamp: timestamp.clone(),
                gain: *actual_gain,
                total_submissions: *total,
                max_gain: *max_gain,
                used_golden_bullet: *used_bullet,
            },
        );
    }

    let mut gains: Vec<f64> = by_email.values().map(|e| e.gain).collect();
    gains.sort_by(|a, b| a.partial_cmp(b).expect("gains are never NaN"));
    let median = median_of(&gains);
    let max = gains.iter().copied().fold(f64::MIN, f64::max);

    let mut rows: Vec<GradeRow> = roster
        .competitors()
        .map(|c| {
            let entry = by_email.get(&c.email);
            GradeRow {
                email: c.email.clone(),
                full_name: c.full_name.clone(),
                gain: entry.map(|e| e.gain).unwrap_or(0.0),
                grade: entry.map(|e| grade_for(e.gain, median, max)).unwrap_or(0.0),
                expected_gain: entry.and_then(|e| e.expected_gain),
                timestamp: entry.map(|e| e.timestamp.clone()),
                total_submissions: entry.map(|e| e.total_submissions).unwrap_or(0),
                max_gain: entry.and_then(|e| e.max_gain),
                used_golden_bullet: entry.map(|e| e.used_golden_bullet).unwrap_or(false),
            }
        })
        .collect();

    rows.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    Ok(rows)
}

fn median_of(sorted_gains: &[f64]) -> f64 {
    let n = sorted_gains.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        sorted_gains[n / 2]
    } else {
        (sorted_gains[n / 2 - 1] + sorted_gains[n / 2]) / 2.0
    }
}

fn grade_for(gain: f64, median: f64, max: f64) -> f64 {
    let spread = max - median;
    let raw = if spread.abs() < f64::EPSILON {
        // Degenerate case: median == max, e.g. a single submitter, or at
        // least half the submitters already at the top score. The formula's
        // linear interpolation has zero width, so anchor directly instead of
        // dividing by ~0: at the max score a 10, anything below an 8.
        if gain >= max {
            10.0
        } else {
            8.0
        }
    } else {
        8.0 + 2.0 * (gain - median) / spread
    };
    raw.max(0.0)
}

fn build_grades_csv(rows: &[GradeRow]) -> Result<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(vec![]);
    writer.write_record([
        "email",
        "name",
        "gain",
        "expected_gain",
        "submission_date",
        "submissions",
        "max",
        "golden_bullet",
        "grade",
    ])?;
    for row in rows {
        writer.write_record([
            row.email.as_str(),
            row.full_name.as_str(),
            // Full precision -- `to_string()` on an f64 prints the shortest
            // round-trippable representation, not a fixed decimal count, so
            // none of these (including `grade` itself) are rounded the way
            // `format!("{:.N}", ...)` would.
            &row.gain.to_string(),
            &row.expected_gain.map(|g| g.to_string()).unwrap_or_default(),
            row.timestamp.as_deref().unwrap_or(""),
            &row.total_submissions.to_string(),
            &row.max_gain.map(|g| g.to_string()).unwrap_or_default(),
            if row.used_golden_bullet { "yes" } else { "no" },
            &row.grade.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(writer.into_inner()?)
}

/// Builds the grades CSV and uploads it to Zulip, on demand -- there is no
/// automatic export. Takes already-computed rows (not `db`/`roster`
/// directly) so the caller can drop any `RefCell` borrow on the roster
/// before this `.await`s; see the `roster reload` comment in main.rs.
pub async fn export_grades_csv(rows: &[GradeRow], client: &ZulipClient) -> String {
    let csv_bytes = match build_grades_csv(rows) {
        Ok(b) => b,
        Err(e) => return format!("❌ Error generating the grades CSV: {}", e),
    };

    let filename = format!("grades_{}.csv", Utc::now().format("%Y%m%d_%H%M%S"));
    match client.upload_file(&filename, csv_bytes, "text/csv").await {
        Ok(url) => format!(
            "📊 **Grades generated** ({} competitors on the roster)\n\n[{}]({})",
            rows.len(),
            filename,
            url
        ),
        Err(e) => format!("❌ Error uploading the CSV to Zulip: {}", e),
    }
}

/// The name inside the first Zulip mention in `content` -- `@**Name**`, or
/// `@**Name|123**`, the form Zulip uses to disambiguate two users with the
/// same name (the `|123` user id is dropped) -- plus everything after the
/// mention. Accepts anything but `*` in the name, so hyphens, periods and
/// apostrophes ("Diego R.", "O'Brien") work. Silent mentions (`@_**Name**`)
/// are accepted too.
pub fn split_mention(content: &str) -> Option<(String, &str)> {
    let re = Regex::new(r"@_?\*\*([^*]+)\*\*").expect("static regex");
    let caps = re.captures(content)?;
    let inner = caps.get(1)?.as_str();
    let name = inner.split('|').next().unwrap_or(inner).trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), &content[caps.get(0)?.end()..]))
}

/// What `user submits` sorts by. In kaggle mode: `Gain` is the private gain
/// of the batch's best-on-public candidate (the graded value), `Mean` the
/// batch's public mean, `Max` its best public gain. In blind mode a
/// submission has a single gain, so all three sort by `actual_gain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitSortKey {
    Gain,
    Mean,
    Max,
    Date,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitOrder {
    pub key: SubmitSortKey,
    pub descending: bool,
}

impl Default for SubmitOrder {
    /// Newest first -- what `user submits` always showed before it was sortable.
    fn default() -> Self {
        Self {
            key: SubmitSortKey::Date,
            descending: true,
        }
    }
}

impl SubmitOrder {
    fn label(self) -> String {
        let (key, high_first, low_first) = match self.key {
            SubmitSortKey::Gain => ("gain", "highest first", "lowest first"),
            SubmitSortKey::Mean => ("mean", "highest first", "lowest first"),
            SubmitSortKey::Max => ("max", "highest first", "lowest first"),
            SubmitSortKey::Date => ("date", "newest first", "oldest first"),
        };
        format!("{key}, {}", if self.descending { high_first } else { low_first })
    }
}

pub fn user_submits_usage(problem: &str) -> String {
    format!(
        "❌ {problem}\n\nUsage: `user submits @user [gain|mean|max|date] [asc|desc]` \
         (use a real Zulip mention; default: `date desc`)"
    )
}

/// Parses the optional sort words after the mention, in either order and
/// case-insensitively. A missing key means `date`; a missing direction
/// means `desc`.
pub fn parse_submit_order(args: &str) -> Result<SubmitOrder, String> {
    let mut key = None;
    let mut descending = None;
    for word in args.split_whitespace() {
        let parsed_key = match word.to_lowercase().as_str() {
            "gain" => Some(SubmitSortKey::Gain),
            "mean" => Some(SubmitSortKey::Mean),
            "max" => Some(SubmitSortKey::Max),
            "date" => Some(SubmitSortKey::Date),
            "asc" | "desc" if descending.is_none() => {
                descending = Some(word.eq_ignore_ascii_case("desc"));
                continue;
            }
            _ => None,
        };
        match parsed_key {
            Some(k) if key.is_none() => key = Some(k),
            _ => return Err(user_submits_usage(&format!("Unexpected `{word}`."))),
        }
    }
    let default = SubmitOrder::default();
    Ok(SubmitOrder {
        key: key.unwrap_or(default.key),
        descending: descending.unwrap_or(default.descending),
    })
}

/// Sorts by `order`: dates are parsed rather than compared as strings
/// (`to_rfc3339()`'s fractional-second width varies), and ties always fall
/// back to newest first.
fn sort_for_user_submits<T>(
    items: &mut [T],
    order: SubmitOrder,
    gain: impl Fn(&T, SubmitSortKey) -> f64,
    timestamp: impl Fn(&T) -> &str,
) {
    items.sort_by(|a, b| {
        let primary = match order.key {
            SubmitSortKey::Date => parse_timestamp(timestamp(a)).cmp(&parse_timestamp(timestamp(b))),
            key => gain(a, key)
                .partial_cmp(&gain(b, key))
                .expect("gains are never NaN"),
        };
        let primary = if order.descending { primary.reverse() } else { primary };
        primary.then_with(|| parse_timestamp(timestamp(b)).cmp(&parse_timestamp(timestamp(a))))
    });
}

pub fn process_user_submits(
    user_identifier: &str,
    order: SubmitOrder,
    db: &Database,
    config: &BotConfig,
) -> String {
    let submissions = match db.get_user_submissions_by_identifier(user_identifier) {
        Ok(s) => s,
        Err(e) => return format!("❌ Error retrieving submissions: {}", e),
    };

    if submissions.is_empty() {
        return format!("📋 No submissions found for '{}'", user_identifier);
    }

    if config.competition.mode == crate::config::CompetitionMode::Kaggle {
        return user_submits_kaggle(user_identifier, &submissions, order);
    }

    let mut submissions = submissions;
    sort_for_user_submits(&mut submissions, order, |s, _| s.actual_gain, |s| &s.timestamp);

    let mut response = format!(
        "📋 **Submissions for '{}'** (sorted by {}):\n\n",
        user_identifier,
        order.label()
    );
    response.push_str("| ID | Name | 📅 Date | 💰 Expected | ✨ Actual | 🎯 | ⏰ |\n");
    response.push_str("|---|---|---|---|---|---|---|\n");

    for sub in submissions {
        let deadline_mark = if sub.after_deadline { "⚠️" } else { "✅" };
        let ts_str: String = sub.timestamp.chars().take(16).collect();
        response.push_str(&format!(
            "|{}|{}|{}|{}|{:.2}|{}|{}|\n",
            sub.id.unwrap_or(0),
            sub.submission_name,
            ts_str,
            fmt_gain(sub.expected_gain),
            sub.actual_gain,
            sub.threshold_category,
            deadline_mark
        ));
    }

    response
}

/// Kaggle mode's `user submits`: one row per submit, however many candidate
/// CSVs it had. Teacher-only, so unlike a student's `list submits` it shows
/// the private side too: the batch's best candidate on PUBLIC and that
/// candidate's PRIVATE gain -- the exact value the leaderboard and grades
/// use if this turns out to be the student's last pre-deadline batch.
fn user_submits_kaggle(user_identifier: &str, submissions: &[Submission], order: SubmitOrder) -> String {
    struct BatchRow<'a> {
        key: String,
        candidates: usize,
        best: &'a Submission,
        mean: f64,
        std_dev: f64,
    }

    let mut batches: Vec<BatchRow> = group_by_batch(submissions)
        .into_iter()
        .map(|(key, rows)| {
            let best = best_on_public(rows.iter().copied()).expect("a batch is never empty");
            let gains: Vec<f64> = rows.iter().filter_map(|s| s.public_gain).collect();
            let (mean, std_dev) = mean_and_std(&gains);
            BatchRow { key, candidates: rows.len(), best, mean, std_dev }
        })
        .collect();

    sort_for_user_submits(
        &mut batches,
        order,
        |b, key| match key {
            SubmitSortKey::Gain => b.best.private_gain.unwrap_or(f64::NEG_INFINITY),
            SubmitSortKey::Max => b.best.public_gain.unwrap_or(f64::NEG_INFINITY),
            SubmitSortKey::Mean | SubmitSortKey::Date => b.mean,
        },
        |b| &b.best.timestamp,
    );

    let mut response = format!(
        "📋 **Submissions for '{}'** (sorted by {}):\n\n",
        user_identifier,
        order.label()
    );
    response.push_str(
        "| Name | 📅 Date | 🔢 Candidates | 🏆 Best public | 💰 Its private | 📊 Public mean ± std | 🆔 Batch | ⏰ |\n",
    );
    response.push_str("|---|---|---|---|---|---|---|---|\n");

    for BatchRow { key, candidates, best, mean, std_dev } in &batches {
        let name = if best.is_baseline {
            format!("📐 {}", best.submission_name)
        } else {
            best.submission_name.clone()
        };
        let deadline_mark = if best.after_deadline { "⚠️" } else { "✅" };
        let ts: String = best.timestamp.chars().take(16).collect();
        response.push_str(&format!(
            "|{}|{}|{}|{}|{}|{:.2} ± {:.2}|`{}`|{}|\n",
            name,
            ts,
            candidates,
            fmt_gain(best.public_gain),
            fmt_gain(best.private_gain),
            mean,
            std_dev,
            key,
            deadline_mark
        ));
    }

    response
}


/// Competitors on the roster with zero submissions of any kind. Replaces the
/// old Zulip-user-directory sweep (N+1 presence calls, one per active user):
/// the roster already IS the list of who is expected to compete, so this is
/// a single query plus a set difference.
pub fn process_no_submits(db: &Database, roster: &Roster) -> String {
    let submitted = match db.get_distinct_submitter_emails() {
        Ok(s) => s,
        Err(e) => return format!("❌ Error retrieving submissions: {}", e),
    };

    let mut missing: Vec<&Competitor> = roster
        .competitors()
        .filter(|c| !submitted.contains(&c.email))
        .collect();

    if missing.is_empty() {
        return format!(
            "✅ The whole roster ({} competitors) has submitted at least once",
            roster.len()
        );
    }

    missing.sort_by(|a, b| a.full_name.cmp(&b.full_name));

    let mut response = format!(
        "📋 **No Submissions ({} of {} on the roster):**\n\n",
        missing.len(),
        roster.len()
    );
    response.push_str("| # | Name | Email |\n");
    response.push_str("|---|---|---|\n");

    for (i, c) in missing.iter().enumerate() {
        response.push_str(&format!("| {} | {} | {} |\n", i + 1, c.full_name, c.email));
    }

    response
}


/// Every column `Submission` has, one row per stored candidate (kaggle mode's
/// unit) or submission (blind mode's), plus `candidates_in_batch` -- computed
/// here, not stored -- so a kaggle batch's size is visible on each of its own
/// rows without a second lookup. Connects to the rest of the export via
/// `batch_id`: every row sharing one is one submit call. `expected_gain`,
/// `public_gain`, and `private_gain` are blank, not `0`, when the mode that
/// produced this row never collects them (see `Submission`'s doc comments).
fn build_all_submissions_csv(submissions: &[Submission]) -> Result<Vec<u8>> {
    let mut batch_sizes: HashMap<&str, usize> = HashMap::new();
    for sub in submissions {
        if let Some(batch_id) = sub.batch_id.as_deref() {
            *batch_sizes.entry(batch_id).or_insert(0) += 1;
        }
    }

    let mut writer = csv::Writer::from_writer(vec![]);
    writer.write_record([
        "id",
        "user_email",
        "user_full_name",
        "submission_name",
        "timestamp",
        "batch_id",
        "candidates_in_batch",
        "expected_gain",
        "actual_gain",
        "public_gain",
        "private_gain",
        "tp",
        "tn",
        "fp",
        "fn",
        "positives_predicted",
        "threshold_category",
        "file_checksum",
        "file_path",
        "after_deadline",
        "used_golden_bullet",
        "is_baseline",
        "baseline_published",
    ])?;

    for sub in submissions {
        let candidates_in_batch = match sub.batch_id.as_deref() {
            Some(batch_id) => batch_sizes[batch_id],
            None => 1,
        };
        writer.write_record([
            sub.id.map(|i| i.to_string()).unwrap_or_default().as_str(),
            sub.user_email.as_str(),
            sub.user_full_name.as_str(),
            sub.submission_name.as_str(),
            sub.timestamp.as_str(),
            sub.batch_id.as_deref().unwrap_or(""),
            candidates_in_batch.to_string().as_str(),
            sub.expected_gain.map(|g| g.to_string()).unwrap_or_default().as_str(),
            sub.actual_gain.to_string().as_str(),
            sub.public_gain.map(|g| g.to_string()).unwrap_or_default().as_str(),
            sub.private_gain.map(|g| g.to_string()).unwrap_or_default().as_str(),
            sub.tp.to_string().as_str(),
            sub.tn.to_string().as_str(),
            sub.fp.to_string().as_str(),
            sub.fn_.to_string().as_str(),
            sub.positives_predicted.to_string().as_str(),
            sub.threshold_category.as_str(),
            sub.file_checksum.as_str(),
            sub.file_path.as_str(),
            if sub.after_deadline { "yes" } else { "no" },
            if sub.used_golden_bullet { "yes" } else { "no" },
            if sub.is_baseline { "yes" } else { "no" },
            if sub.baseline_published { "yes" } else { "no" },
        ])?;
    }

    writer.flush()?;
    Ok(writer.into_inner()?)
}

/// Builds the all-submissions CSV and uploads it to Zulip, on demand -- same
/// shape as `export_grades_csv`. Replaces the old markdown-table version,
/// which paginated into 50-row chunks joined by a literal `---PAGE_BREAK---`
/// that nothing ever split on, so every page beyond the first arrived as
/// unreadable raw text in one giant message; a file attachment has no such
/// limit to page around.
pub async fn export_all_submissions_csv(db: &Database, client: &ZulipClient) -> String {
    let submissions = match db.get_all_submissions() {
        Ok(s) => s,
        Err(e) => return format!("❌ Error retrieving submissions: {}", e),
    };

    if submissions.is_empty() {
        return "📋 No submissions recorded in the system".to_string();
    }

    let csv_bytes = match build_all_submissions_csv(&submissions) {
        Ok(b) => b,
        Err(e) => return format!("❌ Error generating the submissions CSV: {}", e),
    };

    let filename = format!("all_submissions_{}.csv", Utc::now().format("%Y%m%d_%H%M%S"));
    match client.upload_file(&filename, csv_bytes, "text/csv").await {
        Ok(url) => format!(
            "📋 **All submissions** ({} rows)\n\n[{}]({})",
            submissions.len(),
            filename,
            url
        ),
        Err(e) => format!("❌ Error uploading the CSV to Zulip: {}", e),
    }
}

// Helper functions

/// Every `[name.csv](url)` markdown link in the message, in the order they
/// appear -- no network access, so the caller can gate on the count (e.g.
/// `max_files_per_submission`) before downloading anything.
fn find_csv_links(content: &str) -> Result<Vec<(String, String)>> {
    let re = Regex::new(r"\[([^\]]+\.csv)\]\(([^)]+)\)")?;
    Ok(re
        .captures_iter(content)
        .map(|caps| {
            (
                caps.get(1).unwrap().as_str().to_string(),
                caps.get(2).unwrap().as_str().to_string(),
            )
        })
        .collect())
}

async fn download_attachment(url: &str, config: &BotConfig) -> Result<Vec<u8>> {
    let full_url = if url.starts_with("http") {
        url.to_string()
    } else {
        format!("{}{}", config.zulip.site, url)
    };

    let client = reqwest::Client::new();
    let response = client
        .get(&full_url)
        .basic_auth(&config.zulip.email, Some(&config.zulip.api_key))
        .send()
        .await?;

    Ok(response.bytes().await?.to_vec())
}

async fn extract_file_from_message(
    content: &str,
    config: &BotConfig,
) -> Result<Option<(String, Vec<u8>)>> {
    match find_csv_links(content)?.into_iter().next() {
        Some((filename, url)) => Ok(Some((filename, download_attachment(&url, config).await?))),
        None => Ok(None),
    }
}

fn save_submission_file(
    user_name: &str,
    submission_name: &str,
    filename: &str,
    content: &[u8],
    is_teacher: bool,
    config: &BotConfig,
) -> Result<String> {
    let base_path = PathBuf::from(&config.submissions.path);

    let safe_user_name: String = user_name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect();

    let user_dir = if is_teacher {
        base_path.join("teachers").join(safe_user_name)
    } else {
        base_path.join("students").join(safe_user_name)
    };

    fs::create_dir_all(&user_dir)?;

    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let safe_name: String = submission_name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect();

    let file_path = user_dir.join(format!("{}_{}_{}", timestamp, safe_name.trim(), filename));

    fs::write(&file_path, content)?;

    Ok(file_path.to_string_lossy().to_string())
}

/// Counts this user's submissions inside today's local calendar day (per
/// `competition.timezone_offset_minutes`). Reuses `get_user_submissions`
/// (exact match on user_id) rather than a SQL range query on the stringified
/// timestamp column, to avoid depending on lexicographic ordering of
/// `to_rfc3339()` output (whose fractional-second width varies) matching
/// chronological order.
///
/// Counts distinct submissions, not raw rows: in kaggle mode one `submit` stores
/// N candidate rows sharing one `batch_id`, and submissions are counted per
/// submission event, so that whole batch spends exactly one unit of quota. In
/// blind mode `batch_id` is always `None`, so this collapses to the row's
/// own id and behaves exactly as a plain row count.
fn count_submissions_today(user_id: i64, config: &BotConfig, db: &Database) -> Result<u32> {
    let now = Utc::now();
    let (day_start, day_end) = config.competition.local_day_bounds_utc(now);

    let submissions = db.get_user_submissions(user_id)?;

    let submission_keys: HashSet<String> = submissions
        .iter()
        .filter(|s| {
            DateTime::parse_from_rfc3339(&s.timestamp)
                .map(|ts| {
                    let ts = ts.with_timezone(&Utc);
                    ts >= day_start && ts < day_end
                })
                .unwrap_or(false)
        })
        .map(|s| {
            s.batch_id
                .clone()
                .unwrap_or_else(|| s.id.expect("stored submission has an id").to_string())
        })
        .collect();

    Ok(submission_keys.len() as u32)
}

fn calculate_checksum(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

fn read_csv_ids(content: &[u8]) -> Result<HashSet<i32>> {
    let mut reader = ReaderBuilder::new().has_headers(false).from_reader(content);

    let mut ids = HashSet::new();

    for result in reader.records() {
        let record = result?;
        if record.len() != 1 {
            anyhow::bail!("CSV must have exactly 1 column");
        }

        let id: i32 = record[0]
            .parse()
            .with_context(|| format!("Invalid ID: {}", &record[0]))?;
        ids.insert(id);
    }

    Ok(ids)
}

fn calculate_gain(
    predicted_ids: &HashSet<i32>,
    master_data: &MasterData,
    gain_matrix: &crate::config::GainMatrix,
) -> GainResult {
    calculate_gain_over(
        predicted_ids,
        master_data.all_ids(),
        master_data.positive_ids(),
        gain_matrix,
    )
}

/// Confusion matrix and gain restricted to `ids` -- the blind-mode path
/// (`calculate_gain`) passes the whole dataset; kaggle mode passes
/// `MasterData::public_ids`/`private_ids` to score one candidate CSV against
/// just one side of the split. `positive_ids` stays the full (unsplit) label
/// set either way -- ground truth doesn't change with the split.
fn calculate_gain_over(
    predicted_ids: &HashSet<i32>,
    ids: &HashSet<i32>,
    positive_ids: &HashSet<i32>,
    gain_matrix: &crate::config::GainMatrix,
) -> GainResult {
    let mut tp = 0;
    let mut tn = 0;
    let mut fp = 0;
    let mut fn_ = 0;

    for id in ids {
        let is_positive = positive_ids.contains(id);
        let predicted_positive = predicted_ids.contains(id);

        match (is_positive, predicted_positive) {
            (true, true) => tp += 1,
            (true, false) => fn_ += 1,
            (false, true) => fp += 1,
            (false, false) => tn += 1,
        }
    }

    let gain = (tp as f64) * gain_matrix.tp
        + (tn as f64) * gain_matrix.tn
        + (fp as f64) * gain_matrix.fp
        + (fn_ as f64) * gain_matrix.fn_;

    GainResult {
        gain,
        tp,
        tn,
        fp,
        fn_,
    }
}

fn get_threshold_category(gain: f64, config: &BotConfig) -> String {
    let mut thresholds = config.gain_thresholds.clone();
    thresholds.sort_by(|a, b| b.min_gain.partial_cmp(&a.min_gain).unwrap());

    for threshold in thresholds.iter() {
        if gain >= threshold.min_gain {
            return threshold.category.clone();
        }
    }

    thresholds.last().unwrap().category.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GainMatrix;
    use std::io::Write;

    /// Mirrors the competition config: only true positives pay, and every false
    /// positive costs 1.
    fn matrix() -> GainMatrix {
        GainMatrix {
            tp: 39.0,
            tn: 0.0,
            fp: -1.0,
            fn_: 0.0,
        }
    }

    fn test_config(submissions_path: &str, reveal_date: &str) -> BotConfig {
        serde_json::from_str(&format!(
            r#"{{
              "zulip": {{ "email": "bot@e.com", "api_key": "k", "site": "https://e.com" }},
              "database": {{ "path": "t.db" }},
              "logs": {{ "path": "logs" }},
              "teachers": [],
              "master_data": {{ "path": "m.csv" }},
              "submissions": {{ "path": "{submissions_path}" }},
              "roster": {{ "path": "roster.csv" }},
              "gain_matrix": {{ "tp": 39.0, "tn": 0.0, "fp": -1.0, "fn_": 0.0 }},
              "gain_thresholds": [
                {{ "min_gain": 0.0, "category": "low", "message": "m" }},
                {{ "min_gain": 100.0, "category": "mid", "message": "m" }},
                {{ "min_gain": 200.0, "category": "high", "message": "m" }}
              ],
              "competition": {{
                "name": "C", "description": "D",
                "deadline": "2025-12-31T23:59:59",
                "results_reveal_date": "{reveal_date}"
              }}
            }}"#
        ))
        .expect("test config parses")
    }

    fn master_from(rows: &str) -> (tempfile::NamedTempFile, MasterData) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "id,label\n{}", rows).unwrap();
        file.flush().unwrap();
        let master = MasterData::load(file.path().to_str().unwrap()).unwrap();
        (file, master)
    }

    fn ids(values: &[i32]) -> HashSet<i32> {
        values.iter().copied().collect()
    }

    fn db_with_submissions_at(user_id: i64, timestamps: &[String]) -> (tempfile::TempDir, Database) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();

        for (i, ts) in timestamps.iter().enumerate() {
            db.save_submission(&Submission {
                id: None,
                user_id,
                user_email: format!("u{}@e.com", user_id),
                user_full_name: "T".to_string(),
                submission_name: format!("s{}", i),
                timestamp: ts.clone(),
                file_checksum: format!("c{}", i),
                file_path: "/tmp/x.csv".to_string(),
                expected_gain: Some(1.0),
                actual_gain: 1.0,
                tp: 0,
                tn: 0,
                fp: 0,
                fn_: 0,
                positives_predicted: 0,
                threshold_category: "a".to_string(),
                after_deadline: false,
                used_golden_bullet: false,
                batch_id: None,
                public_gain: None,
                private_gain: None,
                is_baseline: false,
                baseline_published: false,
            })
            .unwrap();
        }

        (dir, db)
    }

    fn insert_valid_submission(
        db: &Database,
        user_id: i64,
        email: &str,
        full_name: &str,
        gain: f64,
        after_deadline: bool,
    ) {
        db.save_submission(&Submission {
            id: None,
            user_id,
            user_email: email.to_string(),
            user_full_name: full_name.to_string(),
            submission_name: "s".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            file_checksum: format!("c{}", user_id),
            file_path: "/tmp/x.csv".to_string(),
            expected_gain: Some(gain),
            actual_gain: gain,
            tp: 0,
            tn: 0,
            fp: 0,
            fn_: 0,
            positives_predicted: 0,
            threshold_category: "a".to_string(),
            after_deadline,
            used_golden_bullet: false,
            batch_id: None,
            public_gain: None,
            private_gain: None,
            is_baseline: false,
            baseline_published: false,
        })
        .unwrap();
    }

    fn competitor(email: &str, full_name: &str) -> Competitor {
        Competitor {
            email: email.to_string(),
            full_name: full_name.to_string(),
            daily_submit_limit: 5,
            golden_bullets: 0,
            max_files_per_submission: 1,
        }
    }

    // ---- Grade export ------------------------------------------------------

    #[test]
    fn grade_matches_the_median_anchored_linear_formula() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        // Gains: 10, 20, 30 -> median 20, max 30.
        insert_valid_submission(&db, 1, "a@e.com", "Ana", 10.0, false);
        insert_valid_submission(&db, 2, "b@e.com", "Beto", 20.0, false);
        insert_valid_submission(&db, 3, "c@e.com", "Caro", 30.0, false);

        let roster = crate::roster::from_competitors(vec![
            competitor("a@e.com", "Ana"),
            competitor("b@e.com", "Beto"),
            competitor("c@e.com", "Caro"),
        ]);
        let config = test_config("./s", "2026-01-01T23:59:59");

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let grade_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().grade;

        assert_eq!(grade_of("c@e.com"), 10.0, "at the max");
        assert_eq!(grade_of("b@e.com"), 8.0, "at the median");
        // 8 + 2*(10-20)/(30-20) = 6
        assert_eq!(grade_of("a@e.com"), 6.0, "below the median, still positive");
    }

    #[test]
    fn grade_is_floored_at_zero_not_negative() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        // Median 100, max 110 -> a gain far below the median would go
        // negative under the raw formula; the floor must catch it.
        insert_valid_submission(&db, 1, "a@e.com", "Ana", -400.0, false);
        insert_valid_submission(&db, 2, "b@e.com", "Beto", 100.0, false);
        insert_valid_submission(&db, 3, "c@e.com", "Caro", 110.0, false);

        let roster = crate::roster::from_competitors(vec![
            competitor("a@e.com", "Ana"),
            competitor("b@e.com", "Beto"),
            competitor("c@e.com", "Caro"),
        ]);
        let config = test_config("./s", "2026-01-01T23:59:59");

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let grade_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().grade;
        assert_eq!(grade_of("a@e.com"), 0.0);
    }

    #[test]
    fn competitors_with_no_valid_submission_get_grade_zero() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_valid_submission(&db, 1, "a@e.com", "Ana", 50.0, false);
        // Beto only has an after-deadline submission -- same as never
        // having submitted, as far as grading is concerned.
        insert_valid_submission(&db, 2, "b@e.com", "Beto", 999.0, true);

        let roster = crate::roster::from_competitors(vec![
            competitor("a@e.com", "Ana"),
            competitor("b@e.com", "Beto"),
            competitor("c@e.com", "Caro"), // never submitted at all
        ]);
        let config = test_config("./s", "2026-01-01T23:59:59");

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let grade_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().grade;
        assert_eq!(grade_of("b@e.com"), 0.0, "after-deadline-only submitter");
        assert_eq!(grade_of("c@e.com"), 0.0, "zero submissions");
        assert_eq!(grade_of("a@e.com"), 10.0, "sole valid submitter is both median and max");
    }

    #[test]
    fn teachers_are_excluded_from_the_median_and_max() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_valid_submission(&db, 1, "prof@e.com", "Prof", 9000.0, false);
        insert_valid_submission(&db, 2, "a@e.com", "Ana", 10.0, false);
        insert_valid_submission(&db, 3, "b@e.com", "Beto", 20.0, false);
        insert_valid_submission(&db, 4, "c@e.com", "Caro", 30.0, false);

        let roster = crate::roster::from_competitors(vec![
            competitor("a@e.com", "Ana"),
            competitor("b@e.com", "Beto"),
            competitor("c@e.com", "Caro"),
        ]);
        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.teachers = vec!["prof@e.com".to_string()];

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let grade_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().grade;
        // Without the exclusion, sorting in Prof's 9000 would push the
        // median to 25 and the max to 9000, changing every grade below.
        assert_eq!(grade_of("c@e.com"), 10.0, "max among competitors only");
        assert_eq!(grade_of("b@e.com"), 8.0, "median among competitors only");
    }

    #[test]
    fn grades_csv_has_the_expected_header_and_rows() {
        let rows = vec![
            GradeRow {
                email: "a@e.com".to_string(),
                full_name: "Ana".to_string(),
                gain: 12.3456789,
                grade: 9.5,
                expected_gain: Some(10.0),
                timestamp: Some("2025-01-01T00:00:00Z".to_string()),
                total_submissions: 3,
                max_gain: Some(20.0),
                used_golden_bullet: true,
            },
            GradeRow {
                email: "b@e.com".to_string(),
                full_name: "Beto".to_string(),
                gain: 0.0,
                grade: 0.0,
                expected_gain: None,
                timestamp: None,
                total_submissions: 0,
                max_gain: None,
                used_golden_bullet: false,
            },
        ];
        let csv = String::from_utf8(build_grades_csv(&rows).unwrap()).unwrap();
        assert_eq!(
            csv,
            "email,name,gain,expected_gain,submission_date,submissions,max,golden_bullet,grade\n\
             a@e.com,Ana,12.3456789,10,2025-01-01T00:00:00Z,3,20,yes,9.5\n\
             b@e.com,Beto,0,,,0,,no,0\n"
        );
    }

    // ---- Daily quota -------------------------------------------------------

    #[test]
    fn quota_window_is_start_inclusive_end_exclusive() {
        let config = test_config("./s", "2026-01-01T23:59:59");
        let (start, end) = config.competition.local_day_bounds_utc(Utc::now());

        let (_dir, db) = db_with_submissions_at(
            1,
            &[
                start.to_rfc3339(),                                    // at start: counts
                (start + chrono::Duration::hours(12)).to_rfc3339(),    // mid-day: counts
                (end - chrono::Duration::seconds(1)).to_rfc3339(),     // just before end: counts
                end.to_rfc3339(),                                      // at end: does not count
            ],
        );

        assert_eq!(count_submissions_today(1, &config, &db).unwrap(), 3);
    }

    #[test]
    fn quota_ignores_other_users_and_other_days() {
        let config = test_config("./s", "2026-01-01T23:59:59");
        let (start, _) = config.competition.local_day_bounds_utc(Utc::now());
        let yesterday = start - chrono::Duration::hours(1);

        let (_dir, db) = db_with_submissions_at(
            1,
            &[start.to_rfc3339(), yesterday.to_rfc3339()],
        );

        assert_eq!(
            count_submissions_today(1, &config, &db).unwrap(),
            1,
            "yesterday's submission must not count"
        );
        assert_eq!(
            count_submissions_today(2, &config, &db).unwrap(),
            0,
            "a user with no submissions at all"
        );
    }

    // ---- Kaggle mode -------------------------------------------------------

    fn insert_kaggle_candidate(
        db: &Database,
        user_id: i64,
        batch_id: &str,
        public_gain: f64,
        private_gain: f64,
        after_deadline: bool,
    ) {
        db.save_submission(&Submission {
            id: None,
            user_id,
            user_email: format!("u{}@e.com", user_id),
            user_full_name: "T".to_string(),
            submission_name: "s".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            file_checksum: format!("c-{}-{}", batch_id, public_gain),
            file_path: "/tmp/x.csv".to_string(),
            expected_gain: Some(1.0),
            actual_gain: private_gain,
            tp: 0,
            tn: 0,
            fp: 0,
            fn_: 0,
            positives_predicted: 0,
            threshold_category: "kaggle".to_string(),
            after_deadline,
            used_golden_bullet: false,
            batch_id: Some(batch_id.to_string()),
            public_gain: Some(public_gain),
            private_gain: Some(private_gain),
            is_baseline: false,
            baseline_published: false,
        })
        .unwrap();
    }

    #[test]
    fn mean_and_std_of_a_single_value_has_zero_spread() {
        let (mean, std) = mean_and_std(&[42.0]);
        assert_eq!(mean, 42.0);
        assert_eq!(std, 0.0);
    }

    #[test]
    fn mean_and_std_matches_hand_computed_values() {
        let (mean, std) = mean_and_std(&[1.0, 2.0, 3.0]);
        assert_eq!(mean, 2.0);
        assert!((std - (2.0_f64 / 3.0).sqrt()).abs() < 1e-9, "got std={}", std);
    }

    #[test]
    fn quota_collapses_a_kaggle_batchs_candidates_to_one_submission() {
        let config = test_config("./s", "2026-01-01T23:59:59");
        let (start, _) = config.competition.local_day_bounds_utc(Utc::now());
        let (_dir, db) = db_with_submissions_at(1, &[]);
        // Three candidate rows, one shared batch_id -- same call to
        // count_submissions_today used by the quota gate.
        for i in 0..3 {
            db.save_submission(&Submission {
                id: None,
                user_id: 1,
                user_email: "u1@e.com".to_string(),
                user_full_name: "T".to_string(),
                submission_name: "s".to_string(),
                timestamp: start.to_rfc3339(),
                file_checksum: format!("c{}", i),
                file_path: "/tmp/x.csv".to_string(),
                expected_gain: Some(1.0),
                actual_gain: 1.0,
                tp: 0,
                tn: 0,
                fp: 0,
                fn_: 0,
                positives_predicted: 0,
                threshold_category: "kaggle".to_string(),
                after_deadline: false,
                used_golden_bullet: false,
                batch_id: Some("batch-1".to_string()),
                public_gain: Some(1.0),
                private_gain: Some(1.0),
                is_baseline: false,
                baseline_published: false,
            })
            .unwrap();
        }

        assert_eq!(
            count_submissions_today(1, &config, &db).unwrap(),
            1,
            "3 candidate rows sharing one batch_id are a single submission"
        );
    }

    #[test]
    fn list_submits_never_shows_private_gain_in_kaggle_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_kaggle_candidate(&db, 1, "b1", 5.0, 999.0, false);

        // Reveal date already in the past -- in blind mode this is
        // exactly when actual_gain would start showing. Kaggle mode must
        // never show it, before or after.
        let mut config = test_config("./s", "2000-01-01T00:00:00");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let subs = db.get_user_submissions(1).unwrap();
        let listing = build_student_listing(&subs, &config, Utc::now()).unwrap();
        let response = listing.text.clone();
        let csv = String::from_utf8(listing.csv).unwrap();
        assert!(
            !csv.contains("999"),
            "private gain must never appear in the CSV either: {}",
            csv
        );
        assert!(response.contains("5.00"), "public gain must be shown: {}", response);
        assert!(
            !response.contains("999.00") && !response.contains("999.0"),
            "private gain must never appear in list submits: {}",
            response
        );
        assert!(response.contains("b1"), "batch id must be shown, to correlate with other commands: {}", response);
    }

    // ---- Student `list submits`: two-day window + full CSV ----------------

    /// 2026-10-10 12:00 in the competition's UTC-03:00.
    fn listing_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-10T15:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn listing_config(mode: crate::config::CompetitionMode) -> BotConfig {
        let mut config = test_config("./s", "2099-01-01T00:00:00");
        config.competition.mode = mode;
        config.competition.timezone_offset_minutes = -180;
        config
    }

    fn kaggle_row(name: &str, batch_id: &str, timestamp: &str, public: f64, private: f64) -> Submission {
        Submission {
            id: None,
            user_id: 1,
            user_email: "u1@e.com".to_string(),
            user_full_name: "T".to_string(),
            submission_name: name.to_string(),
            timestamp: timestamp.to_string(),
            file_checksum: "c".to_string(),
            file_path: "/tmp/x.csv".to_string(),
            expected_gain: None,
            actual_gain: private,
            tp: 0,
            tn: 0,
            fp: 0,
            fn_: 0,
            positives_predicted: 0,
            threshold_category: "kaggle".to_string(),
            after_deadline: false,
            used_golden_bullet: false,
            batch_id: Some(batch_id.to_string()),
            public_gain: Some(public),
            private_gain: Some(private),
            is_baseline: false,
            baseline_published: false,
        }
    }

    fn blind_row(id: i64, name: &str, timestamp: &str, expected: f64, actual: f64) -> Submission {
        let mut s = kaggle_row(name, "", timestamp, 0.0, 0.0);
        s.id = Some(id);
        s.batch_id = None;
        s.public_gain = None;
        s.private_gain = None;
        s.expected_gain = Some(expected);
        s.actual_gain = actual;
        s.threshold_category = "ok".to_string();
        s
    }

    fn csv_records(csv: &[u8]) -> (Vec<String>, Vec<Vec<String>>) {
        let mut reader = csv::Reader::from_reader(csv);
        let header = reader.headers().unwrap().iter().map(str::to_string).collect();
        let rows = reader
            .records()
            .map(|r| r.unwrap().iter().map(str::to_string).collect())
            .collect();
        (header, rows)
    }

    #[test]
    fn student_list_shows_two_local_days_and_the_csv_has_everything() {
        // Newest first, as the DB returns them. Local times are UTC-3.
        let subs = vec![
            kaggle_row("today", "b5", "2026-10-10T12:00:00+00:00", 1.0, 1.0),          // 10th 09:00
            kaggle_row("yest-late", "b4", "2026-10-10T02:59:59+00:00", 1.0, 1.0),      // 9th 23:59:59
            kaggle_row("yest-start", "b3", "2026-10-09T03:00:00+00:00", 1.0, 1.0),     // 9th 00:00:00
            kaggle_row("day-before", "b2", "2026-10-09T02:59:59+00:00", 1.0, 1.0),     // 8th 23:59:59
            kaggle_row("old", "b1", "2026-10-01T12:00:00+00:00", 1.0, 1.0),
        ];
        let config = listing_config(crate::config::CompetitionMode::Kaggle);
        let listing = build_student_listing(&subs, &config, listing_now()).unwrap();

        for shown in ["today", "yest-late", "yest-start"] {
            assert!(listing.text.contains(&format!("|{shown}|")), "{shown} is in the window: {}", listing.text);
        }
        for hidden in ["day-before", "old"] {
            assert!(!listing.text.contains(&format!("|{hidden}|")), "{hidden} is outside it: {}", listing.text);
        }
        // The window follows the competition's calendar, not UTC's: the 9th
        // 00:00 local is 03:00 UTC, so a submit at 02:59:59 UTC belongs to the 8th.
        assert!(listing.text.contains("since 2026-10-09"), "{}", listing.text);
        assert!(listing.text.contains("UTC-03:00"), "{}", listing.text);

        let (header, rows) = csv_records(&listing.csv);
        assert_eq!(header[0], "name");
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(names, ["today", "yest-late", "yest-start", "day-before", "old"], "the CSV is the full history, newest first");
        assert_eq!(listing.total, 5);
    }

    #[test]
    fn student_list_dates_are_in_competition_time() {
        let subs = vec![kaggle_row("m", "b1", "2026-10-10T02:30:15+00:00", 1.0, 1.0)];
        let config = listing_config(crate::config::CompetitionMode::Kaggle);
        let listing = build_student_listing(&subs, &config, listing_now()).unwrap();
        assert!(listing.text.contains("|2026-10-09 23:30|"), "table: {}", listing.text);
        let (_, rows) = csv_records(&listing.csv);
        assert_eq!(rows[0][1], "2026-10-09 23:30:15", "CSV: full seconds, same clock");
    }

    #[test]
    fn student_csv_is_plain_text_with_one_row_per_kaggle_batch() {
        let mut late = kaggle_row("model-b", "b2", "2026-10-10T11:00:00+00:00", 3.0, 0.0);
        late.after_deadline = true;
        let subs = vec![
            late,
            kaggle_row("model-a", "b1", "2026-10-10T10:00:00+00:00", 5.5, 8675309.5),
            kaggle_row("model-a", "b1", "2026-10-10T10:00:00+00:00", 7.25, 1.0),
        ];
        let config = listing_config(crate::config::CompetitionMode::Kaggle);
        let listing = build_student_listing(&subs, &config, listing_now()).unwrap();
        let csv = String::from_utf8(listing.csv.clone()).unwrap();

        assert!(csv.is_ascii(), "no emoji or symbols in the file: {csv}");
        assert!(!csv.contains('`') && !csv.contains('±') && !csv.contains('|'), "{csv}");
        assert!(!csv.contains("8675309"), "the private gain never reaches the file: {csv}");

        let (header, rows) = csv_records(&listing.csv);
        assert_eq!(header, ["name", "submitted_at", "candidates", "public_mean", "public_std", "batch_id", "on_time"]);
        assert_eq!(rows.len(), 2, "a 2-candidate submit is ONE row");
        assert_eq!(rows[0], ["model-b", "2026-10-10 08:00:00", "1", "3", "0", "b2", "no"]);
        // Full precision, not the table's 2 decimals: mean 6.375, population std 0.875.
        assert_eq!(rows[1], ["model-a", "2026-10-10 07:00:00", "2", "6.375", "0.875", "b1", "yes"]);
    }

    #[test]
    fn student_csv_round_trips_awkward_names() {
        let subs = vec![kaggle_row("a,b\"c", "b1", "2026-10-10T10:00:00+00:00", 1.0, 1.0)];
        let config = listing_config(crate::config::CompetitionMode::Kaggle);
        let listing = build_student_listing(&subs, &config, listing_now()).unwrap();
        let (_, rows) = csv_records(&listing.csv);
        assert_eq!(rows[0][0], "a,b\"c", "commas and quotes are escaped, not corrupting the row");
        assert_eq!(rows[0].len(), 7);
    }

    #[test]
    fn student_list_with_nothing_recent_says_so_and_still_has_the_csv() {
        let subs = vec![kaggle_row("old", "b1", "2026-09-01T10:00:00+00:00", 1.0, 1.0)];
        let config = listing_config(crate::config::CompetitionMode::Kaggle);
        let listing = build_student_listing(&subs, &config, listing_now()).unwrap();
        assert!(listing.text.contains("No submits in the last 2 days"), "{}", listing.text);
        assert!(!listing.text.contains("| Name |"), "no empty table: {}", listing.text);
        assert_eq!(csv_records(&listing.csv).1.len(), 1);
    }

    #[test]
    fn blind_list_hides_the_actual_gain_from_table_and_csv_until_the_reveal() {
        let subs = vec![blind_row(7, "m1", "2026-10-10T10:00:00+00:00", 100.0, 8675309.5)];

        let hidden_cfg = listing_config(crate::config::CompetitionMode::Blind);
        let hidden = build_student_listing(&subs, &hidden_cfg, listing_now()).unwrap();
        let csv = String::from_utf8(hidden.csv.clone()).unwrap();
        assert!(!hidden.text.contains("8675309") && !csv.contains("8675309"), "{}\n{}", hidden.text, csv);
        assert!(hidden.text.contains("will be revealed"), "{}", hidden.text);
        let (header, rows) = csv_records(&hidden.csv);
        assert_eq!(header, ["id", "name", "submitted_at", "expected_gain", "category", "on_time"]);
        assert_eq!(rows[0], ["7", "m1", "2026-10-10 07:00:00", "100", "ok", "yes"]);

        let mut revealed_cfg = listing_config(crate::config::CompetitionMode::Blind);
        revealed_cfg.competition.results_reveal_date = "2000-01-01T00:00:00".to_string();
        let revealed = build_student_listing(&subs, &revealed_cfg, listing_now()).unwrap();
        assert!(revealed.text.contains("✨ Actual"), "{}", revealed.text);
        let (header, rows) = csv_records(&revealed.csv);
        assert_eq!(header, ["id", "name", "submitted_at", "expected_gain", "actual_gain", "category", "on_time"]);
        assert_eq!(rows[0][4], "8675309.5");
        assert!(String::from_utf8(revealed.csv).unwrap().is_ascii());
    }

    #[test]
    fn student_list_message_stays_under_the_zulip_limit_however_many_recent_submits() {
        // 400 submits in the last two days: far more than fit in one message.
        let subs: Vec<Submission> = (0..400)
            .map(|i| {
                let ts = format!("2026-10-10T{:02}:{:02}:00+00:00", 12 - i / 60, 59 - i % 60);
                kaggle_row(&format!("a-fairly-long-model-name-{i}"), &format!("batch-{i:04}"), &ts, 1.0, 1.0)
            })
            .collect();
        let config = listing_config(crate::config::CompetitionMode::Kaggle);
        let listing = build_student_listing(&subs, &config, listing_now()).unwrap();

        assert!(
            listing.text.chars().count() <= LISTING_BUDGET,
            "{} chars, over the budget Zulip's 10,000 limit leaves",
            listing.text.chars().count()
        );
        let shown = listing.text.matches("|a-fairly-long-model-name-").count();
        assert!(shown > 0 && shown < 400, "some, not all: {shown}");
        assert!(
            listing.text.contains(&format!("…and {} more from these two days, only in the CSV.", 400 - shown)),
            "the hidden count is stated exactly: {}",
            listing.text
        );
        assert!(listing.text.contains("a-fairly-long-model-name-0|"), "the newest are the ones kept");
        assert_eq!(csv_records(&listing.csv).1.len(), 400, "the CSV has all of them");
    }

    #[test]
    fn user_submits_in_kaggle_mode_shows_one_row_per_batch() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        // b1: three candidates; best on public (9.0) has private 30.0, while
        // the highest PRIVATE (99.0) belongs to a weaker public candidate.
        insert_kaggle_candidate(&db, 1, "b1", 5.0, 20.0, false);
        insert_kaggle_candidate(&db, 1, "b1", 9.0, 30.0, false);
        insert_kaggle_candidate(&db, 1, "b1", 1.0, 99.0, false);
        insert_kaggle_candidate(&db, 1, "b2", 4.0, 7.0, true);

        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let table = process_user_submits("u1@e.com", SubmitOrder::default(), &db, &config);
        let rows: Vec<&str> = table.lines().filter(|l| l.contains("`b")).collect();
        assert_eq!(rows.len(), 2, "one row per batch, not per candidate: {table}");

        let b1 = rows.iter().find(|l| l.contains("`b1`")).unwrap();
        assert!(b1.contains("|3|"), "candidate count: {b1}");
        assert!(b1.contains("|9.00|30.00|"), "best public and ITS private gain: {b1}");
        assert!(!b1.contains("99.00"), "not the best private gain: {b1}");
        assert!(b1.contains("5.00 ± 3.27"), "public mean ± std: {b1}");
        assert!(b1.ends_with("✅|"), "{b1}");

        let b2 = rows.iter().find(|l| l.contains("`b2`")).unwrap();
        assert!(b2.contains("|1|") && b2.ends_with("⚠️|"), "late batch: {b2}");
    }

    #[test]
    fn mentions_with_any_name_characters_are_parsed() {
        let m = |s: &str| split_mention(s).map(|(name, _)| name);
        assert_eq!(m("user submits @**Ana Gómez**").as_deref(), Some("Ana Gómez"));
        assert_eq!(m("user submits @**test-student-1-bot**").as_deref(), Some("test-student-1-bot"));
        assert_eq!(m("user submits @**Diego R.**").as_deref(), Some("Diego R."));
        assert_eq!(m("user submits @**O'Brien**").as_deref(), Some("O'Brien"));
        assert_eq!(m("user submits @**Ana Gómez|42**").as_deref(), Some("Ana Gómez"), "drops Zulip's |id");
        assert_eq!(m("user submits @_**Ana**").as_deref(), Some("Ana"), "silent mention");
        assert_eq!(m("user submits Ana"), None);
        assert_eq!(m("user submits @****"), None);
        assert_eq!(
            split_mention("user submits @**Ana|42** mean asc").map(|(_, rest)| rest),
            Some(" mean asc"),
            "the text after the mention is returned for the sort options"
        );
    }

    #[test]
    fn submit_order_parsing() {
        use SubmitSortKey::*;
        let p = |s: &str| parse_submit_order(s);
        assert_eq!(p("").unwrap(), SubmitOrder::default());
        assert_eq!(p("").unwrap(), SubmitOrder { key: Date, descending: true }, "newest first by default");
        assert_eq!(p(" mean").unwrap(), SubmitOrder { key: Mean, descending: true }, "desc by default");
        assert_eq!(p(" max asc").unwrap(), SubmitOrder { key: Max, descending: false });
        assert_eq!(p(" ASC Gain").unwrap(), SubmitOrder { key: Gain, descending: false }, "any order, any case");
        assert_eq!(p(" desc").unwrap(), SubmitOrder { key: Date, descending: true });
        assert_eq!(p(" date asc").unwrap(), SubmitOrder { key: Date, descending: false });
        let err = p(" media").unwrap_err();
        assert!(err.contains("Unexpected `media`") && err.contains("Usage:"), "{err}");
        assert!(p(" mean max").is_err(), "two keys");
        assert!(p(" asc desc").is_err(), "two directions");
    }

    fn batch_order(table: &str) -> Vec<String> {
        table
            .lines()
            .filter_map(|l| l.split('`').nth(1).filter(|_| l.starts_with('|')))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn user_submits_sorts_kaggle_batches_by_each_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        // Inserted oldest -> newest (timestamps come from Utc::now()).
        //            best public  its private  public mean
        // b-old:        9.0          10.0         5.0
        // b-mid:        6.0          50.0         6.0
        // b-new:        8.0          30.0         4.0
        insert_kaggle_candidate(&db, 1, "b-old", 9.0, 10.0, false);
        insert_kaggle_candidate(&db, 1, "b-old", 1.0, 99.0, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        insert_kaggle_candidate(&db, 1, "b-mid", 6.0, 50.0, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        insert_kaggle_candidate(&db, 1, "b-new", 8.0, 30.0, false);
        insert_kaggle_candidate(&db, 1, "b-new", 0.0, 1.0, false);

        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;
        let sorted = |args: &str| {
            let order = parse_submit_order(args).unwrap();
            batch_order(&process_user_submits("u1@e.com", order, &db, &config))
        };

        assert_eq!(sorted(""), ["b-new", "b-mid", "b-old"], "default: newest first");
        assert_eq!(sorted(" date asc"), ["b-old", "b-mid", "b-new"]);
        assert_eq!(sorted(" gain"), ["b-mid", "b-new", "b-old"], "by the best-on-public candidate's private gain, not 99");
        assert_eq!(sorted(" gain asc"), ["b-old", "b-new", "b-mid"]);
        assert_eq!(sorted(" max"), ["b-old", "b-new", "b-mid"]);
        assert_eq!(sorted(" mean"), ["b-mid", "b-old", "b-new"]);
        assert_eq!(sorted(" mean asc"), ["b-new", "b-old", "b-mid"]);

        let table = process_user_submits("u1@e.com", parse_submit_order(" mean asc").unwrap(), &db, &config);
        assert!(table.contains("(sorted by mean, lowest first)"), "{table}");
    }

    #[test]
    fn user_submits_sorts_blind_submissions_by_gain() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_valid_submission(&db, 1, "a@e.com", "Ana", 20.0, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        insert_valid_submission(&db, 2, "a2@e.com", "Ana B", 10.0, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        insert_valid_submission(&db, 3, "a3@e.com", "Ana C", 30.0, false);

        let config = test_config("./s", "2026-01-01T23:59:59");
        let gains = |args: &str| -> Vec<String> {
            process_user_submits("Ana", parse_submit_order(args).unwrap(), &db, &config)
                .lines()
                .filter(|l| l.starts_with('|') && !l.starts_with("| ID") && !l.starts_with("|---"))
                .map(|l| l.split('|').nth(5).unwrap().to_string())
                .collect()
        };
        assert_eq!(gains(""), ["30.00", "10.00", "20.00"], "default: newest first");
        assert_eq!(gains(" gain"), ["30.00", "20.00", "10.00"]);
        assert_eq!(gains(" mean asc"), ["10.00", "20.00", "30.00"], "mean/max mean the single gain in blind mode");
    }

    #[test]
    fn user_submits_breaks_public_gain_ties_the_way_grading_does() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        // Tied on public gain. The leaderboard/grades SQL breaks the tie by
        // `id DESC`, so the later candidate (private 20.0) is the graded one.
        insert_kaggle_candidate(&db, 1, "tie", 5.0, 10.0, false);
        insert_kaggle_candidate(&db, 1, "tie", 5.0, 20.0, false);

        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let graded = db.get_leaderboard("gain", config.competition.mode).unwrap()[0].3;
        assert_eq!(graded, 20.0, "precondition: the grading rule picks the later candidate");

        let table = process_user_submits("u1@e.com", SubmitOrder::default(), &db, &config);
        assert!(table.contains("|5.00|20.00|"), "must show the graded candidate's private gain: {table}");
    }

    #[test]
    fn user_submits_in_blind_mode_still_lists_each_submission() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_valid_submission(&db, 1, "a@e.com", "Ana", 10.0, false);
        insert_valid_submission(&db, 2, "a2@e.com", "Ana B", 20.0, false);

        let config = test_config("./s", "2026-01-01T23:59:59");
        let table = process_user_submits("Ana", SubmitOrder::default(), &db, &config);
        assert!(table.contains("✨ Actual"), "blind keeps its own columns: {table}");
        assert_eq!(table.lines().filter(|l| l.starts_with('|') && !l.starts_with("| ID") && !l.starts_with("|---")).count(), 2);
    }

    #[test]
    fn compute_grades_under_kaggle_mode_uses_the_last_batchs_best_public_private_gain() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();

        // Ana's EARLIER batch has a much higher private gain -- it must not
        // count, since there is no `choose` step anymore: the LAST
        // pre-deadline batch always wins, mirroring blind mode's rule.
        insert_kaggle_candidate(&db, 1, "a-earlier", 100.0, 999.0, false);
        insert_kaggle_candidate(&db, 1, "a-last", 5.0, 20.0, false);
        insert_kaggle_candidate(&db, 1, "a-last", 9.0, 30.0, false);

        insert_kaggle_candidate(&db, 2, "b-last", 3.0, 10.0, false);

        let roster = crate::roster::from_competitors(vec![
            competitor("u1@e.com", "Ana"),
            competitor("u2@e.com", "Beto"),
        ]);
        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let gain_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().gain;

        assert_eq!(
            gain_of("u1@e.com"),
            30.0,
            "must use the LAST batch's best-on-public candidate, not the earlier higher-private batch"
        );
        assert_eq!(gain_of("u2@e.com"), 10.0);
    }

    #[test]
    fn compute_grades_under_kaggle_mode_grades_everyone_who_submitted() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();

        // A single submission -- there is no `choose` step to skip anymore.
        insert_kaggle_candidate(&db, 1, "only-batch", 5.0, 42.0, false);

        let roster = crate::roster::from_competitors(vec![competitor("u1@e.com", "Ana")]);
        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let gain_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().gain;

        assert_eq!(
            gain_of("u1@e.com"),
            42.0,
            "a competitor who submitted must be graded on their best-on-public candidate, \
             with no separate `choose` step required"
        );
    }

    // ---- Baselines -------------------------------------------------------

    fn insert_baseline_candidate(db: &Database, name: &str, batch_id: &str, public_gain: f64, private_gain: f64) {
        db.save_submission(&Submission {
            id: None,
            user_id: 99,
            user_email: "prof@e.com".to_string(),
            user_full_name: "Prof".to_string(),
            submission_name: name.to_string(),
            timestamp: Utc::now().to_rfc3339(),
            file_checksum: format!("b-{}-{}", batch_id, public_gain),
            file_path: "/tmp/x.csv".to_string(),
            expected_gain: None,
            actual_gain: private_gain,
            tp: 0,
            tn: 0,
            fp: 0,
            fn_: 0,
            positives_predicted: 0,
            threshold_category: "kaggle".to_string(),
            after_deadline: false,
            used_golden_bullet: false,
            batch_id: Some(batch_id.to_string()),
            public_gain: Some(public_gain),
            private_gain: Some(private_gain),
            is_baseline: true,
            baseline_published: false,
        })
        .unwrap();
    }

    #[test]
    fn baseline_command_parsing() {
        use BaselineCommand::*;
        assert_eq!(parse_baseline_command(" logistic\n\n[a.csv](/u/a.csv)"), Upload("logistic"));
        assert_eq!(parse_baseline_command(" LogReg-v2 [a.csv](/u)"), Upload("LogReg-v2"), "name keeps its case");
        assert_eq!(parse_baseline_command(" list"), List);
        assert_eq!(parse_baseline_command(" LIST"), List, "subcommands are case-insensitive");
        assert_eq!(parse_baseline_command(" publish 99-123"), Publish("99-123"));
        assert_eq!(parse_baseline_command(" hide 99-123"), Hide("99-123"));
        assert_eq!(parse_baseline_command(""), Usage, "no name at all");
        assert_eq!(parse_baseline_command(" [a.csv](/u/a.csv)"), Usage, "attachment but no name");
        assert_eq!(parse_baseline_command(" publish"), Usage, "publish needs an id");
        assert_eq!(parse_baseline_command(" hide a b"), Usage);
        assert_eq!(parse_baseline_command(" list extra"), Usage);
    }

    #[test]
    fn baselines_never_affect_grades_even_if_the_teacher_is_on_the_roster() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_kaggle_candidate(&db, 1, "a", 5.0, 10.0, false);
        insert_kaggle_candidate(&db, 2, "b", 5.0, 20.0, false);
        insert_kaggle_candidate(&db, 3, "c", 5.0, 30.0, false);
        // A baseline far above everyone: counted, it would become the max and
        // drag every competitor's grade down.
        insert_baseline_candidate(&db, "oracle", "t1", 999.0, 9000.0);

        let roster = crate::roster::from_competitors(vec![
            competitor("u1@e.com", "Ana"),
            competitor("u2@e.com", "Beto"),
            competitor("u3@e.com", "Caro"),
            competitor("prof@e.com", "Prof"),
        ]);
        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let rows = compute_grades(&db, &roster, &config).unwrap();
        let grade_of = |email: &str| rows.iter().find(|r| r.email == email).unwrap().grade;
        assert_eq!(grade_of("u3@e.com"), 10.0, "the top COMPETITOR still scores 10");
        assert_eq!(grade_of("u2@e.com"), 8.0, "the competitors' median still scores 8");
        assert_eq!(grade_of("prof@e.com"), 0.0, "a baseline is never a valid entry");
    }

    #[test]
    fn private_leaderboard_interleaves_baselines_without_a_position_number() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
        db.init().unwrap();
        insert_kaggle_candidate(&db, 1, "a", 5.0, 30.0, false);
        insert_kaggle_candidate(&db, 2, "b", 5.0, 10.0, false);
        insert_baseline_candidate(&db, "logistic", "t1", 50.0, 20.0);
        insert_baseline_candidate(&db, "random", "t2", 1.0, 1.0);
        db.set_baseline_published("t1", true).unwrap();

        let mut config = test_config("./s", "2026-01-01T23:59:59");
        config.competition.mode = crate::config::CompetitionMode::Kaggle;

        let table = process_leaderboard_full(&db, &config, "gain");
        let lines: Vec<&str> = table.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| Pos")).collect();
        assert_eq!(lines.len(), 4, "{table}");
        assert!(lines[0].starts_with("| 1 |") && lines[0].contains("30.00"), "{table}");
        assert!(lines[1].starts_with("| — |") && lines[1].contains("Baseline: logistic"), "{table}");
        assert!(lines[1].contains("shown") && lines[1].contains("20.00"), "{table}");
        assert!(lines[2].starts_with("| 2 |") && lines[2].contains("10.00"), "the second competitor is still #2: {table}");
        assert!(lines[3].contains("Baseline: random") && lines[3].contains("hidden"), "{table}");
    }

    // ---- CSV parsing -----------------------------------------------------

    #[test]
    fn csv_ids_parse_a_single_column_without_header() {
        assert_eq!(read_csv_ids(b"123\n456\n789\n").unwrap(), ids(&[123, 456, 789]));
    }

    #[test]
    fn csv_ids_collapse_duplicates() {
        // A student who submits the same id twice does not get counted twice.
        assert_eq!(read_csv_ids(b"1\n1\n2\n").unwrap(), ids(&[1, 2]));
    }

    #[test]
    fn csv_ids_reject_malformed_input() {
        assert!(read_csv_ids(b"1,2\n").is_err(), "two columns");
        assert!(read_csv_ids(b"abc\n").is_err(), "not an integer");
        // A header is not tolerated, because "id" does not parse as i32.
        assert!(read_csv_ids(b"id\n1\n").is_err(), "header row");
    }

    // ---- Confusion matrix and gain ---------------------------------------

    #[test]
    fn confusion_matrix_partitions_the_master_data() {
        let (_f, master) = master_from("1,1\n2,1\n3,0\n4,0\n5,0\n");
        let result = calculate_gain(&ids(&[1, 3]), &master, &matrix());

        assert_eq!(result.tp, 1, "id 1: positive and predicted");
        assert_eq!(result.fn_, 1, "id 2: positive, not predicted");
        assert_eq!(result.fp, 1, "id 3: negative, predicted");
        assert_eq!(result.tn, 2, "ids 4 and 5");
        assert_eq!(
            result.tp + result.tn + result.fp + result.fn_,
            5,
            "every master id lands in exactly one cell"
        );
        assert_eq!(result.gain, 39.0 - 1.0);
    }

    #[test]
    fn gain_ignores_predicted_ids_outside_the_master_data() {
        // process_submit rejects these upstream, but the scorer must not credit
        // them if that check is ever relaxed.
        let (_f, master) = master_from("1,1\n2,0\n");
        let result = calculate_gain(&ids(&[1, 999]), &master, &matrix());
        assert_eq!(result.tp, 1);
        assert_eq!(result.fp, 0);
        assert_eq!(result.tp + result.tn + result.fp + result.fn_, 2);
    }

    #[test]
    fn labels_other_than_one_count_as_negative() {
        let (_f, master) = master_from("1,1\n2,0\n3,7\n");
        assert_eq!(master.positive_count(), 1);
        assert_eq!(master.total_count(), 3);
    }

    /// Closed-form checks, derived from the definition of gain rather than from
    /// the implementation. These are what catch a swapped or sign-flipped
    /// matrix entry -- the failure mode that produces plausible wrong grades.
    fn assert_gain_invariants(master: &MasterData) {
        let m = matrix();
        let n_pos = master.positive_count() as f64;
        let n_neg = (master.total_count() - master.positive_count()) as f64;

        let nothing = calculate_gain(&HashSet::new(), master, &m);
        assert_eq!(
            nothing.gain,
            n_neg * m.tn + n_pos * m.fn_,
            "predicting nothing: every positive is a FN, every negative a TN"
        );

        let everything = calculate_gain(master.all_ids(), master, &m);
        assert_eq!(
            everything.gain,
            n_pos * m.tp + n_neg * m.fp,
            "predicting everything: every positive is a TP, every negative a FP"
        );

        let perfect = calculate_gain(master.positive_ids(), master, &m);
        assert_eq!(
            perfect.gain,
            n_pos * m.tp + n_neg * m.tn,
            "perfect model: no FP, no FN"
        );
        assert_eq!(perfect.fp, 0);
        assert_eq!(perfect.fn_, 0);
    }

    #[test]
    fn gain_invariants_hold_on_synthetic_data() {
        let (_f, master) = master_from("1,1\n2,1\n3,0\n4,0\n5,0\n6,0\n");
        assert_gain_invariants(&master);
    }

    /// Same invariants against the competition dataset, which is gitignored --
    /// so this skips rather than fails when the file is absent (e.g. in CI).
    #[test]
    fn gain_invariants_hold_on_the_real_dataset() {
        const PATH: &str = "master_data.csv";
        if !std::path::Path::new(PATH).exists() {
            eprintln!("skipping: {} not present", PATH);
            return;
        }
        let master = MasterData::load(PATH).unwrap();
        assert!(master.positive_count() > 0, "dataset has positives");
        assert!(
            master.positive_count() < master.total_count(),
            "dataset has negatives"
        );
        assert_gain_invariants(&master);
    }

    // ---- Thresholds ------------------------------------------------------

    #[test]
    fn threshold_picks_the_highest_bracket_at_or_below_the_gain() {
        let config = test_config("./s", "2026-01-01T23:59:59");

        assert_eq!(get_threshold_category(250.0, &config), "high");
        assert_eq!(get_threshold_category(200.0, &config), "high", "exact boundary");
        assert_eq!(get_threshold_category(199.9, &config), "mid");
        assert_eq!(get_threshold_category(100.0, &config), "mid", "exact boundary");
        assert_eq!(get_threshold_category(0.0, &config), "low");
    }

    #[test]
    fn threshold_below_every_bracket_still_names_one() {
        // process_submit unwraps the looked-up category, so returning something
        // absent from the config would panic.
        let config = test_config("./s", "2026-01-01T23:59:59");
        let category = get_threshold_category(-500.0, &config);
        assert!(
            config.gain_thresholds.iter().any(|t| t.category == category),
            "category {:?} must exist in the config",
            category
        );
    }

    // ---- Checksums and file layout ---------------------------------------

    #[test]
    fn checksums_are_stable_and_content_dependent() {
        // Duplicate detection is only meaningful if both halves hold.
        assert_eq!(calculate_checksum(b"1\n2\n"), calculate_checksum(b"1\n2\n"));
        assert_ne!(calculate_checksum(b"1\n2\n"), calculate_checksum(b"1\n3\n"));
        assert_eq!(calculate_checksum(b"x").len(), 64, "sha-256 as hex");
    }

    #[test]
    fn submission_files_are_routed_by_role_and_sanitized() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path().to_str().unwrap(), "2026-01-01T23:59:59");

        let student = save_submission_file(
            "Ana/../Gómez",
            "My Model #1 (test)",
            "pred.csv",
            b"1\n2\n",
            false,
            &config,
        )
        .unwrap();

        assert!(student.contains("/students/"), "got {}", student);
        assert!(!student.contains(".."), "path traversal stripped: {}", student);
        assert!(!student.contains('#'), "unsafe chars stripped: {}", student);
        assert_eq!(std::fs::read(&student).unwrap(), b"1\n2\n");

        let teacher =
            save_submission_file("Prof", "m", "pred.csv", b"1\n", true, &config).unwrap();
        assert!(teacher.contains("/teachers/"), "got {}", teacher);
    }

    // ---- Result reveal gate ----------------------------------------------

    #[test]
    fn results_stay_hidden_until_the_reveal_date() {
        let future = test_config("./s", "2099-01-01T00:00:00");
        assert!(!results_revealed(&future), "before the reveal date");

        let past = test_config("./s", "2000-01-01T00:00:00");
        assert!(results_revealed(&past), "after the reveal date");
    }

    #[test]
    fn an_unparseable_reveal_date_hides_results() {
        // Fail safe: a misconfigured date must not leak results. Startup
        // validation rejects this config, so this is the belt to that braces.
        let broken = test_config("./s", "nonsense");
        assert!(!results_revealed(&broken));
    }
}
