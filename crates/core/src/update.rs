//! Update check against GitHub releases, plus SHA-256-verified download of
//! the installer. Never executes anything — the shell decides what to run.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const REPO: &str = "jichuo1/GlobalTokenTracker";
const SUMS_NAME: &str = "SHA256SUMS.txt";
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Alpha,
}

impl Channel {
    #[must_use]
    pub fn from_key(key: &str) -> Self {
        if key == "alpha" { Self::Alpha } else { Self::Stable }
    }

    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Alpha => "alpha",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub alpha: Option<u32>,
}

impl Version {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.');
        let major = parse_num(nums.next()?)?;
        let minor = parse_num(nums.next()?)?;
        let patch = parse_num(nums.next()?)?;
        if nums.next().is_some() {
            return None;
        }
        let alpha = match pre {
            None => None,
            Some(p) => Some(parse_num(p.strip_prefix("alpha.")?)?),
        };
        Some(Self { major, minor, patch, alpha })
    }
}

fn parse_num(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.alpha, other.alpha) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(&b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(n) = self.alpha {
            write!(f, "-alpha.{n}")?;
        }
        Ok(())
    }
}

#[must_use]
pub fn current_from(tag: Option<&str>, pkg: &str) -> Version {
    tag.filter(|t| !t.is_empty())
        .and_then(Version::parse)
        .or_else(|| Version::parse(pkg))
        .unwrap_or(Version { major: 0, minor: 0, patch: 0, alpha: None })
}

#[must_use]
/// Debug hooks: `GTT_UPDATE_AS=<tag>` overrides the running version;
/// `GTT_UPDATE_FAIL_DOWNLOAD=1` makes `download` fail before any network access.
pub fn current_version() -> Version {
    if let Some(v) = std::env::var("GTT_UPDATE_AS").ok().and_then(|s| Version::parse(&s)) {
        return v;
    }
    current_from(option_env!("GTT_RELEASE_TAG"), env!("CARGO_PKG_VERSION"))
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub html_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub sums_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Newest release on `channel` that is strictly newer than `current`.
pub fn select_update(
    releases_json: &str,
    channel: Channel,
    current: Version,
) -> Result<Option<Release>> {
    let all: Vec<GhRelease> = serde_json::from_str(releases_json).context("parse releases")?;
    let mut best: Option<Release> = None;
    for r in all {
        if r.draft {
            continue;
        }
        let Some(version) = Version::parse(&r.tag_name) else {
            continue;
        };
        if channel == Channel::Stable && (version.alpha.is_some() || r.prerelease) {
            continue;
        }
        let asset_name = format!("GlobalTokenTracker-Setup-{}-win-x64.exe", r.tag_name);
        let find = |name: &str| r.assets.iter().find(|a| a.name == name);
        let (Some(exe), Some(sums)) = (find(&asset_name), find(SUMS_NAME)) else {
            continue;
        };
        if best.as_ref().is_some_and(|b| b.version >= version) {
            continue;
        }
        best = Some(Release {
            tag: r.tag_name.clone(),
            version,
            html_url: r.html_url.clone(),
            asset_name,
            asset_url: exe.browser_download_url.clone(),
            sums_url: sums.browser_download_url.clone(),
        });
    }
    Ok(best.filter(|b| b.version > current))
}

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
        .build()
        .new_agent()
}

pub fn check(channel: Channel) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let mut resp = agent(15)
        .get(&url)
        .header("User-Agent", "GlobalTokenTracker")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(c @ (403 | 429)) => {
                anyhow!("GitHub API rate limit reached (HTTP {c}), try again later")
            }
            other => anyhow!("GET {url}: {other}"),
        })?;
    let body = resp.body_mut().read_to_string()?;
    select_update(&body, channel, current_version())
}

