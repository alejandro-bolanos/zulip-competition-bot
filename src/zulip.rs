use crate::models::{Event, ZulipEventsResponse};
use anyhow::Result;
use reqwest::Client;
use tracing::{info, warn};

pub struct ZulipClient {
    email: String,
    api_key: String,
    site: String,
    client: Client,
    /// Event queue state. `queue_id` and `last_event_id` belong together: event
    /// ids are only meaningful within one queue, so both are set by
    /// `register_queue` and both are discarded when a queue is dropped.
    queue_id: Option<String>,
    last_event_id: i64,
}

impl ZulipClient {
    pub fn new(email: String, api_key: String, site: String) -> Self {
        Self {
            email,
            api_key,
            site,
            client: Client::new(),
            queue_id: None,
            last_event_id: -1,
        }
    }

    async fn register_queue(&mut self) -> Result<()> {
        let url = format!("{}/api/v1/register", self.site);

        let response = self
            .client
            .post(&url)
            .basic_auth(&self.email, Some(&self.api_key))
            .form(&[("event_types", r#"["message"]"#)])
            .send()
            .await?;

        let data: serde_json::Value = response.json().await?;

        let queue_id = data["queue_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No queue_id in register response: {}", data))?
            .to_string();

        // Anchor to the id Zulip reports for the fresh queue. Carrying over an
        // id from a previous queue makes every subsequent fetch fail.
        let last_event_id = data["last_event_id"].as_i64().unwrap_or(-1);

        info!(
            "Registered event queue {} at last_event_id {}",
            queue_id, last_event_id
        );

        self.queue_id = Some(queue_id);
        self.last_event_id = last_event_id;

        Ok(())
    }

    pub async fn get_events(&mut self) -> Result<Vec<Event>> {
        if self.queue_id.is_none() {
            self.register_queue().await?;
        }

        let queue_id = self
            .queue_id
            .as_ref()
            .expect("queue_id set by register_queue")
            .clone();
        let url = format!("{}/api/v1/events", self.site);

        let response = self
            .client
            .get(&url)
            .basic_auth(&self.email, Some(&self.api_key))
            .query(&[
                ("queue_id", queue_id.as_str()),
                ("last_event_id", &self.last_event_id.to_string()),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!(
                "Event fetch failed ({}), dropping queue to re-register: {}",
                status, body
            );
            // Dropping the queue also drops last_event_id, since register_queue
            // will re-anchor it.
            self.queue_id = None;
            return Ok(vec![]);
        }

        let data: ZulipEventsResponse = response.json().await?;

        if let Some(max_id) = data.events.iter().map(|e| e.id).max() {
            self.last_event_id = max_id;
        }

        Ok(data.events)
    }

    pub async fn send_message(&self, to: &str, content: &str) -> Result<()> {
        use tracing::{error, info};

        let url = format!("{}/api/v1/messages", self.site);

        info!("Sending message to: {}", to);
        info!("Message length: {} chars", content.len());

        // Zulip expects form data, not JSON
        let params = [("type", "private"), ("to", to), ("content", content)];

        let response = self
            .client
            .post(&url)
            .basic_auth(&self.email, Some(&self.api_key))
            .form(&params)
            .send()
            .await?;

        let status = response.status();
        info!("Response status: {}", status);

        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            error!(
                "Failed to send message. Status: {}, Body: {}",
                status, error_body
            );
            anyhow::bail!("Failed to send message: {} - {}", status, error_body);
        }

        info!("Message sent successfully to {}", to);
        Ok(())
    }

    /// Uploads a file and returns an absolute URL suitable for a markdown
    /// link in a message body. Zulip's response key varies by server
    /// version -- older servers return `uri`, newer ones `url` -- so both are
    /// checked.
    pub async fn upload_file(&self, filename: &str, bytes: Vec<u8>, mime: &str) -> Result<String> {
        let url = format!("{}/api/v1/user_uploads", self.site);

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let response = self
            .client
            .post(&url)
            .basic_auth(&self.email, Some(&self.api_key))
            .multipart(form)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to upload file: {} - {}", status, body);
        }

        let data: serde_json::Value = response.json().await?;
        let path = data["url"]
            .as_str()
            .or_else(|| data["uri"].as_str())
            .ok_or_else(|| anyhow::anyhow!("No url/uri in upload response: {}", data))?;

        Ok(if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{}", self.site, path)
        })
    }

    pub async fn download_file(&self, url: &str) -> Result<Vec<u8>> {
        let full_url = if url.starts_with("http") {
            url.to_string()
        } else {
            format!("{}{}", self.site, url)
        };

        let response = self
            .client
            .get(&full_url)
            .basic_auth(&self.email, Some(&self.api_key))
            .send()
            .await?;

        Ok(response.bytes().await?.to_vec())
    }
}
