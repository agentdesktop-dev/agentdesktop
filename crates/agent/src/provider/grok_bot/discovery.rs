use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::Agent;

use super::GrokBot;
use crate::provider::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_all_in_path("grok-bot")
        .into_iter()
        .chain(
            executable_candidates()
                .into_iter()
                .filter(|candidate| candidate.is_file()),
        )
        .find(|candidate| is_grok_bot(candidate))?;
    Some(Agent {
        version: discover_version(&executable),
        executable,
        kind: GrokBot::ID.to_owned(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();

    #[cfg(target_os = "macos")]
    {
        candidates.insert(PathBuf::from(
            "/Applications/Grok Bot.app/Contents/MacOS/Grok Bot",
        ));
        candidates.insert(PathBuf::from(
            "/Applications/Grok Bot 2.app/Contents/MacOS/Grok Bot",
        ));
        for home in metadata::user_home_dirs() {
            candidates.insert(home.join("Applications/Grok Bot.app/Contents/MacOS/Grok Bot"));
        }
    }

    #[cfg(windows)]
    {
        for home in metadata::user_home_dirs() {
            let local = home.join("AppData/Local");
            candidates.insert(local.join("Programs/Grok Bot/Grok Bot.exe"));
            candidates.insert(local.join("Grok Bot/Grok Bot.exe"));
        }
        for root in [
            metadata::env_path("ProgramFiles"),
            metadata::env_path("ProgramFiles(x86)"),
            metadata::env_path("LOCALAPPDATA"),
        ]
        .into_iter()
        .flatten()
        {
            candidates.insert(root.join("Grok Bot/Grok Bot.exe"));
            candidates.insert(root.join("Programs/Grok Bot/Grok Bot.exe"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        // .deb/.rpm install into /opt/Grok Bot (binary may be grok-bot or "Grok Bot").
        candidates.extend([
            PathBuf::from("/opt/Grok Bot/grok-bot"),
            PathBuf::from("/opt/Grok Bot/Grok Bot"),
            PathBuf::from("/opt/grok-bot/grok-bot"),
            PathBuf::from("/usr/bin/grok-bot"),
            PathBuf::from("/usr/local/bin/grok-bot"),
            PathBuf::from("/usr/lib/grok-bot/grok-bot"),
            PathBuf::from("/usr/share/grok-bot/grok-bot"),
        ]);
        for home in metadata::user_home_dirs() {
            candidates.insert(home.join(".local/bin/grok-bot"));
            candidates.insert(home.join(".local/share/Grok Bot/grok-bot"));
            candidates.insert(home.join("Applications/grok-bot.AppImage"));
            candidates.insert(home.join("Applications/Grok Bot.AppImage"));
        }
    }

    candidates.into_iter().collect()
}

fn is_grok_bot(executable: &Path) -> bool {
    if let Some(identifier) = bundle_identifier(executable) {
        return identifier == GrokBot::BUNDLE_IDENTIFIER;
    }
    asar_archives(executable)
        .into_iter()
        .any(|archive| metadata::electron_asar_version(&archive, GrokBot::PRODUCT_NAME).is_some())
}

fn discover_version(executable: &Path) -> Option<String> {
    if let Some(version) = bundle_short_version(executable).filter(|value| !value.is_empty()) {
        return Some(version);
    }
    asar_archives(executable)
        .into_iter()
        .find_map(|archive| metadata::electron_asar_version(&archive, GrokBot::PRODUCT_NAME))
}

fn asar_archives(executable: &Path) -> BTreeSet<PathBuf> {
    let mut archives = BTreeSet::new();
    for executable in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(directory) = executable.parent() {
            archives.insert(directory.join("resources/app.asar"));
            archives.insert(directory.join("../Resources/app.asar"));
        }
    }
    archives
}

#[cfg(target_os = "macos")]
fn bundle_plist(executable: &Path) -> Option<PathBuf> {
    let mut directory = executable.parent()?.to_path_buf();
    for _ in 0..4 {
        let plist = directory.join("Info.plist");
        if plist.is_file() {
            return Some(plist);
        }
        if !directory.pop() {
            break;
        }
    }
    None
}

#[cfg(target_os = "macos")]
#[derive(serde::Deserialize)]
struct InfoPlist {
    #[serde(rename = "CFBundleIdentifier")]
    identifier: Option<String>,
    #[serde(rename = "CFBundleShortVersionString")]
    version: Option<String>,
}

#[cfg(target_os = "macos")]
fn read_info_plist(executable: &Path) -> Option<InfoPlist> {
    let path = bundle_plist(executable)?;
    plist::from_file(path).ok()
}

#[cfg(target_os = "macos")]
fn bundle_identifier(executable: &Path) -> Option<String> {
    read_info_plist(executable)?
        .identifier
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn bundle_identifier(_executable: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn bundle_short_version(executable: &Path) -> Option<String> {
    read_info_plist(executable)?
        .version
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn bundle_short_version(_executable: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;

    #[cfg(target_os = "macos")]
    use super::{bundle_identifier, bundle_short_version, is_grok_bot};
    #[cfg(target_os = "macos")]
    use crate::provider::grok_bot::GrokBot;

    #[cfg(target_os = "macos")]
    #[test]
    fn accepts_official_bundle_and_reads_version() {
        let executable = fake_app("Grok Bot", GrokBot::BUNDLE_IDENTIFIER, "0.57.0");
        assert!(is_grok_bot(&executable));
        assert_eq!(
            bundle_identifier(&executable).as_deref(),
            Some(GrokBot::BUNDLE_IDENTIFIER)
        );
        assert_eq!(bundle_short_version(&executable).as_deref(), Some("0.57.0"));
        let _ = fs::remove_dir_all(executable.ancestors().nth(3).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn discovers_installed_macos_app_when_present() {
        let path = PathBuf::from("/Applications/Grok Bot.app/Contents/MacOS/Grok Bot");
        if !path.is_file() {
            return;
        }
        let agent = super::discover().expect("Grok Bot is installed");
        assert_eq!(agent.kind, GrokBot::ID);
        assert!(
            agent
                .version
                .as_deref()
                .is_some_and(|version| !version.is_empty())
        );
        assert!(agent.mcp_servers.is_empty());
        assert!(agent.skills.is_empty());
        assert!(agent.executable.ends_with("Grok Bot"));
    }

    #[test]
    fn asar_lookup_uses_electron_resources_next_to_the_binary() {
        let root =
            std::env::temp_dir().join(format!("agentdesktop-grok-bot-asar-{}", std::process::id()));
        let bin = root.join("Grok Bot.exe");
        fs::create_dir_all(root.join("resources")).unwrap();
        fs::write(&bin, []).unwrap();
        let archives = super::asar_archives(&bin);
        assert!(archives.contains(&root.join("resources/app.asar")));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_an_unrelated_grok_named_app() {
        let executable = fake_app("Grok Bot", "com.example.unofficial-grok", "1.0.0");
        assert!(!is_grok_bot(&executable));
        let _ = fs::remove_dir_all(executable.ancestors().nth(3).unwrap());
    }

    #[cfg(target_os = "macos")]
    fn fake_app(name: &str, identifier: &str, version: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-grok-bot-{name}-{identifier}-{}",
            std::process::id()
        ));
        let macos = root.join(format!("{name}.app/Contents/MacOS"));
        fs::create_dir_all(&macos).unwrap();
        fs::write(
            root.join(format!("{name}.app/Contents/Info.plist")),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>{identifier}</string>
  <key>CFBundleShortVersionString</key>
  <string>{version}</string>
</dict>
</plist>
"#
            ),
        )
        .unwrap();
        let executable = macos.join(name);
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        executable
    }
}
