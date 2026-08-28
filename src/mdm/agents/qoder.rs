use crate::error::GitAiError;
use crate::mdm::command_line::{HookShell, platform_hook_shell, render_hook_command};
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
#[cfg(not(target_os = "macos"))]
use crate::mdm::utils::binary_exists;
#[cfg(target_os = "windows")]
use crate::mdm::utils::windows_uninstall_display_name_exists;
use crate::mdm::utils::{generate_diff, home_dir, is_git_ai_checkpoint_command, write_atomic};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const QODER_HOOK_ARGS: &[&str] = &["checkpoint", "qoder", "--hook-input", "stdin"];
const QODER_CATCH_ALL_MATCHER: &str = "*";

pub struct QoderInstaller;

impl QoderInstaller {
    fn hook_command(binary_path: &Path) -> String {
        render_hook_command(
            binary_path,
            QODER_HOOK_ARGS,
            platform_hook_shell(HookShell::CmdAndGitBash),
        )
    }

    fn is_qoder_checkpoint_command(command: &str) -> bool {
        is_git_ai_checkpoint_command(command) && command.contains("checkpoint qoder")
    }

    fn config_dir() -> PathBuf {
        home_dir().join(".qoder")
    }

    fn cn_config_dir() -> PathBuf {
        home_dir().join(".qoder-cn")
    }

    fn settings_paths_for_variants(
        home: &Path,
        international_installed: bool,
        cn_installed: bool,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if international_installed {
            paths.push(home.join(".qoder").join("settings.json"));
        }
        if cn_installed {
            paths.push(home.join(".qoder-cn").join("settings.json"));
        }
        paths
    }

