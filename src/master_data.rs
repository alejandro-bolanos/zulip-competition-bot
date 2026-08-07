use anyhow::{Context, Result};
use csv::ReaderBuilder;
use std::collections::HashSet;
use std::fs::File;

#[derive(Debug)]
pub struct MasterData {
    all_ids: HashSet<i32>,
    positive_ids: HashSet<i32>,
    /// Empty (and `has_split() == false`) unless master_data.csv carries a
    /// third `split` column -- required for kaggle mode, irrelevant otherwise.
    public_ids: HashSet<i32>,
    private_ids: HashSet<i32>,
    has_split: bool,
}

impl MasterData {
    pub fn load(path: &str) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open master data file: {}", path))?;

        let mut reader = ReaderBuilder::new().has_headers(true).from_reader(file);

        // The csv crate enforces that every record has the same field count
        // as the header, so this is settled once, not re-checked per row.
        let has_split = reader
            .headers()
            .with_context(|| format!("Failed to read master data header: {}", path))?
            .len()
            >= 3;

        let mut all_ids = HashSet::new();
        let mut positive_ids = HashSet::new();
        let mut public_ids = HashSet::new();
        let mut private_ids = HashSet::new();

        for result in reader.records() {
            let record = result?;

            if record.len() < 2 {
                anyhow::bail!(
                    "Invalid CSV format: expected at least 2 columns (id, label)"
                );
            }

            let id: i32 = record[0]
                .parse()
                .with_context(|| format!("Invalid ID: {}", &record[0]))?;

            let label: i32 = record[1]
                .parse()
                .with_context(|| format!("Invalid label: {}", &record[1]))?;

            all_ids.insert(id);

            if label == 1 {
                positive_ids.insert(id);
            }

            if has_split {
                match record[2].trim().to_lowercase().as_str() {
                    "public" => {
                        public_ids.insert(id);
                    }
                    "private" => {
                        private_ids.insert(id);
                    }
                    other => anyhow::bail!(
                        "{}: invalid split '{}' for id {} (must be 'public' or 'private')",
                        path,
                        other,
                        id
                    ),
                }
            }
        }

        Ok(Self {
            all_ids,
            positive_ids,
            public_ids,
            private_ids,
            has_split,
        })
    }

    pub fn validate_ids(&self, predicted_ids: &HashSet<i32>) -> Vec<i32> {
        predicted_ids
            .iter()
            .filter(|id| !self.all_ids.contains(id))
            .copied()
            .collect()
    }

    pub fn all_ids(&self) -> &HashSet<i32> {
        &self.all_ids
    }

    pub fn positive_ids(&self) -> &HashSet<i32> {
        &self.positive_ids
    }

    pub fn total_count(&self) -> usize {
        self.all_ids.len()
    }

    pub fn positive_count(&self) -> usize {
        self.positive_ids.len()
    }

    /// Whether master_data.csv carried a `split` column -- required for
    /// kaggle mode, checked at startup in `main.rs`.
    pub fn has_split(&self) -> bool {
        self.has_split
    }

    pub fn public_ids(&self) -> &HashSet<i32> {
        &self.public_ids
    }

    pub fn private_ids(&self) -> &HashSet<i32> {
        &self.private_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn master_from(header: &str, rows: &str) -> Result<MasterData> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}\n{}", header, rows).unwrap();
        file.flush().unwrap();
        MasterData::load(file.path().to_str().unwrap())
    }

    #[test]
    fn two_column_csv_has_no_split() {
        let data = master_from("id,label", "1,1\n2,0\n").unwrap();
        assert!(!data.has_split());
        assert!(data.public_ids().is_empty());
        assert!(data.private_ids().is_empty());
        assert_eq!(data.total_count(), 2);
    }

    #[test]
    fn three_column_csv_partitions_into_public_and_private() {
        let data = master_from(
            "id,label,split",
            "1,1,public\n2,0,private\n3,1,public\n4,0,private\n",
        )
        .unwrap();

        assert!(data.has_split());
        assert_eq!(data.public_ids(), &[1, 3].into_iter().collect());
        assert_eq!(data.private_ids(), &[2, 4].into_iter().collect());
        // Every id lands in exactly one side of the split.
        assert_eq!(data.public_ids().len() + data.private_ids().len(), data.total_count());
    }

    #[test]
    fn three_column_csv_rejects_an_unknown_split_value() {
        let err = master_from("id,label,split", "1,1,who_knows\n").unwrap_err();
        assert!(err.to_string().contains("invalid split"), "got: {}", err);
    }
}
