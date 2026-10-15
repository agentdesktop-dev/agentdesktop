//! The proxy auto-config (PAC) set by browser policy.
//!
//! Browsers have no per-tool LLM setting, so organisations steer browser AI
//! traffic with a PAC file delivered by MDM or GPO. This module reports which
//! PAC each installed browser's machine policy sets, and fetches each PAC URL
//! once to report its identity (SHA-256 and size of the body). It never
//! evaluates the PAC, never reports its content and never writes browser
//! settings.
//!
//! Policy is read on Linux (JSON policy files) and Windows (registry, plus
//! Firefox `policies.json`). macOS (managed preferences) reports nothing yet.

#[cfg(any(target_os = "linux", windows, test))]
use std::path::Path;
use std::path::PathBuf;

use std::time::Duration;

use agentdesktop_core::model::{BrowserPac, PacFetch};
#[cfg(any(target_os = "linux", windows, test))]
use serde_json::Value;
use sha2::{Digest, Sha256};

/// At most this many distinct PAC URLs are fetched per scan.
const MAX_PAC_FETCHES: usize = 4;
/// A PAC is a small script; larger bodies are not hashed.
const MAX_PAC_BYTES: usize = 1024 * 1024;
const PAC_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// A fetched PAC's identity is reused for this long, so inventory refreshes
/// triggered by unrelated file changes don't hit the PAC server each time.
const PAC_FETCH_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

type PacFetchCache =
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, PacFetch)>>;

fn pac_fetch_cache() -> &'static PacFetchCache {
    static CACHE: std::sync::OnceLock<PacFetchCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Fetches each distinct PAC URL the browsers use and reports its identity.
///
/// Browsers fetch the PAC directly, so the system proxy settings are ignored
/// here too. The body is hashed and dropped; only `http` and `https` URLs
/// are fetched.
pub(crate) async fn fetch_pacs(browsers: &[BrowserPac]) -> Vec<PacFetch> {
    let mut urls: Vec<String> = browsers
        .iter()
        .filter_map(|browser| browser.pac_url.clone())
        .collect();
    urls.sort_unstable();
    urls.dedup();
    urls.truncate(MAX_PAC_FETCHES);
    let now = std::time::Instant::now();
    let mut fetches = Vec::new();
    let mut pending = Vec::new();
    {
        let cache = pac_fetch_cache().lock().expect("PAC fetch cache lock");
        for url in urls {
            match cache.get(&url) {
                Some((at, fetch)) if now.duration_since(*at) < PAC_FETCH_CACHE_TTL => {
                    fetches.push(fetch.clone());
                }
                _ => pending.push(url),
            }
        }
    }
    if !pending.is_empty() {
        let fresh = fetch_uncached(pending).await;
        let mut cache = pac_fetch_cache().lock().expect("PAC fetch cache lock");
        cache.retain(|_, (at, _)| now.duration_since(*at) < PAC_FETCH_CACHE_TTL);
        for fetch in &fresh {
            cache.insert(fetch.url.clone(), (now, fetch.clone()));
        }
        fetches.extend(fresh);
    }
    fetches.sort_by(|left, right| left.url.cmp(&right.url));
    fetches
}

/// Fetches the URLs concurrently, so a scan waits at most one timeout.
async fn fetch_uncached(urls: Vec<String>) -> Vec<PacFetch> {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(PAC_FETCH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return urls
                .into_iter()
                .map(|url| PacFetch {
                    url,
                    error: Some(crate::reconcile::truncate_detail(format!(
                        "build HTTP client: {error}"
                    ))),
                    ..PacFetch::default()
                })
                .collect();
        }
    };
    let mut tasks = tokio::task::JoinSet::new();
    for url in urls {
        let client = client.clone();
        tasks.spawn(async move {
            match fetch_pac(&client, &url).await {
                Ok((sha256, size)) => PacFetch {
                    url,
                    sha256: Some(sha256),
                    size: Some(size),
                    error: None,
                },
                Err(error) => PacFetch {
                    url,
                    error: Some(crate::reconcile::truncate_detail(error)),
                    ..PacFetch::default()
                },
            }
        });
    }
    let mut fetches = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok(fetch) = result {
            fetches.push(fetch);
        }
    }
    fetches
}

async fn fetch_pac(client: &reqwest::Client, url: &str) -> Result<(String, u64), String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("not fetched: {} URL", parsed.scheme()));
    }
    let mut response = client
        .get(parsed)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.without_url().to_string())?;
    let mut hasher = Sha256::new();
    let mut size = 0usize;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error.without_url().to_string())?
    {
        size += chunk.len();
        if size > MAX_PAC_BYTES {
            return Err(format!("larger than {MAX_PAC_BYTES} bytes"));
        }
        hasher.update(&chunk);
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((sha256, size as u64))
}

/// The browsers' PAC policy on this device.
///
/// `user_policy`: read per-user policy (Windows `HKCU`) too. Only a daemon
/// running as the user sees that user's hive; a service would read its own.
pub(crate) fn discover(user_policy: bool) -> Vec<BrowserPac> {
    #[cfg(target_os = "linux")]
    {
        let _ = user_policy;
        linux::discover(Path::new("/"))
    }
    #[cfg(windows)]
    {
        windows_policy::discover(
            &win_registry::WinRegistry,
            windows_policy::existing_path,
            user_policy,
        )
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = user_policy;
        Vec::new()
    }
}