    fn settings_paths() -> Vec<PathBuf> {
        let home = home_dir();
        let international_config_exists = Self::config_dir().exists();
        let cn_config_exists = Self::cn_config_dir().exists();

        #[cfg(target_os = "macos")]
        let international_installed = international_config_exists
            || [
                PathBuf::from("/Applications/Qoder.app"),
                PathBuf::from("/Applications/Qoder IDE.app"),
                home.join("Applications").join("Qoder.app"),
                home.join("Applications").join("Qoder IDE.app"),
            ]
            .iter()
            .any(|path| path.exists());
        #[cfg(target_os = "macos")]
        let cn_installed = cn_config_exists;

        #[cfg(target_os = "windows")]
        let (international_installed, cn_installed) = {
            let roots = Self::windows_app_roots();
            let tasklist = Self::windows_tasklist();
            let (registry_international, registry_cn) = Self::windows_registry_variants();
            (
                international_config_exists
                    || Self::windows_international_app_candidates(&home, &roots)
                        .iter()
                        .any(|path| path.exists())
                    || ["qoder", "Qoder"].iter().any(|name| binary_exists(name))
                    || tasklist
                        .as_deref()
                        .is_some_and(Self::tasklist_contains_qoder_international)
                    || registry_international,
                cn_config_exists
                    || Self::windows_cn_app_candidates(&home, &roots)
                        .iter()
                        .any(|path| path.exists())
                    || ["qoder-cn", "QoderCN"]
                        .iter()
                        .any(|name| binary_exists(name))
                    || tasklist
                        .as_deref()
                        .is_some_and(Self::tasklist_contains_qoder_cn)
                    || registry_cn,
            )
        };

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let international_installed = international_config_exists || binary_exists("qoder");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let cn_installed = cn_config_exists;

        Self::settings_paths_for_variants(&home, international_installed, cn_installed)
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

    #[cfg(test)]
    fn windows_app_candidates(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut candidates = Self::windows_international_app_candidates(home, roots);
        candidates.extend(Self::windows_cn_app_candidates(home, roots));
        candidates.sort();
        candidates.dedup();
        candidates
    }

    #[cfg(any(test, target_os = "windows"))]
    fn windows_international_app_candidates(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
        Self::windows_variant_app_candidates(
            home,
            roots,
            ".qoder",
            "Qoder.exe",
            &["Qoder", "Qoder IDE"],
        )
    }

    #[cfg(any(test, target_os = "windows"))]
    fn windows_cn_app_candidates(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
        Self::windows_variant_app_candidates(
            home,
            roots,
            ".qoder-cn",
            "QoderCN.exe",
            &["QoderCN", "Qoder CN"],
        )
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

    #[cfg(target_os = "windows")]
    fn windows_registry_variants() -> (bool, bool) {
        (
            windows_uninstall_display_name_exists(&["Qoder", "Qoder IDE"]),
            windows_uninstall_display_name_exists(&["Qoder CN", "QoderCN", "Qoder-CN"]),
        )
    }

    #[cfg(test)]
    fn tasklist_contains_qoder(output: &[u8]) -> bool {
        Self::tasklist_contains_qoder_international(output)
            || Self::tasklist_contains_qoder_cn(output)
    }

    #[cfg(any(test, target_os = "windows"))]
    fn tasklist_contains_qoder_international(output: &[u8]) -> bool {
        Self::tasklist_contains_any(output, &["qoder", "qoder.exe"])
    }

    #[cfg(any(test, target_os = "windows"))]
    fn tasklist_contains_qoder_cn(output: &[u8]) -> bool {
        Self::tasklist_contains_any(
            output,
            &["qodercn", "qodercn.exe", "qoder-cn", "qoder-cn.exe"],
        )
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
            .any(|event| Self::event_has_qoder_hook(settings, event));
        let hooks_up_to_date = ["PreToolUse", "PostToolUse"]
            .iter()
            .all(|event| Self::event_hook_is_up_to_date(settings, event, desired_cmd));

        (hooks_installed, hooks_up_to_date)
    }

    fn event_has_qoder_hook(settings: &Value, event: &str) -> bool {
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
                            .map(Self::is_qoder_checkpoint_command)
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
            .and_then(Value::as_array)
        else {
            return false;
        };

        let mut qoder_hook_count = 0;
        let mut desired_catch_all_count = 0;
        for block in blocks {
            let is_catch_all =
                block.get("matcher").and_then(Value::as_str) == Some(QODER_CATCH_ALL_MATCHER);
            let Some(hooks) = block.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for hook in hooks {
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if Self::is_qoder_checkpoint_command(command) {
                    qoder_hook_count += 1;
                    if is_catch_all && command == desired_cmd {
                        desired_catch_all_count += 1;
                    }
                }
            }
        }

        qoder_hook_count == 1 && desired_catch_all_count == 1
    }

    fn install_hooks_at(
        settings_path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if !dry_run && let Some(dir) = settings_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if settings_path.exists() {
            fs::read_to_string(settings_path)?
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
        let diff_output = generate_diff(settings_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(settings_path, new_content.as_bytes())?;
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
                        .map(|command| !Self::is_qoder_checkpoint_command(command))
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
                    .map(|matcher| matcher == QODER_CATCH_ALL_MATCHER)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| {
                blocks.push(json!({
                    "matcher": QODER_CATCH_ALL_MATCHER,
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

    fn uninstall_hooks_at(
        settings_path: &Path,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if !settings_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(settings_path)?;
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
                                .map(|command| !Self::is_qoder_checkpoint_command(command))
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
        let diff_output = generate_diff(settings_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(settings_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

impl HookInstaller for QoderInstaller {
    fn name(&self) -> &str {
        "Qoder"
    }

    fn id(&self) -> &str {
        "qoder"
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let settings_paths = Self::settings_paths();
        if settings_paths.is_empty() {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let desired_cmd = Self::hook_command(&params.binary_path);
        let mut hooks_installed = false;
        let mut hooks_up_to_date = true;
        for settings_path in settings_paths {
            if !settings_path.exists() {
                hooks_up_to_date = false;
                continue;
            }
            let content = fs::read_to_string(&settings_path)?;
            let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
            let (path_installed, path_up_to_date) = Self::hook_status(&existing, &desired_cmd);
            hooks_installed |= path_installed;
            hooks_up_to_date &= path_up_to_date;
        }

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["qoder", "Qoder", "QoderCN", "Qoder CN"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let mut diffs = Vec::new();
        for settings_path in Self::settings_paths() {
            if let Some(diff) = Self::install_hooks_at(&settings_path, params, dry_run)? {
                diffs.push(diff);
            }
        }
        Ok((!diffs.is_empty()).then(|| diffs.join("\n")))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let mut diffs = Vec::new();
        for settings_path in Self::settings_paths() {
            if let Some(diff) = Self::uninstall_hooks_at(&settings_path, dry_run)? {
                diffs.push(diff);
            }
        }
        Ok((!diffs.is_empty()).then(|| diffs.join("\n")))
    }
}

#[cfg(feature = "test-support")]
pub fn render_qoder_hook_command_for_test(binary_path: &Path) -> String {
    QoderInstaller::hook_command(binary_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".qoder").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        (temp_dir, settings_path)
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: PathBuf::from("/usr/local/bin/git-ai"),
        }
    }

    fn expected_cmd() -> String {
        QoderInstaller::hook_command(&params().binary_path)
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
                        .map(|matcher| matcher == QODER_CATCH_ALL_MATCHER)
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
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter_map(|hook| hook.get("command").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn hook_command_uses_the_platform_runtime() {
        let binary = Path::new(r"C:\Users\Test User\.git-ai\bin\git-ai.exe");
        let expected_shell = if cfg!(windows) {
            HookShell::CmdAndGitBash
        } else {
            HookShell::Posix
        };

        assert_eq!(
            QoderInstaller::hook_command(binary),
            render_hook_command(binary, QODER_HOOK_ARGS, expected_shell)
        );
    }

    #[test]
    fn process_names_include_qoder_cn() {
        assert!(QoderInstaller.process_names().contains(&"QoderCN"));
    }

    #[test]
    fn settings_paths_keep_international_and_cn_configs_separate() {
        let home = Path::new("/Users/admin");
        assert_eq!(
            QoderInstaller::settings_paths_for_variants(home, true, false),
            vec![PathBuf::from("/Users/admin/.qoder/settings.json")]
        );
        assert_eq!(
            QoderInstaller::settings_paths_for_variants(home, false, true),
            vec![PathBuf::from("/Users/admin/.qoder-cn/settings.json")]
        );
        assert_eq!(
            QoderInstaller::settings_paths_for_variants(home, true, true),
            vec![
                PathBuf::from("/Users/admin/.qoder/settings.json"),
                PathBuf::from("/Users/admin/.qoder-cn/settings.json")
            ]
        );
    }

    #[test]
    fn fresh_install_creates_pre_and_post_catch_all_hooks() {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".qoder").join("settings.json");
        assert!(!settings_path.parent().unwrap().exists());

        let diff = QoderInstaller::install_hooks_at(&settings_path, &params(), false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&settings_path);
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
        let settings_path = temp_dir.path().join(".qoder").join("settings.json");

        let diff = QoderInstaller::install_hooks_at(&settings_path, &params(), true).unwrap();

        assert!(diff.is_some());
        assert!(!settings_path.parent().unwrap().exists());
        assert!(!settings_path.exists());
    }

    #[test]
    fn cn_install_preserves_existing_product_settings() {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".qoder-cn").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&json!({
                "enabledPlugins": {
                    "qoder-create-plugin@qoder-bundler": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            QoderInstaller::install_hooks_at(&settings_path, &params(), false)
                .unwrap()
                .is_some()
        );
        let settings = read_settings(&settings_path);
        assert_eq!(
            settings["enabledPlugins"]["qoder-create-plugin@qoder-bundler"],
            true
        );
        for event in ["PreToolUse", "PostToolUse"] {
            assert_eq!(catch_all_hooks(&settings, event).len(), 1, "{event}");
        }
    }

    #[test]
    fn hook_status_requires_pre_and_post_catch_all() {
        let settings = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/git-ai checkpoint qoder --hook-input stdin"}]
                }]
            }
        });

        assert_eq!(
            QoderInstaller::hook_status(&settings, &expected_cmd()),
            (true, false)
        );
    }

    #[test]
    fn windows_app_candidates_cover_per_user_and_machine_installs() {
        let home = PathBuf::from("/Users/admin");
        let roots = vec![
            PathBuf::from("/Users/admin/AppData/Local"),
            PathBuf::from("/Program Files"),
        ];
        let candidates = QoderInstaller::windows_app_candidates(&home, &roots);

        assert!(candidates.contains(&PathBuf::from(
            "/Users/admin/AppData/Local/Programs/Qoder/Qoder.exe"
        )));
        assert!(candidates.contains(&PathBuf::from("/Program Files/Qoder/Qoder.exe")));
        assert!(candidates.contains(&PathBuf::from(
            "/Users/admin/AppData/Local/Programs/QoderCN/QoderCN.exe"
        )));
        assert!(candidates.contains(&PathBuf::from("/Program Files/Qoder CN/QoderCN.exe")));
        assert!(candidates.contains(&PathBuf::from("/Users/admin/.qoder/Qoder.exe")));
        assert!(candidates.contains(&PathBuf::from("/Users/admin/.qoder-cn/QoderCN.exe")));
    }

    #[test]
    fn tasklist_detection_recognizes_qoder_process() {
        let output = br#""Qoder.exe","1234","Console","1","100,000 K"
"QoderCN.exe","2345","Console","1","100,000 K"
"qoder-cn","3456","Console","1","100,000 K"
"powershell.exe","5678","Console","1","50,000 K""#;
        assert!(QoderInstaller::tasklist_contains_qoder(output));
        assert!(QoderInstaller::tasklist_contains_qoder_international(
            output
        ));
        assert!(QoderInstaller::tasklist_contains_qoder_cn(output));
        assert!(QoderInstaller::tasklist_contains_qoder(
            br#""QoderCN","2345","Console","1","100,000 K""#
        ));
        assert!(!QoderInstaller::tasklist_contains_qoder_international(
            br#""QoderCN.exe","2345","Console","1","100,000 K""#
        ));
        assert!(!QoderInstaller::tasklist_contains_qoder(
            br#""powershell.exe","5678","Console","1","50,000 K""#
        ));
    }

    #[test]
    fn legacy_hooks_migrate_once_and_preserve_other_hooks() {
        let (_temp_dir, settings_path) = setup_test_env();
        let params = HookInstallerParams {
            binary_path: PathBuf::from(r"C:\Users\Test User\.git-ai\bin\git-ai.exe"),
        };
        let legacy_cmd = render_hook_command(&params.binary_path, QODER_HOOK_ARGS, HookShell::Cmd);
        let desired_cmd = QoderInstaller::hook_command(&params.binary_path);
        let mut settings = json!({ "hooks": {} });

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
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert_eq!(
            QoderInstaller::hook_status(&settings, &desired_cmd),
            (true, false)
        );
        assert!(
            QoderInstaller::install_hooks_at(&settings_path, &params, false)
                .unwrap()
                .is_some()
        );

        let migrated = read_settings(&settings_path);
        for event in ["PreToolUse", "PostToolUse"] {
            let commands = event_commands(&migrated, event);
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| QoderInstaller::is_qoder_checkpoint_command(command))
                    .count(),
                1,
                "{event}: expected exactly one Qoder Hook"
            );
            assert!(commands.contains(&desired_cmd.as_str()), "{event}");
            assert!(commands.contains(&"echo user hook"), "{event}");
            assert!(
                commands.contains(&"git-ai checkpoint custom-agent --hook-input stdin"),
                "{event}"
            );
        }
        assert_eq!(
            QoderInstaller::hook_status(&migrated, &desired_cmd),
            (true, true)
        );
        assert!(
            QoderInstaller::install_hooks_at(&settings_path, &params, false)
                .unwrap()
                .is_none(),
            "reinstalling an up-to-date config must be idempotent"
        );
    }

    #[test]
    fn uninstall_removes_qoder_hooks_only() {
        let (_temp_dir, settings_path) = setup_test_env();
        let params = HookInstallerParams {
            binary_path: PathBuf::from(r"C:\Users\Test User\.git-ai\bin\git-ai.exe"),
        };
        let desired_cmd = QoderInstaller::hook_command(&params.binary_path);
        let mut settings = json!({ "hooks": {} });
        for event in ["PreToolUse", "PostToolUse"] {
            settings["hooks"][event] = json!([{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": desired_cmd},
                    {"type": "command", "command": "echo user hook"},
                    {"type": "command", "command": "git-ai checkpoint custom-agent --hook-input stdin"}
                ]
            }]);
        }
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(
            QoderInstaller::uninstall_hooks_at(&settings_path, false)
                .unwrap()
                .is_some()
        );
        let uninstalled = read_settings(&settings_path);
        for event in ["PreToolUse", "PostToolUse"] {
            let commands = event_commands(&uninstalled, event);
            assert!(
                !commands
                    .iter()
                    .any(|command| QoderInstaller::is_qoder_checkpoint_command(command)),
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
