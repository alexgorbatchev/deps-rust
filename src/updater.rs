use crate::{CancellationToken, Error, ReleaseClient, Result, compare_versions, message};
use std::path::Path;

/// An explicitly configured release source. This constructor reads no environment.
#[derive(Clone, Debug)]
pub struct ReleaseChannel {
    pub client: ReleaseClient,
    pub owner: String,
    pub repository: String,
}
impl ReleaseChannel {
    pub fn new(
        base: &str,
        owner: impl Into<String>,
        repository: impl Into<String>,
    ) -> Result<Self> {
        let url =
            reqwest::Url::parse(base).map_err(|e| message(format!("release base URL: {e}")))?;
        let loopback = url.host().is_some_and(|host| match host {
            url::Host::Domain(domain) => domain == "localhost",
            url::Host::Ipv4(address) => address.is_loopback(),
            url::Host::Ipv6(address) => address.is_loopback(),
        });
        if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(message(
                "release base URL requires HTTPS (or loopback HTTP) without credentials, query, or fragment",
            ));
        }
        let owner = owner.into();
        let repository = repository.into();
        if [&owner, &repository].iter().any(|s| {
            s.is_empty()
                || !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                || matches!(s.as_str(), "." | "..")
        }) {
            return Err(message("invalid release owner or repository"));
        }
        Ok(Self {
            client: ReleaseClient::new(base)?,
            owner,
            repository,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Update {
    Current(String),
    Installed(String),
}
/// Updates only from an explicit channel; absence is a typed unpublished outcome.
#[derive(Default)]
pub struct Updater {
    pub channel: Option<ReleaseChannel>,
}
impl Updater {
    pub async fn update_to(
        &self,
        current: &str,
        destination: &Path,
        token: &CancellationToken,
    ) -> Result<Update> {
        let channel = self.channel.as_ref().ok_or(Error::Unpublished)?;
        let latest = channel
            .client
            .latest_tag(&channel.owner, &channel.repository, token)
            .await?;
        let current = current.trim_start_matches('v');
        if current != "dev"
            && !current.is_empty()
            && compare_versions(current, latest.trim_start_matches('v')).is_ok()
        {
            return Ok(Update::Current(current.into()));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| message("executable has no directory"))?;
        let binary = destination
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| message("invalid executable name"))?;
        channel
            .client
            .install_tag(
                &channel.owner,
                &channel.repository,
                binary,
                parent,
                &latest,
                token,
            )
            .await?;
        Ok(Update::Installed(latest.trim_start_matches('v').into()))
    }
    pub async fn update_running(&self, current: &str, token: &CancellationToken) -> Result<Update> {
        if self.channel.is_none() {
            return Err(Error::Unpublished);
        }
        let executable = std::fs::canonicalize(std::env::current_exe()?)?;
        let parent = executable
            .parent()
            .ok_or_else(|| message("executable has no directory"))?;
        let staged = tempfile::Builder::new()
            .prefix(".self-update-")
            .tempdir_in(parent)?;
        let target = staged.path().join(
            executable
                .file_name()
                .ok_or_else(|| message("invalid executable name"))?,
        );
        let result = self.update_to(current, &target, token).await?;
        if matches!(result, Update::Installed(_)) {
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self_replace::self_replace(target)?;
        }
        Ok(result)
    }
}
