use crate::{Error, Result};
use regex::Regex;
use std::sync::LazyLock;
use versions::Versioning;

static YT_DLP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{4}\.\d{2}\.\d{2}(?:\.\d+)?)\b").expect("valid pattern"));
static FFMPEG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"ffmpeg\s+version\s+n?([^\s]+)").expect("valid pattern"));

pub fn default_version_parser(name: &str, output: &str) -> String {
    let line = output.trim().lines().next().unwrap_or("").trim();
    let pattern = match name.to_ascii_lowercase().as_str() {
        "yt-dlp" => Some(&*YT_DLP),
        "ffmpeg" => Some(&*FFMPEG),
        _ => None,
    };
    if let Some(pattern) = pattern {
        return pattern
            .captures(line)
            .and_then(|c| c.get(1))
            .map_or(line, |v| v.as_str())
            .to_owned();
    }
    line.strip_prefix(name)
        .unwrap_or(line)
        .trim()
        .strip_prefix("version")
        .unwrap_or_else(|| line.strip_prefix(name).unwrap_or(line).trim())
        .trim()
        .trim_start_matches('v')
        .to_owned()
}

/// SemVer, calendar and segmented versions; development snapshots satisfy any minimum.
pub fn compare_versions(actual: &str, minimum: &str) -> Result<()> {
    let actual = actual.trim();
    let minimum = minimum.trim();
    if actual.is_empty()
        || matches!(actual, "dev" | "master" | "main")
        || actual.starts_with("git-")
        || actual.starts_with("N-")
    {
        return Ok(());
    }
    let normalize = |text: &str| {
        let text = text.strip_prefix('v').unwrap_or(text);
        let split = text.find(['-', '+']).unwrap_or(text.len());
        let mut base = text[..split].to_owned();
        if base.split('.').all(|s| s.parse::<u64>().is_ok()) {
            while base.split('.').count() < 3 {
                base.push_str(".0");
            }
        }
        base.push_str(&text[split..]);
        base
    };
    let older = match (
        Versioning::new(normalize(actual)),
        Versioning::new(normalize(minimum)),
    ) {
        (Some(a), Some(b)) => a < b,
        _ => {
            let digits = |v: &str| {
                v.split(['-', '+'])
                    .next()
                    .unwrap_or("")
                    .split('.')
                    .filter_map(|p| {
                        p.chars()
                            .take_while(char::is_ascii_digit)
                            .collect::<String>()
                            .parse::<u64>()
                            .ok()
                    })
                    .collect::<Vec<_>>()
            };
            let mut a = digits(actual);
            let mut b = digits(minimum);
            if a.is_empty() {
                return Ok(());
            }
            let length = a.len().max(b.len());
            a.resize(length, 0);
            b.resize(length, 0);
            a < b
        }
    };
    if older {
        Err(Error::VersionTooOld {
            actual: actual.to_owned(),
            minimum: minimum.to_owned(),
        })
    } else {
        Ok(())
    }
}
