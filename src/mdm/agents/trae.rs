use crate::error::GitAiError;
use crate::mdm::command_line::{HookShell, platform_hook_shell, render_hook_command};
use crate::mdm::hook_installer::{
    HookCheckResult, HookInstaller, HookInstallerParams, InstallResult,
};
#[cfg(not(target_os = "macos"))]
use crate::mdm::utils::binary_exists;
#[cfg(target_os = "windows")]
use crate::mdm::utils::windows_uninstall_display_name_exists;
use crate::mdm::utils::{
    generate_diff, home_dir, install_vsc_editor_extension, is_git_ai_checkpoint_command,
    is_vsc_editor_extension_installed, resolve_editor_cli, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const TRAE_HOOK_ARGS: &[&str] = &["checkpoint", "trae", "--hook-input", "stdin"];
const TRAE_CATCH_ALL_MATCHER: &str = "*";

pub struct TraeInstaller;

impl TraeInstaller {
    fn hook_command(binary_path: &Path) -> String {
        render_hook_command(
            binary_path,
            TRAE_HOOK_ARGS,
            platform_hook_shell(HookShell::PowerShell),
        )
    }

    fn is_trae_checkpoint_command(command: &str) -> bool {
        is_git_ai_checkpoint_command(command) && command.contains("checkpoint trae")
    }

    fn hooks_paths() -> Vec<PathBuf> {
        let home = home_dir();
        let international_config_exists = home.join(".trae").exists();
        let cn_config_exists = home.join(".trae-cn").exists();

        #[cfg(target_os = "macos")]
        let international_installed = international_config_exists
            || [
                PathBuf::from("/Applications/Trae.app"),
                PathBuf::from("/Applications/TRAE.app"),
                home.join("Applications").join("Trae.app"),
                home.join("Applications").join("TRAE.app"),
            ]
            .iter()
            .any(|path| path.exists());
        #[cfg(target_os = "macos")]
        let cn_installed = cn_config_exists
            || [
                PathBuf::from("/Applications/Trae CN.app"),
                PathBuf::from("/Applications/TRAE CN.app"),
                home.join("Applications").join("Trae CN.app"),
                home.join("Applications").join("TRAE CN.app"),
            ]
            .iter()
            .any(|path| path.exists());

        #[cfg(target_os = "windows")]
        let (international_installed, cn_installed) = {
            let roots = Self::windows_app_roots();
            let tasklist = Self::windows_tasklist();
            (
                international_config_exists
                    || Self::windows_international_app_candidates(&home, &roots)
                        .iter()
                        .any(|path| path.exists())
                    || ["trae", "Trae"].iter().any(|name| binary_exists(name))
                    || tasklist
                        .as_deref()
                        .is_some_and(Self::tasklist_contains_trae_international)
                    || windows_uninstall_display_name_exists(&["Trae", "TRAE"]),
                cn_config_exists
                    || Self::windows_cn_app_candidates(&home, &roots)
                        .iter()
                        .any(|path| path.exists())
                    || ["trae-cn", "TraeCN"].iter().any(|name| binary_exists(name))
                    || tasklist
                        .as_deref()
                        .is_some_and(Self::tasklist_contains_trae_cn)
                    || windows_uninstall_display_name_exists(&[
                        "Trae CN", "TRAE CN", "TraeCN", "Trae-CN",
                    ]),
            )
        };

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let international_installed = international_config_exists || binary_exists("trae");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let cn_installed = cn_config_exists || binary_exists("trae-cn");

        Self::hooks_paths_for_variants(&home, international_installed, cn_installed)
    }

    fn hooks_paths_for_variants(
        home: &Path,
        international_installed: bool,
        cn_installed: bool,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if international_installed {
            paths.push(home.join(".trae").join("hooks.json"));
        }
        if cn_installed {
            paths.push(home.join(".trae-cn").join("hooks.json"));
        }
        paths
    }

    #[cfg(target_os = "windows")]
    fn windows_app_roots() -> Vec<PathBuf> {
        [
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            std::env::var_os("ProgramFiles").map(PathBuf::from),
            std::env::var_os("ProgramW6432").map(PathBuf::from),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    #[cfg(any(test, target_os = "windows"))]
    fn windows_international_app_candidates(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
        Self::windows_variant_app_candidates(home, roots, ".trae", "Trae.exe", &["Trae", "TRAE"])
    }

    #[cfg(any(test, target_os = "windows"))]
    fn windows_cn_app_candidates(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut candidates = Self::windows_variant_app_candidates(
            home,
            roots,
            ".trae-cn",
            "Trae CN.exe",
            &["Trae CN", "TRAE CN", "TraeCN"],
        );
        candidates.extend(Self::windows_variant_app_candidates(
            home,
            roots,
            ".trae-cn",
            "TraeCN.exe",
            &["Trae CN", "TRAE CN", "TraeCN"],
        ));
        candidates.sort();
        candidates.dedup();
        candidates
    }

    #[cfg(any(test, target_os = "windows"))]
    fn windows_variant_app_candidates(
        home: &Path,
        roots: &[PathBuf],
        config_dir: &str,
        executable: &str,
        install_dir_names: &[&str],
    ) -> Vec<PathBuf> {
        let mut candidates = vec![home.join(config_dir).join(executable)];
        for install_dir in install_dir_names {
            candidates.push(
                home.join("AppData")
                    .join("Local")
                    .join("Programs")
                    .join(install_dir)
                    .join(executable),
            );
        }
        for root in roots {
            for install_dir in install_dir_names {
                candidates.push(root.join("Programs").join(install_dir).join(executable));
                candidates.push(root.join(install_dir).join(executable));
            }
        }
        candidates.sort();
        candidates.dedup();
        candidates
    }

    #[cfg(target_os = "windows")]
    fn windows_tasklist() -> Option<Vec<u8>> {
        std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| output.stdout)
    }

    #[cfg(any(test, target_os = "windows"))]
    fn tasklist_contains_trae_international(output: &[u8]) -> bool {
        Self::tasklist_contains_any(output, &["trae", "trae.exe"])
    }

    #[cfg(any(test, target_os = "windows"))]
    fn tasklist_contains_trae_cn(output: &[u8]) -> bool {
        Self::tasklist_contains_any(output, &["trae cn", "trae cn.exe", "traecn", "traecn.exe"])
    }

    #[cfg(any(test, target_os = "windows"))]
    fn tasklist_contains_any(output: &[u8], candidates: &[&str]) -> bool {
        String::from_utf8_lossy(output).lines().any(|line| {
            line.split(',')
                .next()
                .map(|image| image.trim().trim_matches('"'))
                .is_some_and(|image| {
                    candidates
                        .iter()
                        .any(|candidate| image.eq_ignore_ascii_case(candidate))
                })
        })
    }

    fn hook_status(settings: &Value, desired_cmd: &str) -> (bool, bool) {
        let hooks_installed = ["PreToolUse", "PostToolUse"]
            .iter()
            .any(|event| Self::event_has_trae_hook(settings, event));
        let hooks_up_to_date = ["PreToolUse", "PostToolUse"]
            .iter()
            .all(|event| Self::event_hook_is_up_to_date(settings, event, desired_cmd));

        (hooks_installed, hooks_up_to_date)
    }

    fn event_has_trae_hook(settings: &Value, event: &str) -> bool {
        let Some(blocks) = settings
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(|value| value.as_array())
        else {
            return false;
        };

        blocks.iter().any(|block| {
            block
                .get("hooks")
                .and_then(|hooks| hooks.as_array())
                .map(|hooks| {
                    hooks.iter().any(|hook| {
                        hook.get("command")
                            .and_then(|command| command.as_str())
                            .map(Self::is_trae_checkpoint_command)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
    }

    fn event_hook_is_up_to_date(settings: &Value, event: &str, desired_cmd: &str) -> bool {
        let Some(blocks) = settings
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(|value| value.as_array())
        else {
            return false;
        };

        let mut trae_hook_count = 0;
        let mut desired_catch_all_count = 0;

        for block in blocks {
            let is_catch_all = block
                .get("matcher")
                .and_then(|matcher| matcher.as_str())
                .map(|matcher| matcher == TRAE_CATCH_ALL_MATCHER)
                .unwrap_or(false);
            let Some(hooks) = block.get("hooks").and_then(|hooks| hooks.as_array()) else {
                continue;
            };

            for hook in hooks {
                let Some(command) = hook.get("command").and_then(|command| command.as_str()) else {
                    continue;
                };
                if Self::is_trae_checkpoint_command(command) {
                    trae_hook_count += 1;
                    if is_catch_all && command == desired_cmd {
                        desired_catch_all_count += 1;
                    }
                }
            }
        }

        trae_hook_count == 1 && desired_catch_all_count == 1
    }

    fn install_hooks_at(
        hooks_path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if !dry_run && let Some(dir) = hooks_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if hooks_path.exists() {
            fs::read_to_string(hooks_path)?
        } else {
            String::new()
        };
        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let pre_tool_cmd = Self::hook_command(&params.binary_path);
        let post_tool_cmd = pre_tool_cmd.clone();

        let mut merged = existing.clone();
        if !merged.is_object() {
            merged = json!({});
        }

        if let Some(root) = merged.as_object_mut() {
            root.entry("version").or_insert(json!(1));
        }

        let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));
        if !hooks_obj.is_object() {
            hooks_obj = json!({});
        }

        for (event, desired_cmd) in [
            ("PreToolUse", pre_tool_cmd.as_str()),
            ("PostToolUse", post_tool_cmd.as_str()),
        ] {
            let event_blocks = hooks_obj
                .get(event)
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            let event_blocks = Self::merge_event_hooks(event_blocks, desired_cmd);

            if let Some(obj) = hooks_obj.as_object_mut() {
                obj.insert(event.to_string(), Value::Array(event_blocks));
            }
        }

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        if existing == merged {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(hooks_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(hooks_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn merge_event_hooks(mut blocks: Vec<Value>, desired_cmd: &str) -> Vec<Value> {
        let mut emptied_by_migration = vec![false; blocks.len()];

        for (index, block) in blocks.iter_mut().enumerate() {
            if let Some(hooks) = block
                .get_mut("hooks")
                .and_then(|hooks| hooks.as_array_mut())
            {
                let before = hooks.len();
                hooks.retain(|hook| {
                    hook.get("command")
                        .and_then(|command| command.as_str())
                        .map(|command| !Self::is_trae_checkpoint_command(command))
                        .unwrap_or(true)
                });
                if before > 0 && hooks.is_empty() {
                    emptied_by_migration[index] = true;
                }
            }
        }

        let mut index = 0;
        blocks.retain(|_| {
            let remove = emptied_by_migration[index];
            index += 1;
            !remove
        });

        let catch_all_idx = blocks
            .iter()
            .position(|block| {
                block
                    .get("matcher")
                    .and_then(|matcher| matcher.as_str())
                    .map(|matcher| matcher == TRAE_CATCH_ALL_MATCHER)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| {
                blocks.push(json!({
                    "matcher": TRAE_CATCH_ALL_MATCHER,
                    "hooks": []
                }));
                blocks.len() - 1
            });

        let mut hooks_array = blocks[catch_all_idx]
            .get("hooks")
            .and_then(|hooks| hooks.as_array())
            .cloned()
            .unwrap_or_default();

        hooks_array.push(json!({
            "type": "command",
            "command": desired_cmd
        }));

        if let Some(block) = blocks[catch_all_idx].as_object_mut() {
            block.insert("hooks".to_string(), Value::Array(hooks_array));
        }

        blocks
    }

    fn uninstall_hooks_at(hooks_path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        if !hooks_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(hooks_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;
        let mut merged = existing.clone();
        let Some(hooks_obj) = merged.get_mut("hooks") else {
            return Ok(None);
        };

        let mut changed = false;
        for event in ["PreToolUse", "PostToolUse"] {
            if let Some(blocks) = hooks_obj
                .get_mut(event)
                .and_then(|value| value.as_array_mut())
            {
                for block in blocks {
                    if let Some(hooks) = block
                        .get_mut("hooks")
                        .and_then(|value| value.as_array_mut())
                    {
                        let before = hooks.len();
                        hooks.retain(|hook| {
                            hook.get("command")
                                .and_then(|command| command.as_str())
                                .map(|command| !Self::is_trae_checkpoint_command(command))
                                .unwrap_or(true)
                        });
                        changed |= hooks.len() != before;
                    }
                }
            }
        }

        if !changed {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(hooks_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(hooks_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn hook_status_for_paths(
        paths: &[PathBuf],
        desired_cmd: &str,
    ) -> Result<(bool, bool), GitAiError> {
        let mut saw_relevant_path = false;
        let mut hooks_installed = false;
        let mut hooks_up_to_date = true;

        for hooks_path in paths {
            let parent_exists = hooks_path
                .parent()
                .map(|parent| parent.exists())
                .unwrap_or(false);
            if !hooks_path.exists() && !parent_exists {
                continue;
            }

            saw_relevant_path = true;
            if !hooks_path.exists() {
                hooks_up_to_date = false;
                continue;
            }

            let content = fs::read_to_string(hooks_path)?;
            let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
            let (path_hooks_installed, path_hooks_up_to_date) =
                Self::hook_status(&existing, desired_cmd);
            hooks_installed |= path_hooks_installed;
            hooks_up_to_date &= path_hooks_up_to_date;
        }

        if !saw_relevant_path {
            hooks_up_to_date = false;
        }

        Ok((hooks_installed, hooks_up_to_date))
    }
}

impl HookInstaller for TraeInstaller {
    fn name(&self) -> &str {
        "Trae"
    }

    fn id(&self) -> &str {
        "trae"
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let paths = Self::hooks_paths();
        if paths.is_empty() {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let desired_cmd = Self::hook_command(&params.binary_path);
        let (hooks_installed, hooks_up_to_date) =
            Self::hook_status_for_paths(&paths, &desired_cmd)?;

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["trae", "Trae", "TRAE", "Trae CN", "TraeCN", "TRAE CN"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let mut diffs = Vec::new();
        for hooks_path in Self::hooks_paths() {
            if let Some(diff) = Self::install_hooks_at(&hooks_path, params, dry_run)? {
                diffs.push(diff);
            }
        }

        if diffs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(diffs.join("\n")))
        }
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let mut diffs = Vec::new();
        for hooks_path in Self::hooks_paths() {
            if let Some(diff) = Self::uninstall_hooks_at(&hooks_path, dry_run)? {
                diffs.push(diff);
            }
        }

        if diffs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(diffs.join("\n")))
        }
    }

    fn install_extras(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Vec<InstallResult>, GitAiError> {
        let Some(cli) = resolve_editor_cli("trae") else {
            return Ok(vec![InstallResult {
                changed: false,
                diff: None,
                message: "Trae: Unable to install the user extension automatically. Install 'git-ai.git-ai-vscode' from Trae's extensions tab to enable human-save attribution outside managed-runtime regions."
                    .to_string(),
            }]);
        };

        match is_vsc_editor_extension_installed(&cli, "git-ai.git-ai-vscode") {
            Ok(true) => Ok(vec![InstallResult {
                changed: false,
                diff: None,
                message: "Trae: User extension already installed".to_string(),
            }]),
            Ok(false) if dry_run => Ok(vec![InstallResult {
                changed: true,
                diff: None,
                message: "Trae: Pending user extension install for human-save attribution"
                    .to_string(),
            }]),
            Ok(false) => match install_vsc_editor_extension(&cli, "git-ai.git-ai-vscode") {
                Ok(()) => Ok(vec![InstallResult {
                    changed: true,
                    diff: None,
                    message:
                        "Trae: User extension installed for region-independent human-save attribution"
                            .to_string(),
                }]),
                Err(error) => {
                    tracing::debug!(
                        "Trae: Error automatically installing user extension: {}",
                        error
                    );
                    Ok(vec![InstallResult {
                        changed: false,
                        diff: None,
                        message: "Trae: Unable to install the user extension automatically. Install 'git-ai.git-ai-vscode' from Trae's extensions tab to enable human-save attribution outside managed-runtime regions."
                            .to_string(),
                    }])
                }
            },
            Err(error) => Ok(vec![InstallResult {
                changed: false,
                diff: None,
                message: format!("Trae: Failed to check user extension: {}", error),
            }]),
        }
    }
}

#[cfg(feature = "test-support")]
pub fn render_trae_hook_command_for_test(binary_path: &Path) -> String {
    TraeInstaller::hook_command(binary_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let hooks_path = temp_dir.path().join(".trae-cn").join("hooks.json");
        fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        (temp_dir, hooks_path)
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: PathBuf::from("/usr/local/bin/git-ai"),
        }
    }

    fn expected_cmd() -> String {
        TraeInstaller::hook_command(&params().binary_path)
    }

    fn read_settings(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn catch_all_hooks<'a>(settings: &'a Value, event: &str) -> Vec<&'a Value> {
        settings
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(|value| value.as_array())
            .and_then(|blocks| {
                blocks.iter().find(|block| {
                    block
                        .get("matcher")
                        .and_then(|matcher| matcher.as_str())
                        .map(|matcher| matcher == TRAE_CATCH_ALL_MATCHER)
                        .unwrap_or(false)
                })
            })
            .and_then(|block| block.get("hooks").and_then(|hooks| hooks.as_array()))
            .map(|hooks| hooks.iter().collect())
            .unwrap_or_default()
    }

    fn event_commands<'a>(settings: &'a Value, event: &str) -> Vec<&'a str> {
        settings
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("hooks").and_then(|hooks| hooks.as_array()))
            .flatten()
            .filter_map(|hook| hook.get("command").and_then(|command| command.as_str()))
            .collect()
    }

    #[test]
    fn hook_command_uses_the_platform_runtime() {
        let binary = Path::new(r"C:\Users\Test User\.git-ai\bin\git-ai.exe");
        let expected_shell = if cfg!(windows) {
            HookShell::PowerShell
        } else {
            HookShell::Posix
        };

        assert_eq!(
            TraeInstaller::hook_command(binary),
            render_hook_command(binary, TRAE_HOOK_ARGS, expected_shell)
        );
    }

    #[test]
    fn process_names_include_trae_cn() {
        assert!(TraeInstaller.process_names().contains(&"Trae CN"));
        assert!(TraeInstaller.process_names().contains(&"TraeCN"));
    }

    #[test]
    fn fresh_install_creates_version_and_catch_all_hooks() {
        let (_temp_dir, hooks_path) = setup_test_env();
        fs::remove_file(&hooks_path).ok();

        let diff = TraeInstaller::install_hooks_at(&hooks_path, &params(), false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&hooks_path);
        assert_eq!(
            settings.get("version").and_then(|value| value.as_i64()),
            Some(1)
        );
        for event in ["PreToolUse", "PostToolUse"] {
            let hooks = catch_all_hooks(&settings, event);
            assert_eq!(hooks.len(), 1, "{event}: expected one catch-all hook");
            assert_eq!(
                hooks[0].get("command").and_then(|command| command.as_str()),
                Some(expected_cmd().as_str())
            );
        }
    }

    #[test]
    fn fresh_install_dry_run_does_not_create_config_directory() {
        let temp_dir = TempDir::new().unwrap();
        let hooks_path = temp_dir.path().join(".trae").join("hooks.json");

        assert!(
            TraeInstaller::install_hooks_at(&hooks_path, &params(), true)
                .unwrap()
                .is_some()
        );
        assert!(!hooks_path.parent().unwrap().exists());
    }

    #[test]
    fn hook_status_requires_pre_and_post_catch_all() {
        let settings = json!({
            "version": 1,
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/git-ai checkpoint trae --hook-input stdin"}]
                }]
            }
        });

        assert_eq!(
            TraeInstaller::hook_status(&settings, &expected_cmd()),
            (true, false)
        );
    }

    #[test]
    fn hooks_paths_keep_international_and_cn_products_separate() {
        let home = Path::new("/Users/admin");
        assert_eq!(
            TraeInstaller::hooks_paths_for_variants(home, true, false),
            vec![PathBuf::from("/Users/admin/.trae/hooks.json")]
        );
        assert_eq!(
            TraeInstaller::hooks_paths_for_variants(home, false, true),
            vec![PathBuf::from("/Users/admin/.trae-cn/hooks.json")]
        );
        assert_eq!(
            TraeInstaller::hooks_paths_for_variants(home, true, true),
            vec![
                PathBuf::from("/Users/admin/.trae/hooks.json"),
                PathBuf::from("/Users/admin/.trae-cn/hooks.json"),
            ]
        );
    }

    #[test]
    fn windows_app_candidates_cover_both_products_and_standard_roots() {
        let home = Path::new("/Users/admin");
        let roots = vec![PathBuf::from("/Program Files")];
        let international = TraeInstaller::windows_international_app_candidates(home, &roots);
        let cn = TraeInstaller::windows_cn_app_candidates(home, &roots);

        assert!(international.contains(&PathBuf::from("/Program Files/Trae/Trae.exe")));
        assert!(international.contains(&PathBuf::from("/Users/admin/.trae/Trae.exe")));
        assert!(cn.contains(&PathBuf::from("/Program Files/Trae CN/Trae CN.exe")));
        assert!(cn.contains(&PathBuf::from("/Program Files/Trae CN/TraeCN.exe")));
        assert!(cn.contains(&PathBuf::from("/Users/admin/.trae-cn/Trae CN.exe")));
    }

    #[test]
    fn tasklist_detection_distinguishes_trae_products() {
        let output = br#""Trae.exe","123","Console","1","100 K"
"Trae CN.exe","456","Console","1","100 K""#;
        assert!(TraeInstaller::tasklist_contains_trae_international(output));
        assert!(TraeInstaller::tasklist_contains_trae_cn(output));
        assert!(!TraeInstaller::tasklist_contains_trae_international(
            br#""Trae CN.exe","456","Console","1","100 K""#
        ));
    }

    #[test]
    fn hook_status_for_paths_requires_each_relevant_path() {
        let temp_dir = TempDir::new().unwrap();
        let documented_path = temp_dir.path().join(".trae-cn").join("hooks.json");
        let stable_path = temp_dir.path().join(".trae").join("hooks.json");
        fs::create_dir_all(documented_path.parent().unwrap()).unwrap();
        fs::create_dir_all(stable_path.parent().unwrap()).unwrap();

        TraeInstaller::install_hooks_at(&documented_path, &params(), false).unwrap();

        assert_eq!(
            TraeInstaller::hook_status_for_paths(&[documented_path, stable_path], &expected_cmd())
                .unwrap(),
            (true, false)
        );
    }

    #[test]
    fn legacy_git_bash_hooks_migrate_once_and_preserve_user_hooks() {
        let (_temp_dir, hooks_path) = setup_test_env();
        let params = HookInstallerParams {
            binary_path: PathBuf::from(r"C:\Users\Test User\.git-ai\bin\git-ai.exe"),
        };
        let legacy_cmd =
            render_hook_command(&params.binary_path, TRAE_HOOK_ARGS, HookShell::GitBash);
        let desired_cmd = TraeInstaller::hook_command(&params.binary_path);
        let mut settings = json!({ "version": 1, "hooks": {} });

        for event in ["PreToolUse", "PostToolUse"] {
            settings["hooks"][event] = json!([
                {
                    "matcher": "Write",
                    "hooks": [
                        {"type": "command", "command": legacy_cmd},
                        {"type": "command", "command": "echo user hook"}
                    ]
                },
                {
                    "matcher": "*",
                    "hooks": [
                        {"type": "command", "command": legacy_cmd},
                        {"type": "command", "command": "git-ai checkpoint custom-agent --hook-input stdin"}
                    ]
                }
            ]);
        }
        fs::write(&hooks_path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();

        assert_eq!(
            TraeInstaller::hook_status(&settings, &desired_cmd),
            (true, false),
            "legacy Git Bash commands must require an upgrade"
        );
        assert!(
            TraeInstaller::install_hooks_at(&hooks_path, &params, false)
                .unwrap()
                .is_some()
        );

        let migrated = read_settings(&hooks_path);
        for event in ["PreToolUse", "PostToolUse"] {
            let commands = event_commands(&migrated, event);
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| TraeInstaller::is_trae_checkpoint_command(command))
                    .count(),
                1,
                "{event}: expected exactly one Trae Hook"
            );
            assert!(commands.contains(&desired_cmd.as_str()), "{event}");
            assert!(commands.contains(&"echo user hook"), "{event}");
            assert!(
                commands.contains(&"git-ai checkpoint custom-agent --hook-input stdin"),
                "{event}"
            );
        }
        assert_eq!(
            TraeInstaller::hook_status(&migrated, &desired_cmd),
            (true, true)
        );
        assert!(
            TraeInstaller::install_hooks_at(&hooks_path, &params, false)
                .unwrap()
                .is_none(),
            "reinstalling an up-to-date config must be idempotent"
        );
    }

    #[test]
    fn uninstall_removes_legacy_and_current_trae_hooks_only() {
        let (_temp_dir, hooks_path) = setup_test_env();
        let params = HookInstallerParams {
            binary_path: PathBuf::from(r"C:\Users\Test User\.git-ai\bin\git-ai.exe"),
        };
        let legacy_cmd =
            render_hook_command(&params.binary_path, TRAE_HOOK_ARGS, HookShell::GitBash);
        let desired_cmd = TraeInstaller::hook_command(&params.binary_path);
        let mut settings = json!({ "version": 1, "hooks": {} });

        for event in ["PreToolUse", "PostToolUse"] {
            settings["hooks"][event] = json!([{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": legacy_cmd},
                    {"type": "command", "command": desired_cmd},
                    {"type": "command", "command": "echo user hook"},
                    {"type": "command", "command": "git-ai checkpoint custom-agent --hook-input stdin"}
                ]
            }]);
        }
        fs::write(&hooks_path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();

        assert!(
            TraeInstaller::uninstall_hooks_at(&hooks_path, false)
                .unwrap()
                .is_some()
        );
        let uninstalled = read_settings(&hooks_path);
        for event in ["PreToolUse", "PostToolUse"] {
            let commands = event_commands(&uninstalled, event);
            assert!(
                !commands
                    .iter()
                    .any(|command| TraeInstaller::is_trae_checkpoint_command(command)),
                "{event}"
            );
            assert!(commands.contains(&"echo user hook"), "{event}");
            assert!(
                commands.contains(&"git-ai checkpoint custom-agent --hook-input stdin"),
                "{event}"
            );
        }
    }
}
