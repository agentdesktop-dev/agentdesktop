//! The browser PAC value per device and the fleet compliance check.
//!
//! Devices report the PAC their browser policy sets. The device value is shown
//! with or without a profile; the compliance check only runs when the fleet
//! configuration sets `browserEgress.compliance`, and is evaluated here at read
//! time so a profile change shows at once. Agentdesktop never configures
//! browsers from it.

use std::collections::BTreeMap;

use agentdesktop_core::model::{BrowserPac, DevicePac, PacFetch};
use serde::Serialize;
use url::Url;

/// One PAC value per device, plus the compliance result when a profile is set.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeviceBrowserPac {
    /// `absent`, `found`, `differs`, or `not_reported` (no browser rows: an
    /// older daemon, or no supported browser on the device).
    pub state: &'static str,
    /// The PAC URL when `state` is `found`.
    pub url: Option<String>,
    /// SHA-256 of the PAC this device fetched from `url`: its identity.
    pub sha256: Option<String>,
    /// Body size in bytes, with `sha256`.
    pub size: Option<u64>,
    /// Why the device could not fetch `url`.
    pub fetch_error: Option<String>,
    /// `None` without a compliance profile in the fleet configuration.
    pub compliance: Option<PacCompliance>,
}

impl Default for DeviceBrowserPac {
    fn default() -> Self {
        Self {
            state: "not_reported",
            url: None,
            sha256: None,
            size: None,
            fetch_error: None,
            compliance: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PacCompliance {
    /// `compliant`, `not_compliant` or `not_reported`.
    pub state: &'static str,
    /// Why the device is not compliant.
    pub reason: Option<String>,
}

impl PacCompliance {
    fn not_compliant(reason: String) -> Self {
        Self {
            state: "not_compliant",
            reason: Some(reason),
        }
    }
}

/// The device value, and the compliance check against `expected` when set.
pub fn evaluate(browsers: &[BrowserPac], expected: Option<&Url>) -> DeviceBrowserPac {
    if browsers.is_empty() {
        return DeviceBrowserPac {
            state: "not_reported",
            compliance: expected.map(|_| PacCompliance {
                state: "not_reported",
                reason: None,
            }),
            ..DeviceBrowserPac::default()
        };
    }
    let device = DevicePac::from_browsers(browsers);
    let (state, url) = match &device {
        DevicePac::Absent => ("absent", None),
        DevicePac::Found(url) => ("found", Some(url.clone())),
        DevicePac::Differs => ("differs", None),
    };
    DeviceBrowserPac {
        state,
        url,
        compliance: expected.map(|expected| compliance(browsers, &device, expected)),
        ..DeviceBrowserPac::default()
    }
}

/// Adds the identity of the device's PAC from the device's fetch of its URL.
pub fn with_identity(mut pac: DeviceBrowserPac, fetches: &[PacFetch]) -> DeviceBrowserPac {
    if let Some(fetch) = pac
        .url
        .as_deref()
        .and_then(|url| fetches.iter().find(|fetch| fetch.url == url))
    {
        pac.sha256 = fetch.sha256.clone();
        pac.size = fetch.size;
        pac.fetch_error = fetch.error.clone();
    }
    pac
}

/// Devices that found a PAC, grouped by URL and identity, largest group first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PacVersion {
    pub url: String,
    /// `None` for devices that could not fetch the URL (or older daemons).
    pub sha256: Option<String>,
    pub devices: usize,
}

pub fn pac_versions<'a>(pacs: impl IntoIterator<Item = &'a DeviceBrowserPac>) -> Vec<PacVersion> {
    let mut groups: BTreeMap<(String, Option<String>), usize> = BTreeMap::new();
    for pac in pacs {
        if let Some(url) = &pac.url {
            *groups.entry((url.clone(), pac.sha256.clone())).or_default() += 1;
        }
    }
    let mut versions: Vec<PacVersion> = groups
        .into_iter()
        .map(|((url, sha256), devices)| PacVersion {
            url,
            sha256,
            devices,
        })
        .collect();
    versions.sort_by_key(|version| std::cmp::Reverse(version.devices));
    versions
}

fn compliance(browsers: &[BrowserPac], device: &DevicePac, expected: &Url) -> PacCompliance {
    // A browser whose policy could not be read (unreadable or conflicting
    // files) explains a missing or differing PAC better than the PAC itself.
    if let Some(browser) = browsers
        .iter()
        .find(|browser| browser.pac_url.is_none() && browser.detail.is_some())
    {
        return PacCompliance::not_compliant(format!(
            "{} policy: {}",
            browser.browser,
            browser.detail.as_deref().unwrap_or_default()
        ));
    }
    let url = match device {
        DevicePac::Absent => {
            return PacCompliance::not_compliant("no PAC in browser policy".to_owned());
        }
        DevicePac::Differs => {
            return PacCompliance::not_compliant("browsers set different PACs".to_owned());
        }
        DevicePac::Found(url) => url,
    };
    if !same_url(url, expected) {
        return PacCompliance::not_compliant(format!("PAC is {url}, expected {expected}"));
    }
    // The value must be one the user cannot change.
    let overridable: Vec<&str> = browsers
        .iter()
        .filter(|browser| browser.recommended || (browser.browser == "firefox" && !browser.locked))
        .map(|browser| browser.browser.as_str())
        .collect();
    if !overridable.is_empty() {
        return PacCompliance::not_compliant(format!(
            "user-overridable policy: {}",
            overridable.join(", ")
        ));
    }
    PacCompliance {
        state: "compliant",
        reason: None,
    }
}

/// Exact match, after URL parsing on both sides so that the profile's
/// normalised form (`https://pac.example/` for `https://pac.example`) matches.
fn same_url(reported: &str, expected: &Url) -> bool {
    Url::parse(reported).map_or(reported == expected.as_str(), |reported| {
        &reported == expected
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAC: &str = "https://pac.corp.example/ai.pac";

    fn browser(name: &str, pac_url: Option<&str>) -> BrowserPac {
        BrowserPac {
            browser: name.to_owned(),
            pac_url: pac_url.map(str::to_owned),
            locked: name == "firefox",
            ..BrowserPac::default()
        }
    }

    fn expected() -> Url {
        Url::parse(PAC).unwrap()
    }

    #[test]
    fn device_value_without_a_profile_has_no_verdict() {
        let found = evaluate(&[browser("chrome", Some(PAC))], None);
        assert_eq!(found.state, "found");
        assert_eq!(found.url.as_deref(), Some(PAC));
        assert_eq!(found.compliance, None);
        assert_eq!(evaluate(&[browser("chrome", None)], None).state, "absent");
        assert_eq!(evaluate(&[], None).state, "not_reported");
    }

    #[test]
    fn compliant_when_every_browser_sets_the_expected_pac_from_managed_policy() {
        let result = evaluate(
            &[browser("chrome", Some(PAC)), browser("firefox", Some(PAC))],
            Some(&expected()),
        );
        assert_eq!(
            result.compliance,
            Some(PacCompliance {
                state: "compliant",
                reason: None
            })
        );
    }

    #[test]
    fn not_compliant_reasons() {
        let expected = expected();
        let reason = |browsers: &[BrowserPac]| {
            let compliance = evaluate(browsers, Some(&expected)).compliance.unwrap();
            assert_eq!(compliance.state, "not_compliant");
            compliance.reason.unwrap()
        };
        assert_eq!(
            reason(&[browser("chrome", None)]),
            "no PAC in browser policy"
        );
        assert_eq!(
            reason(&[browser("chrome", Some(PAC)), browser("firefox", None)]),
            "browsers set different PACs"
        );
        assert_eq!(
            reason(&[browser("chrome", Some("https://old.example/ai.pac"))]),
            "PAC is https://old.example/ai.pac, expected https://pac.corp.example/ai.pac"
        );
        let mut recommended = browser("chrome", Some(PAC));
        recommended.recommended = true;
        let mut unlocked = browser("firefox", Some(PAC));
        unlocked.locked = false;
        assert_eq!(
            reason(&[recommended, unlocked]),
            "user-overridable policy: chrome, firefox"
        );
    }

    #[test]
    fn a_policy_read_error_is_the_reason() {
        let mut broken = browser("chrome", None);
        broken.detail = Some("conflicting proxy policy in a.json and b.json".to_owned());
        let reason = evaluate(&[broken], Some(&expected()))
            .compliance
            .unwrap()
            .reason
            .unwrap();
        assert_eq!(
            reason,
            "chrome policy: conflicting proxy policy in a.json and b.json"
        );
    }

    #[test]
    fn no_rows_with_a_profile_is_not_reported() {
        let result = evaluate(&[], Some(&expected()));
        assert_eq!(result.compliance.unwrap().state, "not_reported");
    }

    #[test]
    fn identity_comes_from_the_fetch_of_the_device_url() {
        let fetches = [
            PacFetch {
                url: PAC.to_owned(),
                sha256: Some("aa".repeat(32)),
                size: Some(10),
                error: None,
            },
            PacFetch {
                url: "https://other.example/p.pac".to_owned(),
                error: Some("timed out".to_owned()),
                ..PacFetch::default()
            },
        ];
        let found = with_identity(evaluate(&[browser("chrome", Some(PAC))], None), &fetches);
        assert_eq!(found.sha256, Some("aa".repeat(32)));
        assert_eq!(found.size, Some(10));
        let failed = with_identity(
            evaluate(
                &[browser("chrome", Some("https://other.example/p.pac"))],
                None,
            ),
            &fetches,
        );
        assert_eq!(failed.sha256, None);
        assert_eq!(failed.fetch_error.as_deref(), Some("timed out"));
        let absent = with_identity(evaluate(&[browser("chrome", None)], None), &fetches);
        assert_eq!(absent.sha256, None);
    }

    #[test]
    fn versions_group_devices_by_url_and_hash() {
        let pac = |sha: Option<&str>| DeviceBrowserPac {
            state: "found",
            url: Some(PAC.to_owned()),
            sha256: sha.map(str::to_owned),
            ..DeviceBrowserPac::default()
        };
        let devices = [
            pac(Some("aa")),
            pac(Some("aa")),
            pac(Some("bb")),
            pac(None),
            DeviceBrowserPac::default(),
        ];
        let versions = pac_versions(&devices);
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].sha256.as_deref(), Some("aa"));
        assert_eq!(versions[0].devices, 2);
    }

    #[test]
    fn url_comparison_uses_the_parsed_form() {
        let expected = Url::parse("https://pac.corp.example").unwrap();
        assert!(same_url("https://pac.corp.example", &expected));
        assert!(same_url("https://pac.corp.example/", &expected));
        assert!(!same_url("https://pac.corp.example/other.pac", &expected));
    }
}
