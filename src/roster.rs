use anyhow::{bail, Context, Result};
use csv::ReaderBuilder;
use std::collections::HashMap;
use std::fs::File;

/// One row of the roster CSV: a competitor allowed to interact with the bot,
/// keyed by email. Being on the roster is the sole authorization check for
/// students -- someone absent from it gets a single rejection banner and
/// nothing else, regardless of what they type.
#[derive(Debug, Clone, PartialEq)]
pub struct Competitor {
    /// Always lowercase; this is the join key against Zulip's sender_email.
    pub email: String,
    pub full_name: String,
    pub daily_submit_limit: u32,
    pub golden_bullets: u32,
    /// Only meaningful in kaggle mode (`competition.mode`): the max number of
    /// candidate CSVs allowed in a single `submit` submission. Ignored in
    /// blind mode, where a submission is always exactly one CSV.
    pub max_files_per_submission: u32,
}

#[derive(Debug)]
pub struct Roster {
    by_email: HashMap<String, Competitor>,
}

const EXPECTED_HEADER: [&str; 5] = [
    "email",
    "name",
    "daily_limit",
    "golden_bullets",
    "max_files_per_submission",
];

impl Roster {
    pub fn load(path: &str) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open roster file: {}", path))?;

        let mut reader = ReaderBuilder::new().has_headers(true).from_reader(file);

        let headers = reader
            .headers()
            .with_context(|| format!("Failed to read roster header: {}", path))?
            .clone();
        if headers.iter().collect::<Vec<_>>() != EXPECTED_HEADER {
            bail!(
                "{}: header must be exactly {:?}, got {:?}",
                path,
                EXPECTED_HEADER,
                headers.iter().collect::<Vec<_>>()
            );
        }

        let mut by_email = HashMap::new();

        for (i, result) in reader.records().enumerate() {
            let line = i + 2; // 1-indexed rows, plus the header row
            let record = result.with_context(|| format!("{}:{}", path, line))?;

            let email = record[0].trim().to_lowercase();
            if email.is_empty() {
                bail!("{}:{}: email must not be empty", path, line);
            }

            let full_name = record[1].trim().to_string();

            let daily_submit_limit: u32 = record[2].trim().parse().with_context(|| {
                format!("{}:{}: invalid daily_limit '{}'", path, line, &record[2])
            })?;

            let golden_bullets: u32 = record[3].trim().parse().with_context(|| {
                format!("{}:{}: invalid golden_bullets '{}'", path, line, &record[3])
            })?;

            let max_files_per_submission: u32 = record[4].trim().parse().with_context(|| {
                format!(
                    "{}:{}: invalid max_files_per_submission '{}'",
                    path, line, &record[4]
                )
            })?;

            if by_email.contains_key(&email) {
                bail!("{}:{}: duplicate email '{}'", path, line, email);
            }

            by_email.insert(
                email.clone(),
                Competitor {
                    email,
                    full_name,
                    daily_submit_limit,
                    golden_bullets,
                    max_files_per_submission,
                },
            );
        }

        Ok(Self { by_email })
    }

    pub fn get(&self, email: &str) -> Option<&Competitor> {
        self.by_email.get(&email.to_lowercase())
    }

    pub fn is_enabled(&self, email: &str) -> bool {
        self.get(email).is_some()
    }

    pub fn len(&self) -> usize {
        self.by_email.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_email.is_empty()
    }

    pub fn competitors(&self) -> impl Iterator<Item = &Competitor> {
        self.by_email.values()
    }
}

/// Test-only constructor: builds a Roster in memory, without writing a CSV to
/// disk. `pub(crate)` so both this module's tests and src/tests.rs can use it.
#[cfg(test)]
pub(crate) fn from_competitors(competitors: Vec<Competitor>) -> Roster {
    Roster {
        by_email: competitors.into_iter().map(|c| (c.email.clone(), c)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn roster_from(rows: &str) -> Result<Roster> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            "email,name,daily_limit,golden_bullets,max_files_per_submission\n{}",
            rows
        )
        .unwrap();
        file.flush().unwrap();
        Roster::load(file.path().to_str().unwrap())
    }

    #[test]
    fn loads_valid_rows() {
        let roster =
            roster_from("Ana@Example.com,Ana Gomez,5,3,1\nbeto@example.com,Beto Diaz,3,1,4\n")
                .unwrap();

        assert_eq!(roster.len(), 2);

        // Lookup is case-insensitive: Zulip's sender_email casing is not
        // guaranteed to match how the teacher typed it into the CSV.
        let ana = roster.get("ANA@EXAMPLE.COM").unwrap();
        assert_eq!(ana.email, "ana@example.com");
        assert_eq!(ana.full_name, "Ana Gomez");
        assert_eq!(ana.daily_submit_limit, 5);
        assert_eq!(ana.golden_bullets, 3);
        assert_eq!(ana.max_files_per_submission, 1);

        assert!(roster.is_enabled("beto@example.com"));
        assert!(!roster.is_enabled("nadie@example.com"));
    }

    #[test]
    fn rejects_wrong_header() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            "mail,name,daily_limit,golden_bullets,max_files_per_submission\na@e.com,A,1,0,1\n"
        )
        .unwrap();
        file.flush().unwrap();
        assert!(Roster::load(file.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn rejects_duplicate_email_case_insensitively() {
        let err = roster_from("ana@example.com,Ana,5,0,1\nANA@EXAMPLE.COM,Ana Other,5,0,1\n")
            .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "got: {}", err);
    }

    #[test]
    fn rejects_malformed_numeric_columns() {
        assert!(
            roster_from("a@e.com,A,five,0,1\n").is_err(),
            "non-numeric daily_limit"
        );
        assert!(
            roster_from("a@e.com,A,5,-1,1\n").is_err(),
            "negative golden_bullets (unsigned)"
        );
        assert!(
            roster_from("a@e.com,A,5,0,-1\n").is_err(),
            "negative max_files_per_submission (unsigned)"
        );
    }

    #[test]
    fn rejects_empty_email() {
        assert!(roster_from(" ,A,5,0,1\n").is_err());
    }

    #[test]
    fn empty_roster_is_valid() {
        let roster = roster_from("").unwrap();
        assert_eq!(roster.len(), 0);
        assert!(!roster.is_enabled("anyone@example.com"));
    }

    #[test]
    fn from_competitors_builds_an_in_memory_roster() {
        let roster = from_competitors(vec![Competitor {
            email: "ana@example.com".to_string(),
            full_name: "Ana".to_string(),
            daily_submit_limit: 5,
            golden_bullets: 0,
            max_files_per_submission: 1,
        }]);
        assert!(roster.is_enabled("ana@example.com"));
        assert_eq!(roster.len(), 1);
    }
}
