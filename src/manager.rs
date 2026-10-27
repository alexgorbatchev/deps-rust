use crate::{
    CancellationToken, CommandRunner, Error, Installer, ProcessRunner, Result, compare_versions,
    default_version_parser, ensure_managed_bin_dir, is_agent_mode, managed_path, message,
};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub trait Cache: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn put(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}
#[derive(Default)]
pub struct MemoryCache(Mutex<HashMap<String, String>>);
impl Cache for MemoryCache {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| message("cache lock poisoned"))?
            .get(key)
            .cloned())
    }
    fn put(&self, key: &str, value: &str) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| message("cache lock poisoned"))?
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| message("cache lock poisoned"))?
            .remove(key);
        Ok(())
    }
}

pub type VersionParser = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;
pub struct Dependency {
    pub name: String,
    pub min_version: String,
    pub install_url: String,
    pub version_args: Vec<String>,
    pub parse_version: Option<VersionParser>,
    pub installer: Option<Arc<dyn Installer>>,
}
impl Dependency {
    pub fn new(name: impl Into<String>, min_version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            min_version: min_version.into(),
            install_url: String::new(),
            version_args: vec!["--version".into()],
            parse_version: None,
            installer: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyReport {
    pub name: String,
    pub min_version: String,
    pub detected_version: String,
    pub installed: bool,
    pub satisfied: bool,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub error: String,
}
impl DependencyReport {
    pub fn summary(&self) -> String {
        if self.satisfied {
            format!(
                "{} (version {}, min {})",
                self.name, self.detected_version, self.min_version
            )
        } else if self.installed && !self.detected_version.is_empty() {
            format!(
                "{} (installed {}, required >= {})",
                self.name, self.detected_version, self.min_version
            )
        } else {
            format!(
                "{} (not installed, required >= {})",
                self.name, self.min_version
            )
        }
    }
}
pub struct Config {
    pub app_name: String,
    pub dependencies: Vec<Dependency>,
    pub runner: Option<Arc<dyn CommandRunner>>,
    pub cache: Option<Arc<dyn Cache>>,
    /// Override the managed directory, useful for isolated installations.
    pub bin_dir: Option<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            app_name: "cli-deps".into(),
            dependencies: Vec::new(),
            runner: None,
            cache: None,
            bin_dir: None,
        }
    }
}
#[derive(Default, Debug, Clone, Copy)]
pub struct EnsureOptions {
    pub auto_install: bool,
}
#[derive(Debug)]
pub struct DependencyFailure {
    pub name: String,
    pub error: Error,
}
#[derive(Debug, Default)]
pub struct BatchOutcome {
    pub completed: Vec<String>,
    pub failures: Vec<DependencyFailure>,
}
impl BatchOutcome {
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }
    pub fn into_result(self) -> Result<Vec<String>> {
        if self.is_success() {
            Ok(self.completed)
        } else {
            Err(Error::Batch(self.failures.len()))
        }
    }
}
pub struct Manager {
    dependencies: Vec<Dependency>,
    runner: Arc<dyn CommandRunner>,
    cache: Option<Arc<dyn Cache>>,
    bin_dir: PathBuf,
}
impl Manager {
    pub fn new(config: Config) -> Result<Self> {
        let bin_dir = match config.bin_dir {
            Some(p) => {
                std::fs::create_dir_all(&p)?;
                p
            }
            None => ensure_managed_bin_dir(&config.app_name)?,
        };
        let mut names = std::collections::HashSet::new();
        for dependency in &config.dependencies {
            if dependency.name.trim().is_empty()
                || !names.insert(dependency.name.to_ascii_lowercase())
            {
                return Err(message("dependency names must be nonempty and unique"));
            }
        }
        let path = managed_path(&bin_dir, std::env::var_os("PATH").as_deref())?;
        Ok(Self {
            dependencies: config.dependencies,
            runner: config
                .runner
                .unwrap_or_else(|| Arc::new(ProcessRunner { path: Some(path) })),
            cache: config.cache,
            bin_dir,
        })
    }
    pub fn bin_dir(&self) -> &std::path::Path {
        &self.bin_dir
    }

