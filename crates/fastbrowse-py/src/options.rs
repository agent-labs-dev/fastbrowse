//! `run_task`'s keyword arguments that are data, as `fastbrowse.native` writes them, and the `RunTask` they make.

use std::path::PathBuf;
use std::sync::Arc;

use fastbrowse_agent::verification::RecordSchema;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Attachment, Authorization, Bytes, Limits, LocalChrome, SecretRef};
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::protocol::RunAttachment;
use fastbrowse_run::RunTask;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub start: Option<String>,
    pub context: Option<String>,
    pub browser_api_key: Option<String>,
    pub chrome: Option<LocalChrome>,
    pub cloud_profile: Option<String>,
    pub cdp_url: Option<String>,
    pub cdp_port: Option<i64>,
    pub attach: bool,
    pub target_match: Option<String>,
    pub foreground: bool,
    pub allowed_origins: Option<Vec<String>>,
    pub proxy_country: Option<String>,
    pub viewport: Option<(i64, i64)>,
    pub cloud_allow_resizing: bool,
    pub cloud_extensions: Vec<String>,
    pub cloud_timeout_minutes: i64,
    /// The output model's JSON Schema.
    pub output_schema: Option<Value>,
    pub inputs: Option<IndexMap<String, String>>,
    pub attachments: Vec<RunAttachment>,
    pub limits: Option<Limits>,
    pub authorization: Option<Authorization>,
    /// What the caller's `SecretResolver` makes available, or None without one.
    pub secrets: Option<Vec<SecretRef>>,
    pub downloads: Option<PathBuf>,
    pub record: Option<PathBuf>,
    pub cursor: bool,
    pub fetch: bool,
    pub config: Option<Config>,
}

impl Options {
    /// The `RunTask` without its callbacks. An argument out of range fails as Python's `ValueError` would.
    pub fn task(self) -> Result<RunTask, String> {
        let out_of_range = |what: &str| format!("{what} is out of range");
        let cdp_port =
            self.cdp_port.map(|port| u16::try_from(port).map_err(|_| out_of_range("cdp_port"))).transpose()?;
        let viewport = self
            .viewport
            .map(|(width, height)| match (u32::try_from(width), u32::try_from(height)) {
                (Ok(width), Ok(height)) => Ok((width, height)),
                _ => Err(out_of_range("viewport")),
            })
            .transpose()?;
        let cloud_timeout_minutes =
            u32::try_from(self.cloud_timeout_minutes).map_err(|_| out_of_range("cloud_timeout_minutes"))?;
        let output_schema = match &self.output_schema {
            None => None,
            Some(schema) => Some(Arc::new(RecordSchema::from_output(
                output_model(schema).map_err(|error| format!("output_schema: {error}"))?,
            ))),
        };
        Ok(RunTask {
            start: self.start,
            context: self.context,
            browser_api_key: self.browser_api_key,
            chrome: self.chrome,
            cloud_profile: self.cloud_profile,
            cdp_url: self.cdp_url,
            cdp_port,
            attach: self.attach,
            target_match: self.target_match,
            foreground: self.foreground,
            allowed_origins: self.allowed_origins,
            proxy_country: self.proxy_country,
            viewport,
            cloud_allow_resizing: self.cloud_allow_resizing,
            cloud_extensions: self.cloud_extensions,
            cloud_timeout_minutes,
            output_schema,
            inputs: self.inputs.unwrap_or_default(),
            attachments: self
                .attachments
                .into_iter()
                .map(|attachment| Attachment {
                    name: attachment.name,
                    mime_type: attachment.mime_type,
                    content: Bytes(attachment.content.0),
                })
                .collect(),
            limits: self.limits,
            authorization: self.authorization,
            downloads: self.downloads,
            record: self.record,
            cursor: self.cursor,
            fetch: self.fetch,
            config: self.config,
            ..RunTask::default()
        })
    }
}
