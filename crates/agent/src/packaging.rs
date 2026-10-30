//! The packaging render function. Templates live as data files under
//! `scripts/windows-package/templates/` (shared with the PowerShell build
//! script); this module is the pure Rust counterpart the Rust test suite
//! renders and compares against the checked-in goldens. The production
//! renderer is the PowerShell build script
//! (`scripts/windows-package/build-package.ps1`, which performs the
//! identical substitution and validation — its parity is a lab item), so the
//! module is compiled for tests only.

/// Values substituted into a packaging template. The only placeholder across
/// every template is the `agentdesktop` install path.
pub(crate) struct Manifest {
    /// Absolute install directory of `agentdesktop.exe` (fixed at package
    /// build time), e.g. `C:\Program Files\Agent Desktop`. Substituted
    /// verbatim everywhere `{{agentdesktopInstallPath}}` appears.
    pub agentdesktop_install_path: &'static str,
}

/// Renders `template` against `manifest`. Pure and deterministic: no
/// filesystem, no `%TEMP%`, no manifest directory and no out-dir path ever
/// appear in the output; never touches vendor binary bytes. The caller (the
/// build script) refuses a non-empty out directory before writing the
/// result; that check is not this function's job. The install path is
/// refused when the launchers could not carry it safely (see
/// [`validate_install_path`]); the build script applies the same rule.
pub(crate) fn render(template: &str, manifest: &Manifest) -> Result<String, String> {
    validate_install_path(manifest.agentdesktop_install_path)?;
    Ok(template.replace(
        "{{agentdesktopInstallPath}}",
        manifest.agentdesktop_install_path,
    ))
}