    pub async fn verify(&self, token: &CancellationToken) -> Result<Vec<DependencyReport>> {
        let results = join_all(
            self.dependencies
                .iter()
                .map(|dependency| self.verify_one(dependency, token)),
        )
        .await;
        results.into_iter().collect()
    }
    async fn verify_one(
        &self,
        dependency: &Dependency,
        token: &CancellationToken,
    ) -> Result<DependencyReport> {
        if token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut report = DependencyReport {
            name: dependency.name.clone(),
            min_version: dependency.min_version.clone(),
            detected_version: String::new(),
            installed: false,
            satisfied: false,
            error: String::new(),
        };
        let key = cache_key(&dependency.name);
        let cached = self
            .cache
            .as_ref()
            .map(|c| c.get(&key))
            .transpose()?
            .flatten()
            .filter(|v| !v.is_empty());
        let output = if let Some(value) = cached {
            Ok(value.into_bytes())
        } else {
            let default_args = vec!["--version".into()];
            let args = if dependency.version_args.is_empty() {
                &default_args
            } else {
                &dependency.version_args
            };
            let result = self.runner.run(&dependency.name, args, token).await;
            if let (Some(cache), Ok(output)) = (&self.cache, &result)
                && !output.is_empty()
            {
                cache.put(&key, &String::from_utf8_lossy(output))?;
            }
            result
        };
        match output {
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                report.error = if is_agent_mode() {
                    format!(
                        "{} is missing in PATH; install version {} or newer",
                        dependency.name, dependency.min_version
                    )
                } else {
                    format!(
                        "{} is missing in PATH; install {}",
                        dependency.name, dependency.install_url
                    )
                }
            }
            Err(error) => {
                report.error = format!("failed to check {} version: {error}", dependency.name)
            }
            Ok(output) => {
                report.installed = true;
                let output = String::from_utf8_lossy(&output);
                report.detected_version = dependency.parse_version.as_ref().map_or_else(
                    || default_version_parser(&dependency.name, &output),
                    |parse| parse(&dependency.name, &output),
                );
                if report.detected_version.is_empty() {
                    report.error = format!("could not parse {} version output", dependency.name);
                } else if let Err(error) =
                    compare_versions(&report.detected_version, &dependency.min_version)
                {
                    report.error = error.to_string();
                } else {
                    report.satisfied = true;
                }
            }
        }
        Ok(report)
    }

    pub async fn install(&self, name: &str, token: &CancellationToken) -> Result<()> {
        self.mutate(name, false, token).await
    }
    pub async fn update(&self, name: &str, token: &CancellationToken) -> Result<()> {
        self.mutate(name, true, token).await
    }
    async fn mutate(&self, name: &str, update: bool, token: &CancellationToken) -> Result<()> {
        if token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let dependency = self
            .dependencies
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| message(format!("unknown dependency {name:?}")))?;
        let installer = dependency
            .installer
            .as_ref()
            .ok_or_else(|| message(format!("no installer configured for {name:?}")))?;
        if let Some(cache) = &self.cache {
            cache.delete(&cache_key(&dependency.name))?;
        }
        if update {
            installer.update(&self.bin_dir, token).await
        } else {
            installer.install(&self.bin_dir, token).await
        }
    }
    pub async fn install_unsatisfied(&self, token: &CancellationToken) -> Result<BatchOutcome> {
        let reports = self.verify(token).await?;
        let names = reports
            .into_iter()
            .filter(|r| !r.satisfied)
            .map(|r| r.name)
            .collect::<Vec<_>>();
        self.batch(names, false, token).await
    }
    pub async fn update_all(&self, token: &CancellationToken) -> Result<BatchOutcome> {
        self.batch(
            self.dependencies.iter().map(|d| d.name.clone()).collect(),
            true,
            token,
        )
        .await
    }
    async fn batch(
        &self,
        names: Vec<String>,
        update: bool,
        token: &CancellationToken,
    ) -> Result<BatchOutcome> {
        let mut outcome = BatchOutcome::default();
        for name in names {
            match self.mutate(&name, update, token).await {
                Ok(()) => outcome.completed.push(name),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => outcome.failures.push(DependencyFailure { name, error }),
            }
        }
        Ok(outcome)
    }
    /// Uses caller-owned input/output; EOF never grants installation consent.
    pub async fn ensure(
        &self,
        options: EnsureOptions,
        input: &mut (impl BufRead + Send),
        output: &mut (impl Write + Send),
        token: &CancellationToken,
    ) -> Result<()> {
        let reports = self.verify(token).await?;
        let unsatisfied = reports
            .iter()
            .filter(|r| !r.satisfied)
            .map(DependencyReport::summary)
            .collect::<Vec<_>>();
        if unsatisfied.is_empty() {
            return Ok(());
        }
        let details = unsatisfied.join(", ");
        let mut install = options.auto_install;
        if !install && !is_agent_mode() {
            write!(
                output,
                "Missing or outdated dependencies: {details}\nInstall into managed directory? [y/N]: "
            )?;
            output.flush()?;
            let mut answer = String::new();
            let count = input.read_line(&mut answer)?;
            install =
                count > 0 && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        }
        if !install {
            return Err(Error::Unsatisfied(details));
        }
        let result = self.install_unsatisfied(token).await?;
        if !result.is_success() {
            return Err(Error::Batch(result.failures.len()));
        }
        writeln!(output, "Installed: {}", result.completed.join(", "))?;
        let remaining = self
            .verify(token)
            .await?
            .into_iter()
            .filter(|r| !r.satisfied)
            .map(|r| r.summary())
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(Error::Unsatisfied(remaining.join(", ")))
        }
    }
}
fn cache_key(name: &str) -> String {
    format!("godeps_{name}.json")
}
