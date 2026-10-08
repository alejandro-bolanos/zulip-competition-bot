use zulip_competition_bot::{config, database, master_data, models, public_board, roster, submission, zulip};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::FutureExt;
use tracing::{error, info, warn};

use chrono::{Local, Utc};
use std::cell::RefCell;
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use config::{BotConfig, CompetitionMode};
use submission::BaselineCommand;
use database::Database;
use master_data::MasterData;
use roster::Roster;
use zulip::ZulipClient;


#[derive(Parser)]
#[command(name = "zulip-competition-bot")]
#[command(about = "Zulip bot for Kaggle-style competitions", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to config file
    #[arg(short, long, value_name = "FILE")]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a config template
    CreateConfig,
    /// Run the bot
    Run {
        /// Config file path
        #[arg(short, long)]
        config: String,
    },
}

/// Sets up console logging, plus a daily file inside `log_dir` when given.
///
/// The log directory comes from the config, so this runs after the config is
/// loaded. `None` is for commands that have no config to read yet.
fn init_logging(log_dir: Option<&Path>) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Console layer (stdout) - colorful and concise
    let console_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .compact();

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer);

    let Some(log_dir) = log_dir else {
        registry.init();
        return Ok(());
    };

    fs::create_dir_all(log_dir)
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let log_file = log_dir.join(format!(
        "zulip_competition_bot_{}.log",
        Local::now().format("%Y%m%d")
    ));

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .with_context(|| format!("Failed to open log file: {}", log_file.display()))?;

    // File layer - detailed with timestamps
    let file_layer = fmt::layer()
        .with_writer(std::sync::Arc::new(file))
        .with_target(true)
        .with_ansi(false)
        .with_line_number(true)
        .with_thread_ids(true);

    registry.with(file_layer).init();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::CreateConfig) => {
            // Console only: there is no config yet to tell us where logs go.
            init_logging(None)?;
            config::create_config_template()?;
            info!("Config template created successfully at config.json");
            Ok(())
        }
        Some(Commands::Run { config }) => run_bot(&config).await,
        None => {
            if let Some(config_path) = cli.config {
                run_bot(&config_path).await
            } else {
                eprintln!("Please specify a config file or use --help");
                Ok(())
            }
        }
    }
}

async fn run_bot(config_path: &str) -> Result<()> {
    // Loaded before logging exists, because logs.path lives in it. A bad config
    // therefore reports on stderr -- the best available behaviour, since we do
    // not yet know where log files belong.
    let config = BotConfig::load(config_path)?;

    init_logging(Some(Path::new(&config.logs.path)))?;

    info!(
        "Starting zulip-competition-bot with config: {}",
        config_path
    );
    info!("Logging to {}", config.logs.path);

    // Initialize database
    let db = Database::new(&config.database.path)?;
    db.init()?;
    info!("Database initialized at: {}", config.database.path);

    // Load master data
    let master_data = MasterData::load(&config.master_data.path)?;
    info!(
        "Master data loaded: {} records, {} positives",
        master_data.total_count(),
        master_data.positive_count()
    );

    // Kaggle mode ranks and grades off the public/private split; without it
    // there is nothing to score `submit` against, so this refuses to boot
    // rather than silently treating every id as private.
    if config.competition.mode == config::CompetitionMode::Kaggle && !master_data.has_split() {
        anyhow::bail!(
            "competition.mode is 'kaggle' but {} has no 'split' column (expected id,label,split)",
            config.master_data.path
        );
    }

    // Load the roster. Required, not optional: it is the sole authorization
    // list for students, so a missing or malformed file refuses to boot the
    // same way a missing master_data.csv already does.
    let roster = Roster::load(&config.roster.path)?;
    info!("Roster loaded: {} competitors", roster.len());

    // Both dates are already validated as parseable by BotConfig::load, but
    // the resolved UTC instants depend on timezone_offset_minutes -- log them
    // so a misconfigured offset is visible at a glance, not discovered later.
    if let (Ok(deadline), Ok(reveal)) = (
        config.competition.deadline_utc(),
        config.competition.results_reveal_utc(),
    ) {
        info!(
            "Deadline resolves to {} UTC, reveal to {} UTC (offset: {} min)",
            deadline.to_rfc3339(),
            reveal.to_rfc3339(),
            config.competition.timezone_offset_minutes
        );
    }

    // Create Zulip client
    let client = ZulipClient::new(
        config.zulip.email.clone(),
        config.zulip.api_key.clone(),
        config.zulip.site.clone(),
    );

    info!("Competition: {}", config.competition.name);
    info!("Deadline: {}", config.competition.deadline);
    info!("Teachers: {}", config.teachers.len());
    info!("Bot ready! Listening for private messages...");

    // Start message loop
    let bot = Bot {
        config,
        client,
        db,
        master_data,
        roster: RefCell::new(roster),
    };

    let mut bot = bot;
    bot.run().await?;

    Ok(())
}