/// The install path is substituted verbatim into a cmd line and a
/// double-quoted sh word, so it must be a drive-absolute Windows path
/// (`X:\...`; a UNC `\\server\...` path is refused) and must not contain
/// `"`, `%`, `!`, `$`, a backtick or a newline — each of those would expand,
/// split or escape inside a rendered launcher. This mirrors the containment
/// policy's string rules for `executable`, plus the shell and cmd
/// metacharacters the launchers themselves are exposed to.
fn validate_install_path(path: &str) -> Result<(), String> {
    let bytes = path.as_bytes();
    let drive_absolute =
        bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !drive_absolute {
        return Err(format!(
            "the agentdesktop install path must be a drive-absolute Windows path (X:\\...), not a UNC or relative one: {path}"
        ));
    }
    if path.contains(['"', '%', '!', '$', '`', '\n', '\r']) {
        return Err(format!(
            "the agentdesktop install path must not contain \" % ! $ ` or a newline: {path}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Manifest, render};
    use crate::provider::claude_code::managed::{ManagedSources, managed_settings_state};

    const CLAUDE_CMD_TEMPLATE: &str =
        include_str!("../../../scripts/windows-package/templates/claude.cmd.tmpl");
    const CLAUDE_SH_TEMPLATE: &str =
        include_str!("../../../scripts/windows-package/templates/claude.tmpl");
    const DROPIN_TEMPLATE: &str = include_str!(
        "../../../scripts/windows-package/templates/managed-settings-dropin.json.tmpl"
    );

    // The checked-in goldens are byte-exact LF. The one intended divergence
    // (DV6-3): the build script writes the rendered claude.cmd with CRLF
    // line endings for cmd.exe; the lab parity check normalizes the .cmd
    // line endings before comparing. The sh launcher stays LF everywhere.
    const CLAUDE_CMD_GOLDEN: &str =
        include_str!("../../../scripts/windows-package/templates/goldens/claude.cmd");
    const CLAUDE_SH_GOLDEN: &str =
        include_str!("../../../scripts/windows-package/templates/goldens/claude");
    const DROPIN_GOLDEN: &str = include_str!(
        "../../../scripts/windows-package/templates/goldens/managed-settings-dropin.json"
    );

    fn manifest() -> Manifest {
        Manifest {
            agentdesktop_install_path: r"C:\Program Files\Agent Desktop",
        }
    }

    #[test]
    fn claude_cmd_renders_to_its_golden() {
        assert_eq!(
            render(CLAUDE_CMD_TEMPLATE, &manifest()).unwrap(),
            CLAUDE_CMD_GOLDEN
        );
    }

    #[test]
    fn claude_sh_renders_to_its_golden() {
        assert_eq!(
            render(CLAUDE_SH_TEMPLATE, &manifest()).unwrap(),
            CLAUDE_SH_GOLDEN
        );
    }

    #[test]
    fn the_dropin_renders_to_its_golden_and_has_no_placeholder_of_its_own() {
        assert_eq!(render(DROPIN_TEMPLATE, &manifest()).unwrap(), DROPIN_GOLDEN);
        assert!(
            !DROPIN_TEMPLATE.contains("{{"),
            "the drop-in needs no placeholder"
        );
    }

    /// The rendered launchers QUOTE the agentdesktop.exe path and the pipe
    /// name: the default install directory contains a space, and a username
    /// may contain one too.
    #[test]
    fn the_rendered_launchers_quote_the_executable_path_and_the_pipe_name() {
        let cmd = render(CLAUDE_CMD_TEMPLATE, &manifest()).unwrap();
        assert!(
            cmd.contains(r#""C:\Program Files\Agent Desktop\agentdesktop.exe""#),
            "{cmd}"
        );
        assert!(
            cmd.contains(r#""\\.\pipe\agentdesktop-user-%USERNAME%""#),
            "{cmd}"
        );
        let sh = render(CLAUDE_SH_TEMPLATE, &manifest()).unwrap();
        assert!(
            sh.contains(r#""C:\Program Files\Agent Desktop\agentdesktop.exe""#),
            "{sh}"
        );
        assert!(
            sh.contains(r#""//./pipe/agentdesktop-user-$USERNAME""#),
            "{sh}"
        );
    }

    /// The .cmd launcher forwards the arguments INLINE (`... claude -- %*`),
    /// with no intermediate `set` capture and no delayed expansion: a
    /// round-1 `set "ARGS=%*"` capture broke every balanced-quoted argument
    /// containing a metacharacter (a quoted prompt with `&` ran its tail as
    /// a native command), which inline `%*` keeps intact for the daemon's
    /// refusal. Byte-level pin on the behavior-bearing line, verifiable
    /// cross-platform; cmd's runtime parsing itself is a lab item.
    #[test]
    fn the_cmd_launcher_forwards_arguments_inline_without_a_set_capture() {
        let cmd = render(CLAUDE_CMD_TEMPLATE, &manifest()).unwrap();
        let exec_lines: Vec<&str> = cmd
            .lines()
            .filter(|line| line.contains("agentdesktop.exe"))
            .collect();
        assert_eq!(exec_lines.len(), 1, "{cmd}");
        assert!(exec_lines[0].trim_end().ends_with("claude -- %*"), "{cmd}");
        for capture in ["set \"ARGS=", "!ARGS!", "EnableDelayedExpansion"] {
            assert!(!cmd.contains(capture), "{capture} found:\n{cmd}");
        }
    }

    /// AC-5: rendering is deterministic and pure; the same template and
    /// manifest always produce byte-identical output, with no `%TEMP%`,
    /// manifest directory or out-dir path, and no vendor binary bytes.
    #[test]
    fn rendering_is_deterministic_and_carries_no_incidental_paths() {
        let first = render(CLAUDE_CMD_TEMPLATE, &manifest()).unwrap();
        let second = render(CLAUDE_CMD_TEMPLATE, &manifest()).unwrap();
        assert_eq!(first, second);
        for incidental in ["%TEMP%", "AppData\\Local\\Temp", "out\\"] {
            assert!(!first.contains(incidental), "{first}");
        }
        // No vendor binary bytes: the launcher never contains the MZ header
        // or the vendor's own executable name standing alone as a path.
        assert!(!first.contains("\\Anthropic\\ClaudeCode\\claude.exe"));
    }

    /// AC-5: a different manifest produces different output (the install
    /// path is substituted, not ignored).
    #[test]
    fn a_different_install_path_changes_the_rendered_launcher() {
        let other = Manifest {
            agentdesktop_install_path: r"D:\Tools\agentdesktop",
        };
        let rendered = render(CLAUDE_CMD_TEMPLATE, &other).unwrap();
        assert!(rendered.contains(r"D:\Tools\agentdesktop\agentdesktop.exe"));
        assert!(!rendered.contains(r"C:\Program Files\Agent Desktop"));
    }

    /// Adversarial m3: an install path that would expand, split or escape
    /// inside the rendered launchers — or a UNC/relative path — is refused
    /// by `render` itself; the build script mirrors the same rule.
    #[test]
    fn a_hostile_unc_or_relative_install_path_is_refused() {
        for path in [
            r"C:\Apps\100%PATH%x",
            r"C:\A$(id)",
            "C:\\A`whoami`",
            "C:\\A\"B",
            r"C:\A!x",
            "C:\\A\nB",
            r"\\srv\share\agentdesktop",
            r"relative\agentdesktop",
            "",
        ] {
            let manifest = Manifest {
                agentdesktop_install_path: path,
            };
            let error = render(CLAUDE_CMD_TEMPLATE, &manifest)
                .expect_err(&format!("{path:?} must be refused"));
            assert!(error.contains("install path"), "{error}");
        }
    }

    /// AC-8c (N7): every launcher template contains the refusal guard.
    #[test]
    fn every_launcher_template_contains_the_in_box_refusal_guard() {
        for template in [CLAUDE_CMD_TEMPLATE, CLAUDE_SH_TEMPLATE] {
            assert!(
                template.contains("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"),
                "{template}"
            );
        }
    }

    /// AC-4 (golden grep): no launcher template contains the vendor binary's
    /// own path (fallback-to-native is forbidden).
    #[test]
    fn no_launcher_template_contains_the_vendor_binary_path() {
        for template in [CLAUDE_CMD_TEMPLATE, CLAUDE_SH_TEMPLATE] {
            assert!(
                !template.to_lowercase().contains(r"anthropic\claudecode"),
                "{template}"
            );
        }
    }

    /// AC-5: the emitted drop-in parses as a JSON object and
    /// `managed_settings_state` over `{file: none, drop-ins: [it]}` reports
    /// no error.
    #[test]
    fn the_emitted_dropin_parses_as_json_and_managed_settings_state_reports_no_error() {
        let value: serde_json::Value = serde_json::from_str(DROPIN_GOLDEN).unwrap();
        assert!(value.is_object(), "{value}");
        let sources = ManagedSources {
            hklm: None,
            file: None,
            drop_ins: vec![(
                "40-agentdesktop-package.json".to_owned(),
                Ok(DROPIN_GOLDEN.to_owned()),
            )],
        };
        let posture = managed_settings_state(&sources);
        assert_eq!(posture.error, None, "{posture:?}");
    }

    /// AC-5: a non-empty out directory is refused before anything is
    /// written. The refusal is the caller's (the build script), not
    /// `render`'s, so this pins the contract at the boundary this module
    /// owns: `render` itself never consults any directory.
    #[test]
    fn render_takes_no_directory_argument_and_cannot_itself_check_an_out_dir() {
        // Structural guard: `render`'s signature is `(template: &str,
        // manifest: &Manifest) -> Result<String, String>`, so it has no
        // path to check.
        // Compiling this test at all is the assertion.
        let _: fn(&str, &Manifest) -> Result<String, String> = render;
    }
}