/// Directories holding the policy files [`discover`] reads, for file watching.
pub(crate) fn policy_directories() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        linux::policy_directories(Path::new("/"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// The proxy settings one policy source sets, if any.
#[cfg(any(target_os = "linux", windows, test))]
#[derive(Debug, PartialEq)]
struct ProxyPolicy {
    pac_url: Option<String>,
}

/// Chromium policy: `ProxySettings` (current), else the legacy top-level
/// `ProxyMode` and `ProxyPacUrl`. A PAC URL counts only with
/// `ProxyMode: pac_script`; Chrome ignores `ProxyPacUrl` otherwise.
#[cfg(any(target_os = "linux", windows, test))]
fn chromium_proxy(policy: &Value) -> Option<ProxyPolicy> {
    let settings = policy.get("ProxySettings").unwrap_or(policy);
    let mode = settings.get("ProxyMode").and_then(Value::as_str);
    let url = settings.get("ProxyPacUrl").and_then(Value::as_str);
    if mode.is_none() && url.is_none() {
        return None;
    }
    Some(ProxyPolicy {
        pac_url: (mode == Some("pac_script"))
            .then_some(url)
            .flatten()
            .filter(|url| !url.trim().is_empty())
            .map(str::to_owned),
    })
}

/// Firefox `policies.json`: the PAC URL (only with `Mode: autoConfig`) and
/// `Locked`, when the file sets `Proxy` at all.
#[cfg(any(target_os = "linux", windows, test))]
fn firefox_proxy(policies: &Value) -> Option<(Option<String>, bool)> {
    let proxy = policies.get("policies")?.get("Proxy")?;
    let mode = proxy.get("Mode").and_then(Value::as_str);
    let pac_url = (mode == Some("autoConfig"))
        .then(|| proxy.get("AutoConfigURL").and_then(Value::as_str))
        .flatten()
        .filter(|url| !url.trim().is_empty())
        .map(str::to_owned);
    let locked = proxy
        .get("Locked")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some((pac_url, locked))
}

/// Chromium policy JSON: Chromium's loader accepts comments and trailing
/// commas (`JSON_PARSE_CHROMIUM_EXTENSIONS | JSON_ALLOW_TRAILING_COMMAS`).
#[cfg(any(target_os = "linux", test))]
fn read_chromium_json(file: &Path) -> Result<Value, String> {
    let contents = std::fs::read_to_string(file)
        .map_err(|error| format!("read {}: {error}", file.display()))?;
    serde_json::from_str(&strip_jsonc(&contents))
        .map_err(|error| format!("parse {}: {error}", file.display()))
}

/// Removes `//` and `/* */` comments, then trailing commas, outside
/// strings, in two scans; a comma looks ahead only past whitespace.
#[cfg(any(target_os = "linux", test))]
fn strip_jsonc(input: &str) -> String {
    let mut uncommented = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            uncommented.push(c);
            match c {
                '\\' => uncommented.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('/', Some('/')) => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        uncommented.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                uncommented.push(' ');
            }
            _ => {
                in_string = c == '"';
                uncommented.push(c);
            }
        }
    }

    let chars: Vec<char> = uncommented.chars().collect();
    let mut out = String::with_capacity(uncommented.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.get(index + 1) {
                    out.push(*next);
                    index += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
        } else if c == ','
            && matches!(
                chars[index + 1..].iter().find(|next| !next.is_whitespace()),
                Some('}' | ']')
            )
        {
            // trailing comma: dropped
        } else {
            in_string = c == '"';
            out.push(c);
        }
        index += 1;
    }
    out
}

