/// Cross-module tests that exercise the real code paths (config validation,
/// the leaderboard SQL, per-user isolation, batch atomicity).
#[cfg(test)]
mod behaviour {
    use crate::config::{parse_config_datetime, BotConfig, CompetitionMode};
    use crate::database::Database;
    use crate::models::Submission;
    use tempfile::TempDir;

    fn config_json(thresholds: &str, deadline: &str, reveal: &str) -> String {
        format!(
            r#"{{
              "zulip": {{ "email": "b@e.com", "api_key": "k", "site": "https://e.com" }},
              "database": {{ "path": "t.db" }},
              "logs": {{ "path": "logs" }},
              "teachers": ["t@e.com"],
              "master_data": {{ "path": "m.csv" }},
              "submissions": {{ "path": "./s" }},
              "roster": {{ "path": "roster.csv" }},
              "gain_matrix": {{ "tp": 1.0, "tn": 0.0, "fp": -1.0, "fn_": 0.0 }},
              "gain_thresholds": {thresholds},
              "competition": {{
                "name": "C", "description": "D",
                "deadline": "{deadline}", "results_reveal_date": "{reveal}"
              }}
            }}"#
        )
    }

    const OK_THRESHOLDS: &str = r#"[{"min_gain": 0.0, "category": "a", "message": "m"}]"#;

    fn parse(json: &str) -> BotConfig {
        serde_json::from_str(json).expect("valid json")
    }

    #[test]
    fn accepts_rfc3339_and_naive_datetimes() {
        let with_offset = parse_config_datetime("2025-12-31T23:59:59Z", 0).unwrap();
        let naive = parse_config_datetime("2025-12-31T23:59:59", 0).unwrap();
        // At offset 0, a naive value is read as UTC, so both spellings mean
        // the same instant.
        assert_eq!(with_offset, naive);
    }

    #[test]
    fn rejects_unparseable_datetime() {
        assert!(parse_config_datetime("31/12/2025", 0).is_err());
        assert!(parse_config_datetime("2025-12-31", 0).is_err());
    }

    #[test]
    fn naive_datetime_is_read_at_the_configured_offset() {
        // Argentina (UTC-3): local 23:59:59 is UTC 02:59:59 the NEXT day.
        let arg_local = parse_config_datetime("2025-06-15T23:59:59", -180).unwrap();
        let expected_utc = parse_config_datetime("2025-06-16T02:59:59Z", 0).unwrap();
        assert_eq!(arg_local, expected_utc);
    }

    #[test]
    fn an_explicit_rfc3339_offset_ignores_the_config_offset() {
        // The string's own offset always wins; the config offset only
        // disambiguates a naive string.
        let a = parse_config_datetime("2025-06-15T23:59:59-03:00", 0).unwrap();
        let b = parse_config_datetime("2025-06-15T23:59:59-03:00", -180).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn validate_rejects_empty_thresholds() {
        // This is the config that used to panic mid-submission instead.
        let config = parse(&config_json(
            "[]",
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_duplicate_threshold_categories() {
        let dupes = r#"[
            {"min_gain": 0.0, "category": "a", "message": "first"},
            {"min_gain": 10.0, "category": "a", "message": "second"}
        ]"#;
        let config = parse(&config_json(dupes, "2025-12-31T23:59:59", "2026-01-01T23:59:59"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_dates() {
        let bad_deadline = parse(&config_json(OK_THRESHOLDS, "not-a-date", "2026-01-01T23:59:59"));
        assert!(bad_deadline.validate().is_err());

        let bad_reveal = parse(&config_json(OK_THRESHOLDS, "2025-12-31T23:59:59", "nope"));
        assert!(bad_reveal.validate().is_err());
    }

    #[test]
    fn validate_rejects_an_empty_log_path() {
        let mut config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        assert!(config.validate().is_ok());

        // create_dir_all("") fails with an opaque error, so catch it up front.
        config.logs.path = "   ".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_accepts_a_good_config() {
        let config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_an_empty_roster_path() {
        let mut config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        config.roster.path = "   ".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_an_out_of_range_timezone_offset() {
        let mut config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        config.competition.timezone_offset_minutes = 2000; // no real UTC offset is this large
        assert!(config.validate().is_err());
    }

    #[test]
    fn timezone_offset_defaults_to_zero_when_absent() {
        // Configs written before this field existed must keep meaning UTC.
        let config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        assert_eq!(config.competition.timezone_offset_minutes, 0);
    }

    #[test]
    fn competition_mode_defaults_to_blind_and_serializes_lowercase() {
        // Configs written before kaggle mode existed must keep meaning blind.
        let config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        assert_eq!(config.competition.mode, CompetitionMode::Blind);

        assert_eq!(serde_json::to_string(&CompetitionMode::Blind).unwrap(), "\"blind\"");
        assert_eq!(serde_json::to_string(&CompetitionMode::Kaggle).unwrap(), "\"kaggle\"");
        assert_eq!(
            serde_json::from_str::<CompetitionMode>("\"blind\"").unwrap(),
            CompetitionMode::Blind
        );
    }

    #[test]
    fn local_day_bounds_span_exactly_24_hours() {
        let config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));
        let now = chrono::DateTime::parse_from_rfc3339("2025-06-15T14:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (start, end) = config.competition.local_day_bounds_utc(now);
        assert_eq!(end - start, chrono::Duration::days(1));
        assert!(start <= now && now < end, "now must fall inside its own day");
    }

    #[test]
    fn local_day_bounds_shift_with_the_offset() {
        let mut config = parse(&config_json(
            OK_THRESHOLDS,
            "2025-12-31T23:59:59",
            "2026-01-01T23:59:59",
        ));

        // 02:00 UTC on the 20th is 23:00 Argentina time on the 19th: the UTC
        // calendar day is the 20th, the Argentina calendar day is the 19th.
        let at = chrono::DateTime::parse_from_rfc3339("2025-11-20T02:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        config.competition.timezone_offset_minutes = 0;
        let (utc_start, _) = config.competition.local_day_bounds_utc(at);
        let expected_utc_start = chrono::DateTime::parse_from_rfc3339("2025-11-20T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(utc_start, expected_utc_start);

        config.competition.timezone_offset_minutes = -180;
        let (arg_start, _) = config.competition.local_day_bounds_utc(at);
        // Argentina midnight on the 19th, expressed as a UTC instant.
        let expected_arg_start = chrono::DateTime::parse_from_rfc3339("2025-11-19T03:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(arg_start, expected_arg_start);

        assert!(arg_start < utc_start, "Argentina's day starts earlier in UTC terms");
    }

    fn submission(
        user_id: i64,
        full_name: &str,
        timestamp: &str,
        actual_gain: f64,
        after_deadline: bool,
    ) -> Submission {
        Submission {
            id: None,
            user_id,
            user_email: format!("u{}@e.com", user_id),
            user_full_name: full_name.to_string(),
            submission_name: format!("s-{}", timestamp),
            timestamp: timestamp.to_string(),
            file_checksum: format!("sum-{}-{}", user_id, timestamp),
            file_path: "/tmp/x.csv".to_string(),
            expected_gain: Some(1.0),
            actual_gain,
            tp: 1,
            tn: 1,
            fp: 0,
            fn_: 0,
            positives_predicted: 1,
            threshold_category: "a".to_string(),
            after_deadline,
            used_golden_bullet: false,
            batch_id: None,
            public_gain: None,
            private_gain: None,
            is_baseline: false,
            baseline_published: false,
        }
    }

    fn db_with(submissions: &[Submission]) -> (TempDir, Database) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).unwrap();
        db.init().unwrap();
        for s in submissions {
            db.save_submission(s).unwrap();
        }
        (dir, db)
    }

    #[test]
    fn user_submissions_are_isolated_by_id_not_display_name() {
        // Two distinct Zulip accounts that happen to share a display name.
        let (_dir, db) = db_with(&[
            submission(1, "Ana Gómez", "2025-01-01T10:00:00Z", 10.0, false),
            submission(2, "Ana Gómez", "2025-01-02T10:00:00Z", 20.0, false),
        ]);

        let first = db.get_user_submissions(1).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].user_id, 1);

        let second = db.get_user_submissions(2).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].user_id, 2);
    }

    #[test]
    fn leaderboard_reports_the_last_valid_submission() {
        let (_dir, db) = db_with(&[
            // Best gain is the middle one; the ranking must use the latest.
            submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false),
            submission(1, "Ana", "2025-01-02T10:00:00Z", 99.0, false),
            submission(1, "Ana", "2025-01-03T10:00:00Z", 30.0, false),
            // A late submission counts toward the total but never toward gain.
            submission(1, "Ana", "2025-01-09T10:00:00Z", 500.0, true),
        ]);

        let rows = db.get_leaderboard("gain", CompetitionMode::Blind).unwrap();
        assert_eq!(rows.len(), 1);
        let (name, _email, timestamp, final_gain, _expected, total, max_gain, used_bullet) = &rows[0];

        assert_eq!(name, "Ana");
        assert_eq!(*final_gain, 30.0, "ranks on the latest valid submission");
        // The regression: this used to be an arbitrary row's timestamp.
        assert_eq!(
            timestamp, "2025-01-03T10:00:00Z",
            "timestamp must belong to the submission that set final_gain"
        );
        assert_eq!(*max_gain, Some(99.0), "best-ever gain, deadline-respecting");
        assert_eq!(*total, 4, "counts every submission, including late ones");
        assert!(!used_bullet, "none of these submissions used a golden bullet");
    }

    #[test]
    fn leaderboard_marks_the_chosen_submission_as_golden_bullet() {
        let (_dir, db) = db_with(&[
            with_golden_bullet(submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false)),
            // The latest submission decides the mark, not "ever used one".
            submission(1, "Ana", "2025-01-02T10:00:00Z", 20.0, false),
        ]);

        let rows = db.get_leaderboard("gain", CompetitionMode::Blind).unwrap();
        assert_eq!(rows.len(), 1);
        let (_, _, _, _, _, _, _, used_bullet) = &rows[0];
        assert!(
            !used_bullet,
            "the chosen (latest) submission did not itself use a bullet"
        );

        let (_dir2, db2) = db_with(&[
            submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false),
            with_golden_bullet(submission(1, "Ana", "2025-01-02T10:00:00Z", 20.0, false)),
        ]);
        let rows2 = db2.get_leaderboard("gain", CompetitionMode::Blind).unwrap();
        let (_, _, _, _, _, _, _, used_bullet2) = &rows2[0];
        assert!(used_bullet2, "the chosen (latest) submission did use a bullet");
    }

    #[test]
    fn kaggle_leaderboard_scores_the_best_public_candidate_of_the_last_batch() {
        let (_dir, db) = db_with(&[
            // An EARLIER batch with a much higher private gain. There is no
            // `choose` step anymore -- it must be ignored purely because it
            // isn't the last pre-deadline batch, mirroring blind mode's
            // "last submission wins" rule.
            kaggle_submission(1, "Ana", "2025-01-01T10:00:00Z", "earlier", 100.0, 999.0, false),
            kaggle_submission(1, "Ana", "2025-01-02T10:00:00Z", "last", 5.0, 20.0, false),
            // Best on public within the last batch, and its private gain is
            // what must win -- not the batch's own higher-private-but-lower-public row.
            kaggle_submission(1, "Ana", "2025-01-02T10:00:00Z", "last", 9.0, 30.0, false),
        ]);

        let rows = db.get_leaderboard("gain", CompetitionMode::Kaggle).unwrap();
        assert_eq!(rows.len(), 1);
        let (name, _email, _ts, final_gain, _expected, total, _max, _bullet) = &rows[0];
        assert_eq!(name, "Ana");
        assert_eq!(
            *final_gain, 30.0,
            "must use the LAST batch's best-on-public candidate's private gain"
        );
        assert_eq!(*total, 2, "2 distinct submissions (batches), not 3 candidate rows");
    }

    #[test]
    fn save_batch_stores_all_candidates_or_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).unwrap();
        db.init().unwrap();

        // Make any insert of a row named "boom" fail, so the batch below
        // dies on its SECOND row -- after the first one already went in.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_on_boom BEFORE INSERT ON submissions
                 WHEN NEW.submission_name = 'boom'
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();

        let ok = kaggle_submission(1, "Ana", "2025-01-01T10:00:00Z", "b1", 5.0, 20.0, false);
        let mut bad = ok.clone();
        bad.submission_name = "boom".to_string();

        assert!(db.save_batch(&[ok.clone(), bad]).is_err());
        assert!(
            db.get_user_submissions(1).unwrap().is_empty(),
            "a failed batch must leave nothing behind, not its first row"
        );

        db.save_batch(&[ok.clone(), ok]).unwrap();
        assert_eq!(db.get_user_submissions(1).unwrap().len(), 2);
    }

    #[test]
    fn kaggle_leaderboard_includes_every_user_who_submitted() {
        let (_dir, db) = db_with(&[kaggle_submission(
            1,
            "Ana",
            "2025-01-01T10:00:00Z",
            "b1",
            5.0,
            20.0,
            false,
        )]);
        // No separate pick step -- a single pre-deadline submission is enough
        // to get a leaderboard entry.

        let rows = db.get_leaderboard("gain", CompetitionMode::Kaggle).unwrap();
        assert_eq!(rows.len(), 1, "submitting is enough to get a leaderboard entry");
    }

    #[test]
    fn leaderboard_excludes_users_with_only_late_submissions() {
        let (_dir, db) = db_with(&[
            submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false),
            submission(2, "Beto", "2025-01-02T10:00:00Z", 999.0, true),
        ]);

        let rows = db.get_leaderboard("gain", CompetitionMode::Blind).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "Ana");
    }

    /// Overrides the checksum, which `submission()` otherwise derives per user.
    fn with_checksum(mut s: Submission, checksum: &str) -> Submission {
        s.file_checksum = checksum.to_string();
        s
    }

    fn with_golden_bullet(mut s: Submission) -> Submission {
        s.used_golden_bullet = true;
        s
    }

    fn kaggle_submission(
        user_id: i64,
        full_name: &str,
        timestamp: &str,
        batch_id: &str,
        public_gain: f64,
        private_gain: f64,
        after_deadline: bool,
    ) -> Submission {
        let mut s = submission(user_id, full_name, timestamp, private_gain, after_deadline);
        s.batch_id = Some(batch_id.to_string());
        s.public_gain = Some(public_gain);
        s.private_gain = Some(private_gain);
        s
    }

    /// A teacher's baseline candidate: user 99, `t@e.com`, named `name`.
    fn baseline_candidate(name: &str, batch_id: &str, public_gain: f64, private_gain: f64) -> Submission {
        let mut s = kaggle_submission(99, "Teacher", "2025-01-05T10:00:00Z", batch_id, public_gain, private_gain, false);
        s.user_email = "t@e.com".to_string();
        s.submission_name = name.to_string();
        s.is_baseline = true;
        s
    }

    #[test]
    fn baselines_never_enter_the_private_leaderboard_rows() {
        let (_dir, db) = db_with(&[
            kaggle_submission(1, "Ana", "2025-01-01T10:00:00Z", "a1", 5.0, 20.0, false),
            // Beats Ana on both splits -- must still never become a row.
            baseline_candidate("logistic", "t1", 900.0, 900.0),
        ]);
        for mode in [CompetitionMode::Kaggle, CompetitionMode::Blind] {
            let rows = db.get_leaderboard("gain", mode).unwrap();
            assert_eq!(rows.len(), 1, "{mode:?}: only the competitor is a leaderboard row");
            assert_eq!(rows[0].0, "Ana");
        }
    }

    #[test]
    fn baselines_never_reach_the_public_candidates_even_if_the_teacher_is_on_the_roster() {
        let (_dir, db) = db_with(&[
            kaggle_submission(1, "Ana", "2025-01-01T10:00:00Z", "a1", 5.0, 20.0, false),
            baseline_candidate("logistic", "t1", 50.0, 50.0),
        ]);
        let teacher = crate::roster::Competitor {
            email: "t@e.com".to_string(),
            full_name: "Teacher".to_string(),
            daily_submit_limit: 5,
            golden_bullets: 0,
            max_files_per_submission: 5,
        };
        let mut ana = teacher.clone();
        ana.email = "u1@e.com".to_string();
        let roster = crate::roster::from_competitors(vec![teacher, ana]);

        let candidates = db.get_public_candidates(&roster).unwrap();
        assert_eq!(candidates.len(), 1, "the baseline must not appear as a competitor candidate");
        assert_eq!(candidates[0].0, 1);
    }

    #[test]
    fn a_baseline_is_hidden_until_published_and_hides_again() {
        let (_dir, db) = db_with(&[
            baseline_candidate("logistic", "t1", 50.0, 40.0),
            baseline_candidate("logistic", "t1", 30.0, 60.0),
        ]);
        assert!(db.get_public_baselines().unwrap().is_empty(), "hidden by default");

        assert!(db.set_baseline_published("t1", true).unwrap());
        let public = db.get_public_baselines().unwrap();
        assert_eq!(public.len(), 2, "every candidate of the batch, for its shape");
        assert!(public.iter().all(|(name, batch, _)| name == "logistic" && batch == "t1"));

        assert!(db.set_baseline_published("t1", false).unwrap());
        assert!(db.get_public_baselines().unwrap().is_empty());
    }

    #[test]
    fn publishing_by_a_students_batch_id_changes_nothing() {
        let (_dir, db) = db_with(&[kaggle_submission(1, "Ana", "2025-01-01T10:00:00Z", "a1", 5.0, 20.0, false)]);
        assert!(!db.set_baseline_published("a1", true).unwrap(), "a1 is not a baseline");
        assert!(!db.set_baseline_published("nope", true).unwrap());
        assert!(!db.get_user_submissions(1).unwrap()[0].baseline_published);
        assert!(db.get_public_baselines().unwrap().is_empty());
    }

    #[test]
    fn baseline_summary_scores_the_best_public_candidate() {
        let (_dir, db) = db_with(&[
            baseline_candidate("logistic", "t1", 50.0, 40.0),
            // Best on public -- its PRIVATE gain is the one reported.
            baseline_candidate("logistic", "t1", 70.0, 10.0),
            baseline_candidate("logistic", "t1", 30.0, 90.0),
            kaggle_submission(1, "Ana", "2025-01-01T10:00:00Z", "a1", 5.0, 20.0, false),
        ]);
        let baselines = db.get_baselines().unwrap();
        assert_eq!(baselines.len(), 1, "one entry per upload, and no competitor rows");
        let b = &baselines[0];
        assert_eq!((b.name.as_str(), b.batch_id.as_str()), ("logistic", "t1"));
        assert_eq!(b.candidates, 3);
        assert_eq!(b.best_public_gain, 70.0);
        assert_eq!(b.private_gain, 10.0, "the best-on-public candidate's private gain, not the best private");
        assert!(!b.published);
    }

    #[test]
    fn no_submits_ignores_a_teachers_baselines() {
        let (_dir, db) = db_with(&[baseline_candidate("logistic", "t1", 50.0, 40.0)]);
        assert!(db.get_distinct_submitter_emails().unwrap().is_empty());
    }

    #[test]
    fn duplicates_need_two_distinct_users() {
        let (_dir, db) = db_with(&[
            // Same file from two accounts: this is the case worth catching.
            with_checksum(
                submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false),
                "shared",
            ),
            with_checksum(
                submission(2, "Beto", "2025-01-02T10:00:00Z", 10.0, false),
                "shared",
            ),
            // One account resubmitting its own file is not a duplicate.
            with_checksum(
                submission(3, "Cyn", "2025-01-03T10:00:00Z", 5.0, false),
                "own",
            ),
            with_checksum(
                submission(3, "Cyn", "2025-01-04T10:00:00Z", 5.0, false),
                "own",
            ),
        ]);

        let rows = db.get_duplicates().unwrap();
        assert_eq!(rows.len(), 1, "only the cross-user collision is reported");

        let (checksum, count, users, _names) = &rows[0];
        assert_eq!(checksum, "shared");
        assert_eq!(*count, 2);
        assert!(users.contains("Ana") && users.contains("Beto"), "got {}", users);
    }

    #[test]
    fn user_lookup_by_identifier_is_a_substring_match() {
        // Documents current behaviour rather than endorsing it: `user submits`
        // builds a LIKE '%needle%', so a name that prefixes another returns both.
        let (_dir, db) = db_with(&[
            submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false),
            submission(2, "Ana María", "2025-01-02T10:00:00Z", 20.0, false),
            submission(3, "Beto", "2025-01-03T10:00:00Z", 30.0, false),
        ]);

        let ana = db.get_user_submissions_by_identifier("Ana").unwrap();
        assert_eq!(ana.len(), 2, "'Ana' also matches 'Ana María'");

        let exact = db.get_user_submissions_by_identifier("Ana María").unwrap();
        assert_eq!(exact.len(), 1);

        // Emails are searched with the same pattern.
        let by_email = db.get_user_submissions_by_identifier("u3@e.com").unwrap();
        assert_eq!(by_email.len(), 1);
        assert_eq!(by_email[0].user_full_name, "Beto");
    }

    #[test]
    fn distinct_submitter_emails_are_lowercased_and_deduplicated() {
        let (_dir, db) = db_with(&[
            submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false),
            submission(1, "Ana", "2025-01-02T10:00:00Z", 20.0, false), // same user, 2nd submission
            submission(2, "Beto", "2025-01-01T10:00:00Z", 5.0, false),
        ]);

        let emails = db.get_distinct_submitter_emails().unwrap();
        assert_eq!(emails.len(), 2, "Ana's two submissions collapse to one email");
        assert!(emails.contains("u1@e.com"));
        assert!(emails.contains("u2@e.com"));
    }

    #[test]
    fn no_submits_lists_only_roster_members_with_zero_submissions() {
        use crate::roster::{from_competitors, Competitor};
        use crate::submission::process_no_submits;

        let (_dir, db) = db_with(&[submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false)]);

        let competitor = |email: &str, name: &str| Competitor {
            email: email.to_string(),
            full_name: name.to_string(),
            daily_submit_limit: 5,
            golden_bullets: 0,
            max_files_per_submission: 1,
        };
        // Ana (u1@e.com) submitted; Beto did not; Cyn is not on the roster at
        // all and must not appear either way.
        let roster = from_competitors(vec![
            competitor("u1@e.com", "Ana"),
            competitor("u2@e.com", "Beto"),
        ]);

        let response = process_no_submits(&db, &roster);
        assert!(!response.contains("Ana"), "Ana already submitted");
        assert!(response.contains("Beto"), "Beto has not submitted");
        assert!(!response.contains("Cyn"), "Cyn is not on the roster");
    }

    #[test]
    fn no_submits_reports_full_participation() {
        use crate::roster::{from_competitors, Competitor};
        use crate::submission::process_no_submits;

        let (_dir, db) = db_with(&[submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false)]);

        let roster = from_competitors(vec![Competitor {
            email: "u1@e.com".to_string(),
            full_name: "Ana".to_string(),
            daily_submit_limit: 5,
            golden_bullets: 0,
            max_files_per_submission: 1,
        }]);

        let response = process_no_submits(&db, &roster);
        assert!(response.contains('✅'), "got: {}", response);
    }

    #[test]
    fn golden_bullets_used_counts_only_the_flagged_rows_for_that_user() {
        let (_dir, db) = db_with(&[
            with_golden_bullet(submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false)),
            submission(1, "Ana", "2025-01-02T10:00:00Z", 20.0, false),
            // Another user's bullet must not count toward Ana's total.
            with_golden_bullet(submission(2, "Beto", "2025-01-01T10:00:00Z", 5.0, false)),
        ]);

        assert_eq!(db.get_golden_bullets_used(1).unwrap(), 1);
        assert_eq!(db.get_golden_bullets_used(2).unwrap(), 1);
        assert_eq!(db.get_golden_bullets_used(3).unwrap(), 0, "no submissions at all");
    }

    #[test]
    fn golden_bullets_used_counts_after_deadline_submissions_too() {
        // Once stored, a bullet is spent -- same principle as the daily
        // quota not refunding a late-but-otherwise-valid submission.
        let (_dir, db) = db_with(&[with_golden_bullet(submission(
            1,
            "Ana",
            "2025-01-01T10:00:00Z",
            10.0,
            true,
        ))]);

        assert_eq!(db.get_golden_bullets_used(1).unwrap(), 1);
    }

    #[test]
    fn init_adds_the_golden_bullet_column_to_a_pre_existing_database() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("old.db");

        // Recreates the schema as it existed before golden bullets, using a
        // raw connection so this test does not depend on Database::init
        // already knowing about the column it's supposed to add.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE submissions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id INTEGER,
                    user_email TEXT,
                    user_full_name TEXT,
                    submission_name TEXT,
                    timestamp TEXT,
                    file_checksum TEXT,
                    file_path TEXT,
                    expected_gain REAL,
                    actual_gain REAL,
                    tp INTEGER,
                    tn INTEGER,
                    fp INTEGER,
                    fn INTEGER,
                    positives_predicted INTEGER,
                    threshold_category TEXT,
                    after_deadline INTEGER DEFAULT 0
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO submissions (user_id, user_email, user_full_name, submission_name,
                     timestamp, file_checksum, file_path, expected_gain, actual_gain,
                     tp, tn, fp, fn, positives_predicted, threshold_category, after_deadline)
                 VALUES (2, 'old@e.com', 'Old', 'old', '2024-01-01T00:00:00Z', 'c', '/x',
                     1.0, 5.0, 0, 0, 0, 0, 0, 'a', 0)",
                [],
            )
            .unwrap();
        }

        let db = Database::new(path.to_str().unwrap()).unwrap();
        db.init().unwrap();

        db.save_submission(&submission(1, "Ana", "2025-01-01T10:00:00Z", 10.0, false))
            .unwrap();
        assert_eq!(db.get_golden_bullets_used(1).unwrap(), 0);

        // A row that predates the baseline columns must read back as an
        // ordinary, unpublished competitor row.
        let old = &db.get_user_submissions(2).unwrap()[0];
        assert!(!old.is_baseline && !old.baseline_published);
    }

    #[test]
    fn leaderboard_orders_by_gain_or_datetime() {
        let (_dir, db) = db_with(&[
            submission(1, "Ana", "2025-01-03T10:00:00Z", 10.0, false),
            submission(2, "Beto", "2025-01-01T10:00:00Z", 50.0, false),
        ]);

        let by_gain: Vec<String> = db
            .get_leaderboard("gain", CompetitionMode::Blind)
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(by_gain, vec!["Beto", "Ana"]);

        let by_date: Vec<String> = db
            .get_leaderboard("datetime", CompetitionMode::Blind)
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(by_date, vec!["Ana", "Beto"]);
    }
}