struct Bot {
    config: BotConfig,
    client: ZulipClient,
    db: Database,
    master_data: MasterData,
    /// `RefCell`, not a lock: `run()` awaits `handle_message_isolated` fully
    /// before moving to the next event (no spawning anywhere in this crate),
    /// so at most one borrow is ever live at a time. `roster reload` is the
    /// only writer.
    roster: RefCell<Roster>,
}

impl Bot {
    async fn run(&mut self) -> Result<()> {
        loop {
            // The client owns queue_id and last_event_id together, so a dropped
            // queue re-anchors both instead of reusing a stale event id.
            let events = match self.client.get_events().await {
                Ok(events) => events,
                Err(e) => {
                    error!("Error fetching events: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            for event in events {
                if event.event_type != "message" {
                    continue;
                }

                let Some(message) = event.message else {
                    continue;
                };

                if message.msg_type != "private" || message.sender_email == self.config.zulip.email
                {
                    continue;
                }

                self.handle_message_isolated(message).await;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    /// Contains a panic to the message that caused it. Without this, one
    /// malformed message takes the whole bot down.
    async fn handle_message_isolated(&self, message: models::Message) {
        let sender_email = message.sender_email.clone();

        if AssertUnwindSafe(self.handle_message(message))
            .catch_unwind()
            .await
            .is_err()
        {
            error!("Panic while handling message from {}", sender_email);

            // The handler panicked before replying, so the sender is still waiting.
            if let Err(e) = self
                .client
                .send_message(
                    &sender_email,
                    "❌ Internal error processing your message. Please tell a teacher.",
                )
                .await
            {
                error!("Could not report the failure to {}: {}", sender_email, e);
            }
        }
    }

    async fn handle_message(&self, message: models::Message) {
        let sender_email = message.sender_email.clone();
        let content = message.content.trim().to_lowercase();

        // Truncate by chars, not bytes: byte slicing splits multi-byte
        // characters (accents, emoji) and panics.
        let preview: String = content.chars().take(50).collect();
        let ellipsis = if content.chars().count() > 50 {
            "..."
        } else {
            ""
        };
        info!("Message from {}: {}{}", sender_email, preview, ellipsis);

        let is_teacher = self.config.teachers.contains(&sender_email);
        let is_competitor = self.roster.borrow().is_enabled(&sender_email);

        // The roster is the sole authorization list for students. Anyone on
        // neither list gets exactly one banner and nothing else -- no help
        // text, no command list, no hint about what exists.
        if !is_teacher && !is_competitor {
            info!("Rejecting sender outside teachers/roster: {}", sender_email);
            self.reply(
                &sender_email,
                "🚫 You are not authorized to use this bot. If you think you should be, please tell a teacher.",
            )
            .await;
            return;
        }

        info!("User is teacher: {}", is_teacher);

        let mode = self.config.competition.mode;

        let response = if content.starts_with("reveal ") && mode == CompetitionMode::Kaggle {
            // Golden bullets reveal a single hidden gain; kaggle mode has no
            // such thing (public gain is already visible on every submit,
            // and private gain is exactly the value a bullet would leak
            // early) -- rejected before the is_teacher/is_competitor split,
            // since neither role can use it here.
            "❌ The `reveal` command is not available in kaggle mode.".to_string()
        } else if (content.starts_with("submit ") || content.starts_with("reveal ")) && !is_teacher
        {
            let use_golden_bullet = content.starts_with("reveal ");
            info!(
                "Processing submit command (student, golden bullet: {})",
                use_golden_bullet
            );
            // Cloned out of the RefCell borrow (a couple of Strings + two
            // u32s) so the guard drops here, before the .await below --
            // holding a RefCell Ref across an await point makes the future
            // non-Send, which would break the moment anything spawns message
            // handling onto another task. The lookup cannot miss: the gate
            // above already proved is_competitor for this exact message.
            let competitor = self.roster.borrow().get(&sender_email).cloned();
            match competitor {
                Some(competitor) => match mode {
                    CompetitionMode::Blind => {
                        submission::process_submit(
                            &message,
                            &self.config,
                            &self.db,
                            &self.master_data,
                            &competitor,
                            is_teacher,
                            use_golden_bullet,
                        )
                        .await
                    }
                    CompetitionMode::Kaggle => {
                        submission::process_kaggle_submit(
                            &message,
                            &self.config,
                            &self.db,
                            &self.master_data,
                            &competitor,
                        )
                        .await
                    }
                },
                None => {
                    warn!("Submit from {} but no matching roster entry", sender_email);
                    "❌ You are not on the roster for this competition. Please tell a teacher."
                        .to_string()
                }
            }
        } else if (content.starts_with("submit ") || content.starts_with("reveal ")) && is_teacher {
            info!("Submit command blocked for teacher");
            match mode {
                CompetitionMode::Kaggle => "⚠️ Teachers cannot submit models. To upload a reference model, use \
                    `baseline <name>` and attach the CSVs -- it's never ranked or graded."
                    .to_string(),
                CompetitionMode::Blind => {
                    "⚠️ Teachers cannot submit models. Use the administration commands instead.".to_string()
                }
            }
        } else if content == "list submits" && !is_teacher {
            info!("Processing list submits command");
            submission::process_list_submits(message.sender_id, &self.db, &self.config)
        } else if content == "duplicates" && is_teacher {
            info!("Processing duplicates command (teacher)");
            submission::process_duplicates(&self.db)
        } else if content.split_whitespace().next() == Some("baseline") && is_teacher {
            info!("Processing baseline command (teacher)");
            if mode != CompetitionMode::Kaggle {
                "❌ `baseline` is only available in kaggle mode.".to_string()
            } else {
                // Re-parsed from the original text so the name and id keep
                // their case; "baseline" itself is the same length either way.
                let args = &message.content.trim()["baseline".len()..];
                match submission::parse_baseline_command(args) {
                    BaselineCommand::Upload(name) => {
                        submission::process_baseline_upload(
                            &message,
                            &self.config,
                            &self.db,
                            &self.master_data,
                            name,
                        )
                        .await
                    }
                    BaselineCommand::List => submission::process_baseline_list(&self.db),
                    BaselineCommand::Publish(id) => {
                        submission::process_baseline_visibility(&self.db, id, true)
                    }
                    BaselineCommand::Hide(id) => {
                        submission::process_baseline_visibility(&self.db, id, false)
                    }
                    BaselineCommand::Usage => submission::baseline_usage(),
                }
            }
        } else if content.starts_with("public leaderboard") && is_teacher {
            info!("Processing public leaderboard command (teacher)");
            if mode != CompetitionMode::Kaggle {
                "❌ `public leaderboard` is only available in kaggle mode -- blind mode has no public gain to show."
                    .to_string()
            } else {
                let args = message.content.trim()["public leaderboard".len()..].trim();
                match public_board::parse_options(args) {
                    Ok(opts) => self.generate_public_leaderboard(&opts).await,
                    Err(usage) => usage,
                }
            }
        } else if content.starts_with("leaderboard") && is_teacher {
            info!("Processing leaderboard command (teacher)");
            let parts: Vec<&str> = message.content.split_whitespace().collect();
            let order_by = if parts.len() >= 2 {
                match parts[1].to_lowercase().as_str() {
                    "datetime" => "datetime",
                    "gain" => "gain",
                    _ => "gain", // default to gain for invalid options
                }
            } else {
                "gain" // default to gain
            };
            submission::process_leaderboard_full(&self.db, &self.config, order_by)
        } else if content == "all submits" && is_teacher {
            info!("Processing all submits command (teacher)");
            submission::export_all_submissions_csv(&self.db, &self.client).await
        } else if content == "no submits" && is_teacher {
            info!("Processing no submits command (teacher)");
            submission::process_no_submits(&self.db, &self.roster.borrow())
        } else if content.starts_with("user submits") && is_teacher {
            info!("Processing user submits command (teacher)");
            if let Some(user_name) = submission::mentioned_user_name(&message.content) {
                submission::process_user_submits(&user_name, &self.db, &self.config)
            } else {
                "❌ Usage: user submits @user (use a Zulip mention)".to_string()
            }
        } else if content == "roster reload" && is_teacher {
            info!("Processing roster reload command (teacher)");
            self.reload_roster()
        } else if content == "grades" && is_teacher {
            info!("Processing grades command (teacher)");
            // Computed synchronously so the roster borrow drops here, before
            // the upload's .await -- same reasoning as the submit arm above.
            let grades = {
                let roster = self.roster.borrow();
                submission::compute_grades(&self.db, &roster, &self.config)
            };
            match grades {
                Ok(rows) => submission::export_grades_csv(&rows, &self.client).await,
                Err(e) => format!("❌ Error computing grades: {}", e),
            }
        } else if content == "help" {
            info!("Processing help command");
            self.get_help_message(is_teacher)
        } else {
            info!("Unrecognized command: {}", preview);
            format!(
                "❓ I don't recognize that command.\n\n{}",
                self.get_help_message(is_teacher)
            )
        };

        self.reply(&sender_email, &response).await;
    }

    async fn reply(&self, to: &str, content: &str) {
        info!("Response generated, length: {} chars", content.len());
        match self.client.send_message(to, content).await {
            Ok(_) => info!("✅ Response sent successfully to {}", to),
            Err(e) => error!("❌ Error sending message to {}: {}", to, e),
        }
    }

    /// Reloads the roster from disk. On a parse error, the previous roster
    /// stays in effect -- a half-edited CSV must not lock everyone out.
    fn reload_roster(&self) -> String {
        let path = &self.config.roster.path;
        match Roster::load(path) {
            Ok(new_roster) => {
                let old_count = self.roster.borrow().len();
                let new_count = new_roster.len();
                *self.roster.borrow_mut() = new_roster;
                info!(
                    "Roster reloaded: {} -> {} competitors",
                    old_count, new_count
                );
                format!(
                    "✅ Roster reloaded: {} competitors (before: {}).",
                    new_count, old_count
                )
            }
            Err(e) => {
                warn!("Roster reload failed, keeping the previous roster: {:#}", e);
                format!(
                    "❌ Error reloading the roster, keeping the previous one: {:#}",
                    e
                )
            }
        }
    }

    /// Builds and uploads the public leaderboard PNG. Queries only
    /// `Database::get_public_candidates` -- never the private leaderboard --
    /// so this can never leak a private gain or an email; see
    /// `public_board`'s module doc comment for why that matters. Passing the
    /// roster also means a competitor removed from it (or never on it) can
    /// never appear on the image, even if old rows for them still sit in the
    /// database.
    async fn generate_public_leaderboard(&self, opts: &public_board::BoardOptions) -> String {
        let candidates = match self.db.get_public_candidates(&self.roster.borrow()) {
            Ok(c) => c,
            Err(e) => return format!("❌ Error retrieving public gains: {}", e),
        };

        let baselines = match self.db.get_public_baselines() {
            Ok(b) => b,
            Err(e) => return format!("❌ Error retrieving baselines: {}", e),
        };

        let rows = public_board::build_rows(&candidates, &baselines, opts);
        if rows.is_empty() {
            return "📊 No pre-deadline kaggle submissions yet -- nothing to show.".to_string();
        }

        let header = public_board::BoardHeader {
            title: self.config.competition.name.clone(),
            generated_at: public_board::generated_at_label(
                Utc::now(),
                self.config.competition.timezone_offset_minutes,
            ),
        };
        let svg = public_board::render_svg(&rows, opts, &header);
        let png = match public_board::rasterize_png(&svg) {
            Ok(bytes) => bytes,
            Err(e) => return format!("❌ Error rendering the leaderboard image: {:#}", e),
        };

        let filename = format!("public_leaderboard_{}.png", Utc::now().format("%Y%m%d_%H%M%S"));
        match self.client.upload_file(&filename, png, "image/png").await {
            Ok(url) => format!(
                "📊 **Public leaderboard** ({})\n\n[{}]({})",
                row_counts(&rows),
                filename,
                url
            ),
            Err(e) => format!("❌ Error uploading the image to Zulip: {}", e),
        }
    }

    fn get_help_message(&self, is_teacher: bool) -> String {
        let comp = &self.config.competition;
        let mode = comp.mode;

        if is_teacher {
            let mode_notes = match mode {
                CompetitionMode::Blind => {
                    "- The leaderboard and grades use each competitor's LAST on-time submission, not their best.\n\
                     - 🌟 on the leaderboard marks that the ranking submission (the last one on time) was made with `reveal`."
                }
                CompetitionMode::Kaggle => {
                    "- Kaggle mode: the leaderboard and grades use the PRIVATE gain of the candidate with the best \
                     public gain, within each competitor's LAST pre-deadline submission -- there is no separate \
                     pick step, the same \"last submission wins\" rule as blind mode.\n\
                     - `reveal` is not available in this mode."
                }
            };
            let public_leaderboard_bullet = match mode {
                CompetitionMode::Kaggle => {
                    "• `public leaderboard [top=N] [order=best|mean] [values=on|off] [range=MIN:MAX] \
                     [axis=on|off] [median=on|off]` - Generate a PUBLIC gain leaderboard image, safe to share \
                     with students (never shows a private gain)\n\
                     • `baseline <name>` (attach one or more CSVs) - Upload a reference model. Never ranked or \
                     graded; hidden from the public image until published\n\
                     • `baseline list` - Every baseline, with its ID and whether it's published\n\
                     • `baseline publish <id>` / `baseline hide <id>` - Show or hide a baseline on the public image\n"
                }
                CompetitionMode::Blind => "",
            };
            let teacher_submit_note = match mode {
                CompetitionMode::Kaggle => {
                    "- Teachers cannot `submit`; use `baseline` for reference models, which appear marked and \
                     unnumbered in `leaderboard` and, once published, in `public leaderboard`."
                }
                CompetitionMode::Blind => "- Teachers cannot submit models.",
            };
            format!(
                "🤖 **Help for Teachers**\n\n\
                **Competition:** {}\n\
                **Description:** {}\n\
                **Deadline:** {}\n\
                **Roster:** {} competitors\n\n\
                **Available commands:**\n\
                • `duplicates` - List duplicate submissions\n\
                • `leaderboard [gain|datetime]` - Full leaderboard with statistics (sorted by gain or date)\n\
                {}\
                • `all submits` - Generate and upload a CSV of every submission in the system\n\
                • `no submits` - View roster members with no submission at all\n\
                • `user submits @user` - View a user's submissions (use an @ mention)\n\
                • `roster reload` - Reload the roster from the CSV\n\
                • `grades` - Generate and upload the grades CSV (10 at the max, 8 at the median, linear in between)\n\
                • `help` - Show this help\n\n\
                **Notes:**\n\
                {}\n\
                {}\n\
                - Anyone with no valid submission before the deadline gets a grade of 0.",
                comp.name,
                comp.description,
                comp.deadline,
                self.roster.borrow().len(),
                public_leaderboard_bullet,
                teacher_submit_note,
                mode_notes
            )
        } else {
            match mode {
                CompetitionMode::Blind => format!(
                    "🤖 **Help for Students**\n\n\
                    **Competition:** {}\n\
                    **Description:** {}\n\
                    **Deadline:** {}\n\n\
                    **Available commands:**\n\
                    • `submit <name> <expected_gain>` - Submit a model outcome (attach a CSV)\n\
                    • `reveal <name> <expected_gain>` - Like `submit`, but spends a golden bullet to see your true gain \n\
                    • `list submits` - List your submissions\n\
                    • `help` - Show this help\n\n\
                    **CSV format:** 1 column with the IDs you predict as positive (no header)\n\n\
                    ℹ️ You have a daily submission limit and a limited number of golden bullets for the whole competition \
                    (both are shown in the reply to each submission). \
                    Your final result is your LAST on-time submission, not your best one -- choose carefully what you send last.",
                    comp.name, comp.description, comp.deadline
                ),
                CompetitionMode::Kaggle => format!(
                    "🤖 **Help for Students**\n\n\
                    **Competition:** {}\n\
                    **Description:** {}\n\
                    **Deadline:** {}\n\n\
                    **Available commands:**\n\
                    • `submit <name>` - Submit one or more candidate CSVs (one submission; no expected gain to type -- \
                    you're shown the mean and standard deviation of the public gain across your candidates right away, not each one individually)\n\
                    • `list submits` - List your submissions\n\
                    • `help` - Show this help\n\n\
                    **CSV format:** 1 column with the IDs you predict as positive (no header)\n\n\
                    ℹ️ You have a daily submission limit (one submission can have several CSVs; each submission spends 1, no matter how many \
                    files it has) and a maximum of files per submission, shown in the reply to each submission. \
                    Your final result is your LAST on-time submission, not your best one -- choose carefully what you send last.",
                    comp.name, comp.description, comp.deadline
                ),
            }
        }
    }
}

/// "3 competitors" or "3 competitors, 1 baseline" for the public board reply.
fn row_counts(rows: &[public_board::BoardRow]) -> String {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let baselines = rows.iter().filter(|r| r.is_baseline).count();
    let competitors = rows.len() - baselines;
    if baselines == 0 {
        plural(competitors, "competitor")
    } else {
        format!("{}, {}", plural(competitors, "competitor"), plural(baselines, "baseline"))
    }
}