#[cfg(any(target_os = "linux", windows, test))]
fn read_json(file: &Path) -> Result<Value, String> {
    let contents = std::fs::read_to_string(file)
        .map_err(|error| format!("read {}: {error}", file.display()))?;
    serde_json::from_str(&contents).map_err(|error| format!("parse {}: {error}", file.display()))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// A Chromium-family browser: install markers and policy roots, each with
    /// `managed/` and `recommended/` subdirectories of JSON files.
    struct Chromium {
        name: &'static str,
        installs: &'static [(&'static str, &'static str)],
        policy_roots: &'static [&'static str],
    }

    const CHROMIUM_FAMILY: &[Chromium] = &[
        Chromium {
            name: "chrome",
            installs: &[("opt/google/chrome/chrome", "system")],
            policy_roots: &["etc/opt/chrome/policies"],
        },
        Chromium {
            name: "edge",
            installs: &[("opt/microsoft/msedge/msedge", "system")],
            policy_roots: &["etc/opt/edge/policies"],
        },
        Chromium {
            name: "chromium",
            installs: &[
                ("snap/bin/chromium", "snap"),
                ("var/lib/flatpak/app/org.chromium.Chromium", "flatpak"),
                ("usr/bin/chromium", "system"),
                ("usr/bin/chromium-browser", "system"),
            ],
            // The snap reads /etc/chromium-browser/policies.
            policy_roots: &["etc/chromium/policies", "etc/chromium-browser/policies"],
        },
    ];

    const FIREFOX_INSTALLS: &[(&str, &str)] = &[
        ("snap/bin/firefox", "snap"),
        ("var/lib/flatpak/app/org.mozilla.firefox", "flatpak"),
        ("usr/lib/firefox/firefox", "system"),
        ("usr/lib64/firefox/firefox", "system"),
        ("opt/firefox/firefox", "system"),
    ];

    /// Firefox policy files in Firefox's own order: it uses
    /// `/etc/firefox/policies/policies.json` when that file exists (read by
    /// the deb and the snap) and otherwise the install's `distribution/`
    /// file, never both.
    const FIREFOX_POLICY_FILES: &[&str] = &[
        "etc/firefox/policies/policies.json",
        "usr/lib/firefox/distribution/policies.json",
        "usr/lib64/firefox/distribution/policies.json",
        "opt/firefox/distribution/policies.json",
    ];

    pub(super) fn discover(root: &Path) -> Vec<BrowserPac> {
        let mut browsers: Vec<BrowserPac> = CHROMIUM_FAMILY
            .iter()
            .filter_map(|browser| chromium(root, browser))
            .collect();
        browsers.extend(firefox(root));
        browsers
    }

    pub(super) fn policy_directories(root: &Path) -> Vec<PathBuf> {
        let mut directories: Vec<PathBuf> = CHROMIUM_FAMILY
            .iter()
            .flat_map(|browser| browser.policy_roots.iter())
            .flat_map(|policies| {
                ["managed", "recommended"].map(|level| root.join(policies).join(level))
            })
            .collect();
        directories.extend(
            FIREFOX_POLICY_FILES
                .iter()
                .filter_map(|file| root.join(file).parent().map(Path::to_path_buf)),
        );
        // A directory an MDM creates later is caught through its nearest
        // existing parent below /etc; the rescan then watches it directly.
        let stop = [root.join("etc"), root.to_path_buf()];
        let mut watched: Vec<PathBuf> = directories
            .into_iter()
            .filter_map(|directory| {
                directory
                    .ancestors()
                    .find(|ancestor| ancestor.is_dir())
                    .filter(|existing| !stop.iter().any(|stop| stop == existing))
                    .map(Path::to_path_buf)
            })
            .collect();
        watched.sort();
        watched.dedup();
        watched
    }

    fn packaging(root: &Path, installs: &[(&str, &'static str)]) -> Option<&'static str> {
        installs
            .iter()
            .find(|(path, _)| root.join(path).exists())
            .map(|(_, packaging)| *packaging)
    }

    fn chromium(root: &Path, browser: &Chromium) -> Option<BrowserPac> {
        let packaging = packaging(root, browser.installs);
        let mut row = BrowserPac {
            browser: browser.name.to_owned(),
            packaging: packaging.unwrap_or_default().to_owned(),
            ..BrowserPac::default()
        };
        // Managed policy wins; recommended policy is user-overridable.
        let mut found_policy = false;
        let mut unreadable = Vec::new();
        for (level, recommended) in [("managed", false), ("recommended", true)] {
            let directories: Vec<PathBuf> = browser
                .policy_roots
                .iter()
                .map(|policies| root.join(policies).join(level))
                .collect();
            match chromium_level(&directories, &mut unreadable) {
                Ok(None) => continue,
                Ok(Some((policy, source))) => {
                    row.pac_url = policy.pac_url;
                    row.recommended = recommended;
                    row.source = Some(source);
                }
                Err(detail) => row.detail = Some(detail),
            }
            found_policy = true;
            break;
        }
        // Chrome skips policy files it can't parse; report them without
        // letting them hide a valid proxy policy in another file.
        if !unreadable.is_empty() && row.detail.is_none() {
            row.detail = Some(format!("skipped {}", unreadable.join("; ")));
            found_policy = true;
        }
        row.detail = row.detail.map(crate::reconcile::truncate_detail);
        (packaging.is_some() || found_policy).then_some(row)
    }

    /// Reads every `*.json` in the directories of one level and merges the
    /// proxy keys across files the way Chrome merges policies key by key
    /// (`ProxyMode` in one file and `ProxyPacUrl` in another still give a
    /// PAC). The same key with different values in two files is reported as
    /// a conflict rather than guessing Chrome's merge order. Unreadable files
    /// and directories are collected in `unreadable` and skipped.
    fn chromium_level(
        directories: &[PathBuf],
        unreadable: &mut Vec<String>,
    ) -> Result<Option<(ProxyPolicy, PathBuf)>, String> {
        const KEYS: [&str; 3] = ["ProxySettings", "ProxyMode", "ProxyPacUrl"];
        let mut merged: Vec<(&str, Value, PathBuf)> = Vec::new();
        let mut conflicts: Vec<(&str, PathBuf, PathBuf)> = Vec::new();
        for directory in directories {
            let entries = match std::fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    unreadable.push(format!("read {}: {error}", directory.display()));
                    continue;
                }
            };
            let mut files: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
                .collect();
            files.sort();
            for file in files {
                let value = match read_chromium_json(&file) {
                    Ok(value) => value,
                    Err(error) => {
                        unreadable.push(error);
                        continue;
                    }
                };
                for key in KEYS {
                    let Some(setting) = value.get(key) else {
                        continue;
                    };
                    match merged.iter().find(|(existing, _, _)| *existing == key) {
                        Some((_, existing, first)) if existing != setting => {
                            conflicts.push((key, first.clone(), file.clone()));
                        }
                        Some(_) => {}
                        None => merged.push((key, setting.clone(), file.clone())),
                    }
                }
            }
        }
        // ProxySettings overrides the legacy keys, so their conflicts only
        // matter when it isn't set (or conflicts itself).
        let settings_in_effect = merged.iter().any(|(key, _, _)| *key == "ProxySettings")
            && !conflicts.iter().any(|(key, _, _)| *key == "ProxySettings");
        if let Some((_, first, second)) = conflicts
            .iter()
            .find(|(key, _, _)| !settings_in_effect || *key == "ProxySettings")
        {
            return Err(format!(
                "conflicting proxy policy in {} and {}",
                first.display(),
                second.display()
            ));
        }
        // The source shown is the file that set the PAC URL, if any.
        let Some(source) = ["ProxySettings", "ProxyPacUrl", "ProxyMode"]
            .iter()
            .find_map(|key| merged.iter().find(|(existing, _, _)| existing == key))
            .map(|(_, _, file)| file.clone())
        else {
            return Ok(None);
        };
        let policy = Value::Object(
            merged
                .into_iter()
                .map(|(key, setting, _)| (key.to_owned(), setting))
                .collect(),
        );
        Ok(chromium_proxy(&policy).map(|policy| (policy, source)))
    }

    fn firefox(root: &Path) -> Option<BrowserPac> {
        let packaging = packaging(root, FIREFOX_INSTALLS);
        let mut row = BrowserPac {
            browser: "firefox".to_owned(),
            packaging: packaging.unwrap_or_default().to_owned(),
            ..BrowserPac::default()
        };
        let mut found_policy = false;
        // Only the file Firefox uses; a Proxy section in a file it ignores
        // must not count.
        if let Some(file) = FIREFOX_POLICY_FILES
            .iter()
            .map(|file| root.join(file))
            .find(|file| file.is_file())
        {
            match read_json(&file) {
                Ok(value) => {
                    if let Some((pac_url, locked)) = firefox_proxy(&value) {
                        row.pac_url = pac_url;
                        row.locked = locked;
                        row.source = Some(file);
                        found_policy = true;
                    }
                }
                Err(detail) => {
                    row.detail = Some(crate::reconcile::truncate_detail(detail));
                    found_policy = true;
                }
            }
        }
        (packaging.is_some() || found_policy).then_some(row)
    }

    #[cfg(test)]
    mod tests {
        use std::fs;

        use super::*;

        fn temp_root(name: &str) -> PathBuf {
            let root = std::env::temp_dir().join(format!(
                "agentdesktop-browser-pac-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            root
        }

        fn write(root: &Path, path: &str, contents: &str) {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        const PAC: &str = "http://127.0.0.1:8088/ai-egress.pac";

        #[test]
        fn lab_policies_report_the_pac_for_chrome_and_firefox() {
            // The files placed on the lab VM (MDM stand-in).
            let root = temp_root("lab");
            write(&root, "opt/google/chrome/chrome", "");
            write(&root, "snap/bin/firefox", "");
            write(
                &root,
                "etc/opt/chrome/policies/managed/agentdesktop-egress.json",
                &format!(
                    r#"{{"ProxySettings":{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}},"QuicAllowed":false}}"#
                ),
            );
            write(
                &root,
                "etc/firefox/policies/policies.json",
                &format!(
                    r#"{{"policies":{{"Proxy":{{"Mode":"autoConfig","AutoConfigURL":"{PAC}","Locked":true}}}}}}"#
                ),
            );
            let browsers = discover(&root);
            assert_eq!(browsers.len(), 2);
            let chrome = &browsers[0];
            assert_eq!(chrome.browser, "chrome");
            assert_eq!(chrome.packaging, "system");
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
            assert!(!chrome.recommended);
            assert_eq!(
                chrome.source.as_deref(),
                Some(
                    root.join("etc/opt/chrome/policies/managed/agentdesktop-egress.json")
                        .as_path()
                )
            );
            let firefox = &browsers[1];
            assert_eq!(firefox.browser, "firefox");
            assert_eq!(firefox.packaging, "snap");
            assert_eq!(firefox.pac_url.as_deref(), Some(PAC));
            assert!(firefox.locked);
            assert_eq!(
                agentdesktop_core::model::DevicePac::from_browsers(&browsers),
                agentdesktop_core::model::DevicePac::Found(PAC.to_owned())
            );
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn installed_browser_without_policy_has_no_pac() {
            let root = temp_root("nopolicy");
            write(&root, "opt/google/chrome/chrome", "");
            let browsers = discover(&root);
            assert_eq!(browsers.len(), 1);
            assert_eq!(browsers[0].pac_url, None);
            assert_eq!(browsers[0].source, None);
            assert!(discover(&temp_root("empty")).is_empty());
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn recommended_policy_is_flagged_and_managed_wins() {
            let root = temp_root("levels");
            write(
                &root,
                "etc/opt/chrome/policies/recommended/p.json",
                r#"{"ProxyMode":"pac_script","ProxyPacUrl":"https://user.example/p.pac"}"#,
            );
            let browsers = discover(&root);
            assert!(browsers[0].recommended);
            assert_eq!(
                browsers[0].pac_url.as_deref(),
                Some("https://user.example/p.pac")
            );
            write(
                &root,
                "etc/opt/chrome/policies/managed/p.json",
                &format!(
                    r#"{{"ProxySettings":{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}}}"#
                ),
            );
            let browsers = discover(&root);
            assert!(!browsers[0].recommended);
            assert_eq!(browsers[0].pac_url.as_deref(), Some(PAC));
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn non_pac_modes_and_conflicts_report_no_pac_url() {
            let root = temp_root("modes");
            // Fixed servers: proxy policy, but no PAC.
            write(
                &root,
                "etc/opt/edge/policies/managed/a.json",
                r#"{"ProxySettings":{"ProxyMode":"fixed_servers","ProxyServer":"agw:3128"}}"#,
            );
            // A PAC URL without pac_script mode is ignored by Chrome.
            write(
                &root,
                "etc/chromium-browser/policies/managed/a.json",
                &format!(r#"{{"ProxyPacUrl":"{PAC}"}}"#),
            );
            // Two managed files with different PAC URLs.
            write(
                &root,
                "etc/opt/chrome/policies/managed/a.json",
                &format!(r#"{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}"#),
            );
            write(
                &root,
                "etc/opt/chrome/policies/managed/b.json",
                r#"{"ProxyMode":"pac_script","ProxyPacUrl":"https://other.example/p.pac"}"#,
            );
            // Firefox with a system proxy instead of a PAC.
            write(
                &root,
                "etc/firefox/policies/policies.json",
                r#"{"policies":{"Proxy":{"Mode":"system"}}}"#,
            );
            let browsers = discover(&root);
            let by_name = |name: &str| browsers.iter().find(|row| row.browser == name).unwrap();
            assert_eq!(by_name("edge").pac_url, None);
            assert!(by_name("edge").source.is_some());
            assert_eq!(by_name("chromium").pac_url, None);
            let chrome = by_name("chrome");
            assert_eq!(chrome.pac_url, None);
            assert!(
                chrome
                    .detail
                    .as_deref()
                    .unwrap()
                    .contains("conflicting proxy policy")
            );
            assert_eq!(by_name("firefox").pac_url, None);
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn chromium_policy_files_may_have_comments_and_trailing_commas() {
            let root = temp_root("jsonc");
            write(
                &root,
                "etc/opt/chrome/policies/managed/egress.json",
                &format!(
                    "// AI egress\n{{\n  /* PAC */ \"ProxySettings\": {{\"ProxyMode\": \"pac_script\", \"ProxyPacUrl\": \"{PAC}\",}},\n}}\n"
                ),
            );
            assert_eq!(discover(&root)[0].pac_url.as_deref(), Some(PAC));
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn strip_jsonc_keeps_strings_intact() {
            assert_eq!(
                strip_jsonc(
                    r#"{"u": "http://a/b,}", /* c */ "v": [1, 2,], // x
}"#
                ),
                "{\"u\": \"http://a/b,}\",   \"v\": [1, 2] \n}"
            );
            assert_eq!(strip_jsonc(r#"{"q": "a\"//b"}"#), r#"{"q": "a\"//b"}"#);
        }

        #[test]
        fn proxy_settings_wins_over_conflicting_legacy_keys() {
            let root = temp_root("precedence");
            write(
                &root,
                "etc/opt/chrome/policies/managed/a.json",
                r#"{"ProxyMode":"pac_script","ProxyPacUrl":"https://old.example/a.pac"}"#,
            );
            write(
                &root,
                "etc/opt/chrome/policies/managed/b.json",
                r#"{"ProxyPacUrl":"https://old.example/b.pac"}"#,
            );
            write(
                &root,
                "etc/opt/chrome/policies/managed/c.json",
                &format!(
                    r#"{{"ProxySettings":{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}}}"#
                ),
            );
            let chrome = &discover(&root)[0];
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
            assert_eq!(chrome.detail, None);
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn firefox_reads_only_the_file_it_uses() {
            let root = temp_root("ffactive");
            write(&root, "usr/lib/firefox/firefox", "");
            // /etc exists without Proxy: the distribution file is ignored.
            write(
                &root,
                "etc/firefox/policies/policies.json",
                r#"{"policies":{"DisableAppUpdate":true}}"#,
            );
            write(
                &root,
                "usr/lib/firefox/distribution/policies.json",
                &format!(
                    r#"{{"policies":{{"Proxy":{{"Mode":"autoConfig","AutoConfigURL":"{PAC}","Locked":true}}}}}}"#
                ),
            );
            let firefox = discover(&root)
                .into_iter()
                .find(|row| row.browser == "firefox")
                .unwrap();
            assert_eq!(firefox.pac_url, None);
            assert!(!firefox.locked);
            fs::remove_file(root.join("etc/firefox/policies/policies.json")).unwrap();
            let firefox = discover(&root)
                .into_iter()
                .find(|row| row.browser == "firefox")
                .unwrap();
            assert_eq!(firefox.pac_url.as_deref(), Some(PAC));
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn legacy_keys_split_across_files_are_merged() {
            let root = temp_root("split");
            write(
                &root,
                "etc/opt/chrome/policies/managed/a.json",
                r#"{"ProxyMode":"pac_script"}"#,
            );
            write(
                &root,
                "etc/opt/chrome/policies/managed/b.json",
                &format!(r#"{{"ProxyPacUrl":"{PAC}"}}"#),
            );
            let chrome = &discover(&root)[0];
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
            assert_eq!(
                chrome.source.as_deref(),
                Some(
                    root.join("etc/opt/chrome/policies/managed/b.json")
                        .as_path()
                )
            );
            assert_eq!(chrome.detail, None);
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn an_unreadable_policy_directory_is_reported() {
            use std::os::unix::fs::PermissionsExt;
            let root = temp_root("noaccess");
            let managed = root.join("etc/opt/chrome/policies/managed");
            write(&root, "etc/opt/chrome/policies/managed/p.json", "{}");
            fs::set_permissions(&managed, fs::Permissions::from_mode(0o000)).unwrap();
            let readable = fs::read_dir(&managed).is_ok(); // root ignores modes
            let chrome = discover(&root).into_iter().next();
            fs::set_permissions(&managed, fs::Permissions::from_mode(0o755)).unwrap();
            if !readable {
                let chrome = chrome.expect("reported");
                assert_eq!(chrome.pac_url, None);
                assert!(chrome.detail.as_deref().unwrap().contains("read "));
            }
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn an_unreadable_file_does_not_hide_a_valid_proxy_policy() {
            let root = temp_root("mixed");
            write(
                &root,
                "etc/opt/chrome/policies/managed/a-broken.json",
                "{not json",
            );
            write(
                &root,
                "etc/opt/chrome/policies/managed/egress.json",
                &format!(r#"{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}"#),
            );
            let chrome = &discover(&root)[0];
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
            assert!(
                chrome
                    .detail
                    .as_deref()
                    .unwrap()
                    .starts_with("skipped parse")
            );
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn unreadable_policy_is_reported_not_fatal() {
            let root = temp_root("broken");
            write(
                &root,
                "etc/opt/chrome/policies/managed/broken.json",
                "{not json",
            );
            write(&root, "etc/firefox/policies/policies.json", "[");
            let browsers = discover(&root);
            assert_eq!(browsers.len(), 2);
            assert!(
                browsers
                    .iter()
                    .all(|row| row.pac_url.is_none() && row.detail.is_some())
            );
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn policy_directories_are_the_existing_ones() {
            let root = temp_root("dirs");
            write(&root, "etc/opt/chrome/policies/managed/a.json", "{}");
            write(&root, "etc/firefox/policies/policies.json", "{}");
            assert_eq!(
                policy_directories(&root),
                vec![
                    // existing parent of recommended/ and of edge's tree
                    root.join("etc/firefox/policies"),
                    root.join("etc/opt"),
                    root.join("etc/opt/chrome/policies"),
                    root.join("etc/opt/chrome/policies/managed"),
                ]
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(test)]
mod fetch_tests {
    use axum::{Router, routing::get};

    use super::*;

    fn browser(pac_url: &str) -> BrowserPac {
        BrowserPac {
            browser: "chrome".to_owned(),
            pac_url: Some(pac_url.to_owned()),
            ..BrowserPac::default()
        }
    }

    #[tokio::test]
    async fn a_fresh_fetch_is_reused_from_the_cache() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/cached.pac",
                    get(move || {
                        counter.fetch_add(1, Ordering::SeqCst);
                        async { "function FindProxyForURL() { return \"DIRECT\"; }" }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let url = format!("http://{address}/cached.pac");
        let first = fetch_pacs(&[browser(&url)]).await;
        let second = fetch_pacs(&[browser(&url)]).await;
        assert_eq!(first, second);
        assert!(first[0].sha256.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fetches_each_distinct_pac_once_and_reports_its_hash() {
        const PAC: &str = "function FindProxyForURL(url, host) { return \"DIRECT\"; }\n";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/ai.pac", get(|| async { PAC }))
                    .route("/big.pac", get(|| async { "x".repeat(MAX_PAC_BYTES + 1) })),
            )
            .await
            .unwrap();
        });
        let ok = format!("http://{address}/ai.pac");
        let missing = format!("http://{address}/missing.pac");
        let big = format!("http://{address}/big.pac");
        let fetches = fetch_pacs(&[
            browser(&ok),
            browser(&ok),
            browser(&missing),
            browser(&big),
            browser("file:///etc/ai.pac"),
        ])
        .await;
        let by_url = |url: &str| fetches.iter().find(|fetch| fetch.url == url).unwrap();
        assert_eq!(fetches.len(), 4);
        let expected: String = Sha256::digest(PAC.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(by_url(&ok).sha256.as_deref(), Some(expected.as_str()));
        assert_eq!(by_url(&ok).size, Some(PAC.len() as u64));
        assert!(by_url(&missing).error.as_deref().unwrap().contains("404"));
        assert!(
            by_url(&big)
                .error
                .as_deref()
                .unwrap()
                .contains("larger than")
        );
        assert_eq!(
            by_url("file:///etc/ai.pac").error.as_deref(),
            Some("not fetched: file URL")
        );
        assert!(fetch_pacs(&[]).await.is_empty());
    }
}

/// Windows: Chromium-family and Firefox policy from the registry, and Firefox
/// `policies.json`. The logic sits behind [`windows_policy::Registry`] so it
/// is tested on every platform; only [`win_registry`] touches the registry.
#[cfg(any(windows, test))]
mod windows_policy {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) enum Hive {
        LocalMachine,
        CurrentUser,
    }

    impl Hive {
        fn prefix(self) -> &'static str {
            match self {
                Self::LocalMachine => "HKLM",
                Self::CurrentUser => "HKCU",
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum RegValue {
        String(String),
        Dword(u32),
    }

    /// Read access to registry values (`REG_SZ`/`REG_EXPAND_SZ` and `REG_DWORD`).
    pub(super) trait Registry {
        fn value(&self, hive: Hive, key: &str, name: &str) -> Option<RegValue>;
    }

    /// A path below a known folder: (environment variable, relative path).
    type KnownPath = (&'static str, &'static str);

    struct Chromium {
        name: &'static str,
        installs: &'static [KnownPath],
        policy_key: &'static str,
    }

    // %LOCALAPPDATA% is the daemon's own profile: a per-user install is seen
    // by a daemon running as that user, not by the service.
    const CHROMIUM_FAMILY: &[Chromium] = &[
        Chromium {
            name: "chrome",
            installs: &[
                ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
                ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
                ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
            ],
            policy_key: r"SOFTWARE\Policies\Google\Chrome",
        },
        Chromium {
            name: "edge",
            installs: &[
                (
                    "ProgramFiles(x86)",
                    r"Microsoft\Edge\Application\msedge.exe",
                ),
                ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
            ],
            policy_key: r"SOFTWARE\Policies\Microsoft\Edge",
        },
    ];

    const FIREFOX_INSTALLS: &[KnownPath] = &[
        ("ProgramFiles", r"Mozilla Firefox\firefox.exe"),
        ("ProgramFiles(x86)", r"Mozilla Firefox\firefox.exe"),
    ];
    const FIREFOX_POLICY_KEY: &str = r"SOFTWARE\Policies\Mozilla\Firefox\Proxy";
    const FIREFOX_POLICY_FILES: &[KnownPath] = &[
        (
            "ProgramFiles",
            r"Mozilla Firefox\distribution\policies.json",
        ),
        (
            "ProgramFiles(x86)",
            r"Mozilla Firefox\distribution\policies.json",
        ),
    ];

    /// Machine policy first, then user policy; managed before recommended
    /// (the `Recommended` subkey is user-overridable).
    const SEARCH: &[(Hive, bool)] = &[
        (Hive::LocalMachine, false),
        (Hive::CurrentUser, false),
        (Hive::LocalMachine, true),
        (Hive::CurrentUser, true),
    ];

    /// The existing file at a known path, resolved from the environment.
    #[cfg(windows)]
    pub(super) fn existing_path(known: KnownPath) -> Option<PathBuf> {
        let path = PathBuf::from(std::env::var_os(known.0)?).join(known.1);
        path.exists().then_some(path)
    }

    /// `user_policy`: also read `HKCU`. Only meaningful when the daemon runs
    /// as the signed-in user; a service's `HKCU` is the service account's.
    pub(super) fn discover(
        registry: &impl Registry,
        existing: impl Fn(KnownPath) -> Option<PathBuf>,
        user_policy: bool,
    ) -> Vec<BrowserPac> {
        let hives: &[Hive] = if user_policy {
            &[Hive::LocalMachine, Hive::CurrentUser]
        } else {
            &[Hive::LocalMachine]
        };
        let mut browsers: Vec<BrowserPac> = CHROMIUM_FAMILY
            .iter()
            .filter_map(|browser| chromium(registry, &existing, hives, browser))
            .collect();
        browsers.extend(firefox(registry, &existing, hives));
        browsers
    }

    fn chromium(
        registry: &impl Registry,
        existing: &impl Fn(KnownPath) -> Option<PathBuf>,
        hives: &[Hive],
        browser: &Chromium,
    ) -> Option<BrowserPac> {
        let installed = browser
            .installs
            .iter()
            .any(|known| existing(*known).is_some());
        let mut row = BrowserPac {
            browser: browser.name.to_owned(),
            packaging: if installed {
                "system".to_owned()
            } else {
                String::new()
            },
            ..BrowserPac::default()
        };
        for &(hive, recommended) in SEARCH.iter().filter(|(hive, _)| hives.contains(hive)) {
            let key = if recommended {
                format!(r"{}\Recommended", browser.policy_key)
            } else {
                browser.policy_key.to_owned()
            };
            match chromium_key(registry, hive, &key) {
                Ok(None) => continue,
                Ok(Some(policy)) => {
                    row.pac_url = policy.pac_url;
                    row.recommended = recommended;
                }
                Err(detail) => row.detail = Some(crate::reconcile::truncate_detail(detail)),
            }
            row.source = Some(PathBuf::from(format!(r"{}\{key}", hive.prefix())));
            return Some(row);
        }
        installed.then_some(row)
    }

    /// `ProxySettings` is a JSON string value; the legacy `ProxyMode` and
    /// `ProxyPacUrl` are plain string values.
    fn chromium_key(
        registry: &impl Registry,
        hive: Hive,
        key: &str,
    ) -> Result<Option<ProxyPolicy>, String> {
        let string = |name: &str| match registry.value(hive, key, name) {
            Some(RegValue::String(value)) => Some(value),
            _ => None,
        };
        let policy = match string("ProxySettings") {
            Some(json) => serde_json::from_str::<Value>(&json)
                .map(|settings| serde_json::json!({ "ProxySettings": settings }))
                .map_err(|error| {
                    format!(r"parse {}\{key}\ProxySettings: {error}", hive.prefix())
                })?,
            None => {
                let mut legacy = serde_json::Map::new();
                for name in ["ProxyMode", "ProxyPacUrl"] {
                    if let Some(value) = string(name) {
                        legacy.insert(name.to_owned(), Value::String(value));
                    }
                }
                Value::Object(legacy)
            }
        };
        Ok(chromium_proxy(&policy))
    }

    /// Registry policy first, then `policies.json` in the install directory.
    fn firefox(
        registry: &impl Registry,
        existing: &impl Fn(KnownPath) -> Option<PathBuf>,
        hives: &[Hive],
    ) -> Option<BrowserPac> {
        let installed = FIREFOX_INSTALLS
            .iter()
            .any(|known| existing(*known).is_some());
        let mut row = BrowserPac {
            browser: "firefox".to_owned(),
            packaging: if installed {
                "system".to_owned()
            } else {
                String::new()
            },
            ..BrowserPac::default()
        };
        for &hive in hives {
            let string = |name: &str| match registry.value(hive, FIREFOX_POLICY_KEY, name) {
                Some(RegValue::String(value)) => Some(value),
                _ => None,
            };
            let mode = string("Mode");
            let url = string("AutoConfigURL");
            if mode.is_none() && url.is_none() {
                continue;
            }
            row.pac_url = (mode.as_deref() == Some("autoConfig"))
                .then_some(url)
                .flatten()
                .filter(|url| !url.trim().is_empty());
            row.locked = matches!(
                registry.value(hive, FIREFOX_POLICY_KEY, "Locked"),
                Some(RegValue::Dword(value)) if value != 0
            );
            row.source = Some(PathBuf::from(format!(
                r"{}\{FIREFOX_POLICY_KEY}",
                hive.prefix()
            )));
            return Some(row);
        }
        for file in FIREFOX_POLICY_FILES
            .iter()
            .filter_map(|known| existing(*known))
        {
            match read_json(&file) {
                Ok(value) => {
                    let Some((pac_url, locked)) = firefox_proxy(&value) else {
                        continue;
                    };
                    row.pac_url = pac_url;
                    row.locked = locked;
                }
                Err(detail) => row.detail = Some(crate::reconcile::truncate_detail(detail)),
            }
            row.source = Some(file);
            return Some(row);
        }
        installed.then_some(row)
    }

    #[cfg(test)]
    mod tests {
        use std::collections::BTreeMap;

        use super::*;

        #[derive(Default)]
        struct FakeRegistry(BTreeMap<(Hive, String, String), RegValue>);

        impl FakeRegistry {
            fn set(mut self, hive: Hive, key: &str, name: &str, value: RegValue) -> Self {
                self.0
                    .insert((hive, key.to_owned(), name.to_owned()), value);
                self
            }
        }

        impl Registry for FakeRegistry {
            fn value(&self, hive: Hive, key: &str, name: &str) -> Option<RegValue> {
                self.0
                    .get(&(hive, key.to_owned(), name.to_owned()))
                    .cloned()
            }
        }

        const PAC: &str = "https://pac.aks.solo.sademo.de/ai-egress.pac";
        const CHROME: &str = r"SOFTWARE\Policies\Google\Chrome";
        const EDGE: &str = r"SOFTWARE\Policies\Microsoft\Edge";

        fn installed(known: KnownPath) -> Option<PathBuf> {
            (known.0 == "ProgramFiles"
                && (known.1.ends_with("chrome.exe") || known.1.ends_with("firefox.exe")))
            .then(|| PathBuf::from(r"C:\Program Files").join(known.1))
        }

        fn s(value: &str) -> RegValue {
            RegValue::String(value.to_owned())
        }

        #[test]
        fn proxy_settings_json_and_firefox_registry_policy() {
            let registry = FakeRegistry::default()
                .set(
                    Hive::LocalMachine,
                    CHROME,
                    "ProxySettings",
                    s(&format!(
                        r#"{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}"#
                    )),
                )
                .set(
                    Hive::LocalMachine,
                    FIREFOX_POLICY_KEY,
                    "Mode",
                    s("autoConfig"),
                )
                .set(
                    Hive::LocalMachine,
                    FIREFOX_POLICY_KEY,
                    "AutoConfigURL",
                    s(PAC),
                )
                .set(
                    Hive::LocalMachine,
                    FIREFOX_POLICY_KEY,
                    "Locked",
                    RegValue::Dword(1),
                );
            let browsers = discover(&registry, installed, true);
            assert_eq!(browsers.len(), 2);
            let chrome = &browsers[0];
            assert_eq!(chrome.browser, "chrome");
            assert_eq!(chrome.packaging, "system");
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
            assert!(!chrome.recommended);
            assert_eq!(
                chrome.source,
                Some(PathBuf::from(format!(r"HKLM\{CHROME}")))
            );
            let firefox = &browsers[1];
            assert_eq!(firefox.pac_url.as_deref(), Some(PAC));
            assert!(firefox.locked);
            assert_eq!(
                agentdesktop_core::model::DevicePac::from_browsers(&browsers),
                agentdesktop_core::model::DevicePac::Found(PAC.to_owned())
            );
        }

        #[test]
        fn precedence_legacy_values_and_recommended() {
            // User policy only, legacy values: found, not recommended.
            let registry = FakeRegistry::default()
                .set(Hive::CurrentUser, CHROME, "ProxyMode", s("pac_script"))
                .set(Hive::CurrentUser, CHROME, "ProxyPacUrl", s(PAC));
            let chrome = &discover(&registry, installed, true)[0];
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
            assert_eq!(
                chrome.source,
                Some(PathBuf::from(format!(r"HKCU\{CHROME}")))
            );
            // Machine policy wins over user policy.
            let registry = registry.set(
                Hive::LocalMachine,
                CHROME,
                "ProxySettings",
                s(r#"{"ProxyMode":"direct"}"#),
            );
            assert_eq!(discover(&registry, installed, true)[0].pac_url, None);
            // Only the Recommended subkey: reported as recommended.
            let registry = FakeRegistry::default().set(
                Hive::LocalMachine,
                &format!(r"{CHROME}\Recommended"),
                "ProxySettings",
                s(&format!(
                    r#"{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}"#
                )),
            );
            let chrome = &discover(&registry, installed, true)[0];
            assert!(chrome.recommended);
            assert_eq!(chrome.pac_url.as_deref(), Some(PAC));
        }

        #[test]
        fn a_service_ignores_hkcu() {
            let registry = FakeRegistry::default()
                .set(Hive::CurrentUser, CHROME, "ProxyMode", s("pac_script"))
                .set(Hive::CurrentUser, CHROME, "ProxyPacUrl", s(PAC))
                .set(
                    Hive::CurrentUser,
                    FIREFOX_POLICY_KEY,
                    "Mode",
                    s("autoConfig"),
                )
                .set(
                    Hive::CurrentUser,
                    FIREFOX_POLICY_KEY,
                    "AutoConfigURL",
                    s(PAC),
                );
            let browsers = discover(&registry, installed, false);
            assert!(
                browsers
                    .iter()
                    .all(|row| row.pac_url.is_none() && row.source.is_none())
            );
            assert!(
                discover(&registry, installed, true)
                    .iter()
                    .all(|row| row.pac_url.as_deref() == Some(PAC))
            );
        }

        #[test]
        fn edge_policy_without_install_and_broken_json() {
            let registry = FakeRegistry::default()
                .set(
                    Hive::LocalMachine,
                    EDGE,
                    "ProxySettings",
                    s(&format!(
                        r#"{{"ProxyMode":"pac_script","ProxyPacUrl":"{PAC}"}}"#
                    )),
                )
                .set(Hive::LocalMachine, CHROME, "ProxySettings", s("{not json"));
            let browsers = discover(&registry, |_| None, true);
            let edge = browsers.iter().find(|row| row.browser == "edge").unwrap();
            assert_eq!(edge.packaging, "");
            assert_eq!(edge.pac_url.as_deref(), Some(PAC));
            let chrome = browsers.iter().find(|row| row.browser == "chrome").unwrap();
            assert_eq!(chrome.pac_url, None);
            assert!(chrome.detail.as_deref().unwrap().contains("ProxySettings"));
            assert!(browsers.iter().all(|row| row.browser != "firefox"));
        }

        #[test]
        fn installed_without_policy_reports_no_pac() {
            let browsers = discover(&FakeRegistry::default(), installed, true);
            assert_eq!(browsers.len(), 2);
            assert!(
                browsers
                    .iter()
                    .all(|row| row.pac_url.is_none() && row.source.is_none())
            );
        }
    }
}

/// The real registry, read-only (`RegGetValueW`).
#[cfg(windows)]
mod win_registry {
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_DWORD, REG_SZ, RRF_RT_REG_DWORD,
        RRF_RT_REG_SZ, RegGetValueW,
    };

    use super::windows_policy::{Hive, RegValue, Registry};

    pub(super) struct WinRegistry;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    impl Registry for WinRegistry {
        fn value(&self, hive: Hive, key: &str, name: &str) -> Option<RegValue> {
            let root: HKEY = match hive {
                Hive::LocalMachine => HKEY_LOCAL_MACHINE,
                Hive::CurrentUser => HKEY_CURRENT_USER,
            };
            let key = wide(key);
            let name = wide(name);
            // RRF_RT_REG_SZ also returns REG_EXPAND_SZ values, expanded, as
            // REG_SZ; asking for RRF_RT_REG_EXPAND_SZ without RRF_NOEXPAND
            // is rejected with ERROR_INVALID_PARAMETER.
            let flags = RRF_RT_REG_SZ | RRF_RT_REG_DWORD;
            let mut kind = 0u32;
            let mut buffer = vec![0u8; 512];
            loop {
                let mut size = buffer.len() as u32;
                // SAFETY: the key and name are NUL-terminated UTF-16; the
                // buffer is writable for `size` bytes.
                let status = unsafe {
                    RegGetValueW(
                        root,
                        key.as_ptr(),
                        name.as_ptr(),
                        flags,
                        &mut kind,
                        buffer.as_mut_ptr().cast(),
                        &mut size,
                    )
                };
                if status == ERROR_MORE_DATA && size as usize > buffer.len() && size <= 1 << 20 {
                    buffer.resize(size as usize, 0);
                    continue;
                }
                if status != ERROR_SUCCESS {
                    return None;
                }
                buffer.truncate(size as usize);
                return match kind {
                    REG_DWORD if buffer.len() >= 4 => Some(RegValue::Dword(u32::from_le_bytes([
                        buffer[0], buffer[1], buffer[2], buffer[3],
                    ]))),
                    REG_SZ => {
                        let units: Vec<u16> = buffer
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| u16::from_le_bytes(*pair))
                            .take_while(|unit| *unit != 0)
                            .collect();
                        Some(RegValue::String(String::from_utf16_lossy(&units)))
                    }
                    _ => None,
                };
            }
        }
    }
}