fn parse_sums(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (hash, rest) = line.trim().split_once(char::is_whitespace)?;
        let file = rest.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Download the installer into `%TEMP%\GlobalTokenTracker-update`, verified
/// against the release's `SHA256SUMS.txt`. Returns the verified file path.
pub fn download(rel: &Release) -> Result<PathBuf> {
    if std::env::var_os("GTT_UPDATE_FAIL_DOWNLOAD").is_some() {
        bail!("simulated download failure (GTT_UPDATE_FAIL_DOWNLOAD)");
    }
    let dir = std::env::temp_dir().join("GlobalTokenTracker-update");
    fs::create_dir_all(&dir)?;
    for e in fs::read_dir(&dir)?.flatten() {
        let _ = fs::remove_file(e.path());
    }
    let ag = agent(600);
    let sums = ag
        .get(&rel.sums_url)
        .header("User-Agent", "GlobalTokenTracker")
        .call()
        .context("download SHA256SUMS")?
        .body_mut()
        .read_to_string()?;
    let expected = parse_sums(&sums, &rel.asset_name)
        .ok_or_else(|| anyhow!("SHA256SUMS.txt has no entry for {}", rel.asset_name))?;

    let part = dir.join(format!("{}.part", rel.asset_name));
    let final_path = dir.join(&rel.asset_name);
    let result = (|| -> Result<()> {
        let mut resp = ag
            .get(&rel.asset_url)
            .header("User-Agent", "GlobalTokenTracker")
            .call()
            .context("download installer")?;
        let mut reader = resp.body_mut().as_reader().take(MAX_DOWNLOAD + 1);
        let mut out = fs::File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_DOWNLOAD {
                bail!("installer exceeds {} MB cap", MAX_DOWNLOAD / 1024 / 1024);
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        drop(out);
        let actual = hex(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(&expected) {
            bail!("SHA-256 mismatch (expected {expected}, got {actual})");
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    fs::rename(&part, &final_path)?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parse_accepts_and_rejects() {
        assert_eq!(v("v0.3.0"), Version { major: 0, minor: 3, patch: 0, alpha: None });
        assert_eq!(v("0.3.0"), v("v0.3.0"));
        assert_eq!(v("v0.3.0-alpha.2").alpha, Some(2));
        for bad in ["v0.3", "v0.3.0-beta.1", "v0.3.0-alpha", "", "v", "v0.3.0.1", "v0.3.x", "vv0.3.0"]
        {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn ordering_and_display() {
        let order = ["0.3.0-alpha.1", "0.3.0-alpha.2", "0.3.0", "0.3.1-alpha.1", "0.3.1"];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("0.3.0").to_string(), "v0.3.0");
        assert_eq!(v("v0.3.0-alpha.2").to_string(), "v0.3.0-alpha.2");
    }

    #[test]
    fn current_from_cases() {
        assert_eq!(current_from(Some(""), "0.3.0"), v("0.3.0"));
        assert_eq!(current_from(None, "0.3.0"), v("0.3.0"));
        assert_eq!(current_from(Some("garbage"), "0.3.0"), v("0.3.0"));
        assert_eq!(current_from(Some("v0.4.0-alpha.1"), "0.4.0").alpha, Some(1));
    }

    #[test]
    fn channel_keys() {
        assert_eq!(Channel::from_key("alpha"), Channel::Alpha);
        assert_eq!(Channel::from_key(""), Channel::Stable);
        assert_eq!(Channel::from_key("x"), Channel::Stable);
        assert_eq!(Channel::Alpha.key(), "alpha");
    }

    fn rel_json(tag: &str, draft: bool, pre: bool, exe: bool, sums: bool) -> String {
        let mut assets = Vec::new();
        if exe {
            assets.push(format!(
                r#"{{"name":"GlobalTokenTracker-Setup-{tag}-win-x64.exe","browser_download_url":"https://x/{tag}/exe"}}"#
            ));
        }
        if sums {
            assets.push(format!(
                r#"{{"name":"SHA256SUMS.txt","browser_download_url":"https://x/{tag}/sums"}}"#
            ));
        }
        format!(
            r#"{{"tag_name":"{tag}","draft":{draft},"prerelease":{pre},"html_url":"https://x/{tag}","assets":[{}]}}"#,
            assets.join(",")
        )
    }

    fn fixture(items: &[String]) -> String {
        format!("[{}]", items.join(","))
    }

    #[test]
    fn select_stable_ignores_alpha() {
        let j = fixture(&[
            rel_json("v0.4.0-alpha.1", false, true, true, true),
            rel_json("v0.3.5", false, true, true, true),
            rel_json("v0.3.1", false, false, true, true),
        ]);
        let r = select_update(&j, Channel::Stable, v("0.3.0")).unwrap().unwrap();
        assert_eq!(r.tag, "v0.3.1");
        assert_eq!(r.asset_url, "https://x/v0.3.1/exe");
        assert_eq!(r.sums_url, "https://x/v0.3.1/sums");
        assert_eq!(r.html_url, "https://x/v0.3.1");
        assert_eq!(r.asset_name, "GlobalTokenTracker-Setup-v0.3.1-win-x64.exe");
    }

    #[test]
    fn select_alpha_picks_highest_overall() {
        let j = fixture(&[
            rel_json("v0.3.1-alpha.2", false, true, true, true),
            rel_json("v0.3.1", false, false, true, true),
            rel_json("v0.3.1-alpha.1", false, true, true, true),
        ]);
        assert_eq!(select_update(&j, Channel::Alpha, v("0.3.0")).unwrap().unwrap().tag, "v0.3.1");
        let j = fixture(&[
            rel_json("v0.3.1-alpha.2", false, true, true, true),
            rel_json("v0.3.0", false, false, true, true),
        ]);
        assert_eq!(
            select_update(&j, Channel::Alpha, v("0.3.0")).unwrap().unwrap().tag,
            "v0.3.1-alpha.2"
        );
    }

    #[test]
    fn select_skips_unusable() {
        let j = fixture(&[
            rel_json("v0.9.0", true, false, true, true),
            rel_json("v0.8.0", false, false, false, true),
            rel_json("v0.7.0", false, false, true, false),
            rel_json("nightly", false, false, true, true),
            rel_json("v0.3.1", false, false, true, true),
        ]);
        assert_eq!(select_update(&j, Channel::Stable, v("0.3.0")).unwrap().unwrap().tag, "v0.3.1");
    }

    #[test]
    fn select_never_downgrades() {
        let j = fixture(&[rel_json("v0.3.0", false, false, true, true)]);
        assert!(select_update(&j, Channel::Stable, v("0.3.0")).unwrap().is_none());
        assert!(select_update(&j, Channel::Stable, v("0.3.0-alpha.5")).unwrap().is_some());
        let j = fixture(&[rel_json("v0.2.0", false, false, true, true)]);
        assert!(select_update(&j, Channel::Stable, v("0.3.1-alpha.1")).unwrap().is_none());
        assert!(select_update("[]", Channel::Alpha, v("0.3.0")).unwrap().is_none());
    }

    #[test]
    fn sums_parsing() {
        let h1 = "A".repeat(64);
        let h2 = "b".repeat(64);
        let t = format!("{h1}  one.exe\n{h2} *two.exe\r\nbad  three.exe\n");
        assert_eq!(parse_sums(&t, "one.exe").unwrap(), "a".repeat(64));
        assert_eq!(parse_sums(&t, "two.exe").unwrap(), h2);
        assert!(parse_sums(&t, "three.exe").is_none());
        assert!(parse_sums(&t, "four.exe").is_none());
        assert!(parse_sums(&t, "ne.exe").is_none());
    }
}
