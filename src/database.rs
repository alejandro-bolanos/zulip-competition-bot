use crate::models::Submission;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashSet;

pub struct Database {
    path: String,
}

impl Database {
    pub fn new(path: &str) -> Result<Self> {
        Ok(Self {
            path: path.to_string(),
        })
    }

    fn get_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.path)?)
    }

    pub fn init(&self) -> Result<()> {
        let conn = self.get_connection()?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS submissions (
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
                after_deadline INTEGER DEFAULT 0,
                used_golden_bullet INTEGER NOT NULL DEFAULT 0,
                batch_id TEXT,
                public_gain REAL,
                private_gain REAL
            )",
            [],
        )?;

        // Migrations for databases created before a given column existed.
        // CREATE TABLE above already includes each column on a fresh DB, so
        // these fail with "duplicate column name" there -- that specific
        // failure is expected and ignored; anything else is a real error.
        // Add the next column addition here, following the same pattern.
        for migration in [
            "ALTER TABLE submissions ADD COLUMN used_golden_bullet INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE submissions ADD COLUMN batch_id TEXT",
            "ALTER TABLE submissions ADD COLUMN public_gain REAL",
            "ALTER TABLE submissions ADD COLUMN private_gain REAL",
        ] {
            if let Err(e) = conn.execute(migration, []) {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    pub fn save_submission(&self, submission: &Submission) -> Result<i64> {
        let conn = self.get_connection()?;

        conn.execute(
            "INSERT INTO submissions (
                user_id, user_email, user_full_name, submission_name,
                timestamp, file_checksum, file_path, expected_gain, actual_gain,
                tp, tn, fp, fn, positives_predicted, threshold_category, after_deadline,
                used_golden_bullet, batch_id, public_gain, private_gain
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                submission.user_id,
                submission.user_email,
                submission.user_full_name,
                submission.submission_name,
                submission.timestamp,
                submission.file_checksum,
                submission.file_path,
                submission.expected_gain,
                submission.actual_gain,
                submission.tp,
                submission.tn,
                submission.fp,
                submission.fn_,
                submission.positives_predicted,
                submission.threshold_category,
                submission.after_deadline as i32,
                submission.used_golden_bullet as i32,
                submission.batch_id,
                submission.public_gain,
                submission.private_gain,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// Shared row mapper for every `SELECT ... FROM submissions` that returns
    /// full rows, so the 20-column layout is written down in exactly one
    /// place. Column order must match every query above using it.
    fn row_to_submission(row: &rusqlite::Row) -> rusqlite::Result<Submission> {
        Ok(Submission {
            id: Some(row.get(0)?),
            user_id: row.get(1)?,
            user_email: row.get(2)?,
            user_full_name: row.get(3)?,
            submission_name: row.get(4)?,
            timestamp: row.get(5)?,
            file_checksum: row.get(6)?,
            file_path: row.get(7)?,
            expected_gain: row.get(8)?,
            actual_gain: row.get(9)?,
            tp: row.get(10)?,
            tn: row.get(11)?,
            fp: row.get(12)?,
            fn_: row.get(13)?,
            positives_predicted: row.get(14)?,
            threshold_category: row.get(15)?,
            after_deadline: row.get::<_, i32>(16)? != 0,
            used_golden_bullet: row.get::<_, i32>(17)? != 0,
            batch_id: row.get(18)?,
            public_gain: row.get(19)?,
            private_gain: row.get(20)?,
        })
    }

    /// Counts golden bullets already spent by this user -- derived from
    /// `submissions` rather than a separate table, so there is nothing to
    /// keep in sync. Every stored row with the flag counts, including
    /// after-deadline ones: once a submission is stored, the bullet is
    /// spent, the same principle as the daily quota.
    pub fn get_golden_bullets_used(&self, user_id: i64) -> Result<u32> {
        let conn = self.get_connection()?;
        let count: u32 = conn.query_row(
            "SELECT COUNT(*) FROM submissions WHERE user_id = ?1 AND used_golden_bullet = 1",
            [user_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Keyed on user_id, not display name: names are not unique in Zulip, and
    /// matching on them leaks one user's submissions to their namesake.
    pub fn get_user_submissions(&self, user_id: i64) -> Result<Vec<Submission>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, user_email, user_full_name, submission_name,
                    timestamp, file_checksum, file_path, expected_gain, actual_gain,
                    tp, tn, fp, fn, positives_predicted, threshold_category, after_deadline,
                    used_golden_bullet, batch_id, public_gain, private_gain
             FROM submissions
             WHERE user_id = ?1
             ORDER BY timestamp DESC",
        )?;

        let submissions = stmt
            .query_map([user_id], Self::row_to_submission)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(submissions)
    }

    pub fn get_duplicates(&self) -> Result<Vec<(String, i32, String, String)>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT file_checksum, COUNT(*),
                    GROUP_CONCAT(DISTINCT user_full_name) as users,
                    GROUP_CONCAT(submission_name) as names
             FROM submissions
             GROUP BY file_checksum
             HAVING COUNT(DISTINCT user_id) > 1",
        )?;

        let duplicates = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(duplicates)
    }

    /// Blind mode ranks by each user's most recent pre-deadline submission
    /// (`actual_gain`, against the whole dataset). Kaggle mode instead ranks
    /// by the PRIVATE gain of the best-on-PUBLIC candidate within the user's
    /// LAST pre-deadline batch -- there is no explicit "choose" step, the
    /// last submit is always the one that counts, mirroring blind mode's own
    /// rule. Same return shape either way, so callers (leaderboard rendering,
    /// grade export) don't need to know which mode is active.
    /// `total_submissions` counts distinct submissions, not raw candidate rows --
    /// `COALESCE(batch_id, id)` collapses a kaggle batch's N candidate rows
    /// to 1, and is a no-op in blind mode where `batch_id` is always NULL.
    #[allow(clippy::type_complexity)]
    pub fn get_leaderboard(
        &self,
        order_by: &str,
        mode: crate::config::CompetitionMode,
    ) -> Result<Vec<(String, String, String, f64, f64, i32, Option<f64>, bool)>> {
        let conn = self.get_connection()?;

        let query = match mode {
            crate::config::CompetitionMode::Blind => {
                let order_clause = match order_by {
                    "datetime" => "ORDER BY lv.timestamp DESC",
                    _ => "ORDER BY lv.actual_gain DESC",
                };
                format!(
                    "WITH last_valid AS (
                        SELECT
                            user_id,
                            actual_gain,
                            expected_gain,
                            timestamp,
                            used_golden_bullet,
                            ROW_NUMBER() OVER (
                                PARTITION BY user_id
                                ORDER BY timestamp DESC, id DESC
                            ) AS rn
                        FROM submissions
                        WHERE after_deadline = 0
                    ),
                    stats AS (
                        SELECT
                            user_id,
                            MAX(user_full_name) AS user_full_name,
                            MAX(user_email) AS user_email,
                            COUNT(DISTINCT COALESCE(batch_id, CAST(id AS TEXT))) AS total_submissions,
                            MAX(CASE WHEN after_deadline = 0 THEN actual_gain END) AS max_gain
                        FROM submissions
                        GROUP BY user_id
                    )
                    SELECT
                        st.user_full_name, st.user_email, lv.timestamp, lv.actual_gain,
                        lv.expected_gain, st.total_submissions, st.max_gain, lv.used_golden_bullet
                    FROM stats st
                    JOIN last_valid lv ON lv.user_id = st.user_id AND lv.rn = 1
                    {}",
                    order_clause
                )
            }
            crate::config::CompetitionMode::Kaggle => {
                let order_clause = match order_by {
                    "datetime" => "ORDER BY bp.timestamp DESC",
                    _ => "ORDER BY bp.private_gain DESC",
                };
                format!(
                    "WITH final_batch AS (
                        SELECT
                            user_id,
                            COALESCE(batch_id, CAST(id AS TEXT)) AS batch_key,
                            ROW_NUMBER() OVER (
                                PARTITION BY user_id
                                ORDER BY timestamp DESC, id DESC
                            ) AS rn
                        FROM submissions
                        WHERE after_deadline = 0
                    ),
                    best_public AS (
                        SELECT
                            s.user_id, s.user_full_name, s.user_email, s.timestamp,
                            s.private_gain, s.expected_gain, s.used_golden_bullet,
                            ROW_NUMBER() OVER (
                                PARTITION BY s.user_id
                                ORDER BY s.public_gain DESC, s.id DESC
                            ) AS rn
                        FROM submissions s
                        JOIN final_batch fb
                            ON fb.user_id = s.user_id
                            AND fb.batch_key = COALESCE(s.batch_id, CAST(s.id AS TEXT))
                            AND fb.rn = 1
                        WHERE s.after_deadline = 0
                    ),
                    stats AS (
                        SELECT
                            user_id,
                            MAX(user_full_name) AS user_full_name,
                            MAX(user_email) AS user_email,
                            COUNT(DISTINCT COALESCE(batch_id, CAST(id AS TEXT))) AS total_submissions,
                            MAX(CASE WHEN after_deadline = 0 THEN private_gain END) AS max_gain
                        FROM submissions
                        GROUP BY user_id
                    )
                    SELECT
                        st.user_full_name, st.user_email, bp.timestamp, bp.private_gain,
                        bp.expected_gain, st.total_submissions, st.max_gain, bp.used_golden_bullet
                    FROM stats st
                    JOIN best_public bp ON bp.user_id = st.user_id AND bp.rn = 1
                    {}",
                    order_clause
                )
            }
        };

        let mut stmt = conn.prepare(&query)?;

        let results = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, i32>(5)?,
                    row.get::<_, Option<f64>>(6)?,
                    row.get::<_, i32>(7)? != 0,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    pub fn get_user_submissions_by_identifier(&self, identifier: &str) -> Result<Vec<Submission>> {
        let conn = self.get_connection()?;
        let pattern = format!("%{}%", identifier);
        let mut stmt = conn.prepare(
            "SELECT id, user_id, user_email, user_full_name, submission_name,
                    timestamp, file_checksum, file_path, expected_gain, actual_gain,
                    tp, tn, fp, fn, positives_predicted, threshold_category, after_deadline,
                    used_golden_bullet, batch_id, public_gain, private_gain
             FROM submissions
             WHERE user_email LIKE ?1 OR user_full_name LIKE ?1
             ORDER BY timestamp DESC",
        )?;

        let submissions = stmt
            .query_map([&pattern], Self::row_to_submission)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(submissions)
    }

    pub fn get_all_submissions(&self) -> Result<Vec<Submission>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, user_email, user_full_name, submission_name,
                    timestamp, file_checksum, file_path, expected_gain, actual_gain,
                    tp, tn, fp, fn, positives_predicted, threshold_category, after_deadline,
                    used_golden_bullet, batch_id, public_gain, private_gain
             FROM submissions
             ORDER BY timestamp DESC",
        )?;

        let submissions = stmt
            .query_map([], Self::row_to_submission)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(submissions)
    }

    /// Every pre-deadline kaggle candidate's PUBLIC gain, grouped by
    /// competitor. Deliberately never selects `private_gain` or `user_email`:
    /// this feeds the public leaderboard image, which is shown to the whole
    /// class -- see `public_board.rs`'s privacy regression test.
    /// `public_gain IS NOT NULL` scopes this to kaggle rows on its own; blind
    /// mode always stores `NULL` there.
    pub fn get_public_candidates(&self) -> Result<Vec<(i64, String, String, f64)>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT user_id,
                    user_full_name,
                    COALESCE(batch_id, CAST(id AS TEXT)) AS batch_key,
                    public_gain
             FROM submissions
             WHERE after_deadline = 0 AND public_gain IS NOT NULL
             ORDER BY user_id, batch_key, id",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Lowercased distinct emails with at least one submission, of any kind
    /// (including late ones -- this answers "has this person engaged at all",
    /// not "does this person have a valid entry"). Used to derive who on the
    /// roster is still missing, without querying Zulip's user directory.
    pub fn get_distinct_submitter_emails(&self) -> Result<HashSet<String>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare("SELECT DISTINCT user_email FROM submissions")?;

        let emails = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|e| e.to_lowercase())
            .collect();

        Ok(emails)
    }
}
