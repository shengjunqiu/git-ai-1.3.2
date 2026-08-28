use crate::error::GitAiError;
use crate::mdm::command_line::{HookShell, platform_hook_shell, render_hook_command};
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
#[cfg(target_os = "windows")]
use crate::mdm::utils::windows_uninstall_display_name_exists;
use crate::mdm::utils::{
    binary_exists, generate_diff, home_dir, is_git_ai_checkpoint_command, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const CODEBUDDY_HOOK_ARGS: &[&str] = &["checkpoint", "codebuddy", "--hook-input", "stdin"];
const CODEBUDDY_CATCH_ALL_MATCHER: &str = "*";

pub struct CodeBuddyInstaller;

impl CodeBuddyInstaller {
    fn hook_command(binary_path: &Path) -> String {
        render_hook_command(
            binary_path,
            CODEBUDDY_HOOK_ARGS,
            platform_hook_shell(HookShell::CmdAndGitBash),
        )
    }

    fn is_codebuddy_checkpoint_command(command: &str) -> bool {
        is_git_ai_checkpoint_command(command) && command.contains("checkpoint codebuddy")
    }

    fn settings_path() -> PathBuf {
        home_dir().join(".codebuddy").join("settings.json")
    }

    fn config_dir() -> PathBuf {
        home_dir().join(".codebuddy")
    }

    fn tool_installed() -> bool {
        let home = home_dir();
        if Self::config_dir().exists() || home.join(".codebuddycn").exists() {
            return true;
        }

        #[cfg(target_os = "macos")]
        {
            [
                PathBuf::from("/Applications/CodeBuddy.app"),
                PathBuf::from("/Applications/CodeBuddy CN.app"),
                home.join("Applications").join("CodeBuddy.app"),
                home.join("Applications").join("CodeBuddy CN.app"),
            ]
            .iter()
            .any(|path| path.exists())
                || binary_exists("codebuddy")
        }

        #[cfg(target_os = "windows")]
        {
            let roots = Self::windows_app_roots();
            let tasklist = Self::windows_tasklist();
            Self::windows_app_candidates(&home, &roots)
                .iter()
                .any(|path| path.exists())
                || ["codebuddy", "CodeBuddy", "codebuddy-cn", "CodeBuddyCN"]
                    .iter()
                    .any(|name| binary_exists(name))
                || tasklist
                    .as_deref()
                    .is_some_and(Self::tasklist_contains_codebuddy)
                || windows_uninstall_display_name_exists(&[
                    "CodeBuddy",
                    "CodeBuddy IDE",
                    "CodeBuddy CN",
                    "CodeBuddyCN",
                    "CodeBuddy-CN",
                ])
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            binary_exists("codebuddy") || binary_exists("codebuddy-cn")
        }
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
    fn windows_app_candidates(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        for (config_dir, executable, install_dirs) in [
            (
                ".codebuddy",
                "CodeBuddy.exe",
                &["CodeBuddy", "CodeBuddy IDE"][..],
            ),
            (
                ".codebuddycn",
                "CodeBuddy CN.exe",
                &["CodeBuddy CN", "CodeBuddyCN"][..],
            ),
            (
                ".codebuddycn",
                "CodeBuddyCN.exe",
                &["CodeBuddy CN", "CodeBuddyCN"][..],
            ),
        ] {
            candidates.push(home.join(config_dir).join(executable));
            for install_dir in install_dirs {
                candidates.push(
                    home.join("AppData")
                        .join("Local")
                        .join("Programs")
                        .join(install_dir)
                        .join(executable),
                );
                for root in roots {
                    candidates.push(root.join("Programs").join(install_dir).join(executable));
                    candidates.push(root.join(install_dir).join(executable));
                }
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
    fn tasklist_contains_codebuddy(output: &[u8]) -> bool {
        String::from_utf8_lossy(output).lines().any(|line| {
            line.split(',')
                .next()
                .map(|image| image.trim().trim_matches('"'))
                .is_some_and(|image| {
                    [
                        "codebuddy",
                        "codebuddy.exe",
                        "codebuddy cn",
                        "codebuddy cn.exe",
                        "codebuddycn",
                        "codebuddycn.exe",
                    ]
                    .iter()
                    .any(|candidate| image.eq_ignore_ascii_case(candidate))
                })
        })
    }

    fn hook_status(settings: &Value, desired_cmd: &str) -> (bool, bool) {
        let hooks_installed = ["PreToolUse", "PostToolUse"]
            .iter()
            .any(|event| Self::event_has_codebuddy_hook(settings, event));
        let hooks_up_to_date = ["PreToolUse", "PostToolUse"]
            .iter()
            .all(|event| Self::event_hook_is_up_to_date(settings, event, desired_cmd));

        (hooks_installed, hooks_up_to_date)
    }

    fn event_has_codebuddy_hook(settings: &Value, event: &str) -> bool {
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
                            .map(Self::is_codebuddy_checkpoint_command)
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

        let mut codebuddy_hook_count = 0;
        let mut desired_catch_all_count = 0;

        for block in blocks {
            let is_catch_all =
                block.get("matcher").and_then(Value::as_str) == Some(CODEBUDDY_CATCH_ALL_MATCHER);
            let Some(hooks) = block.get("hooks").and_then(Value::as_array) else {
                continue;
            };

            for hook in hooks {
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if Self::is_codebuddy_checkpoint_command(command) {
                    codebuddy_hook_count += 1;
                    if is_catch_all && command == desired_cmd {
                        desired_catch_all_count += 1;
                    }
                }
            }
        }

        codebuddy_hook_count == 1 && desired_catch_all_count == 1
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
            let hook_type_array = hooks_obj
                .get(event)
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            let hook_type_array = Self::merge_event_hooks(hook_type_array, desired_cmd);

            if let Some(obj) = hooks_obj.as_object_mut() {
                obj.insert(event.to_string(), Value::Array(hook_type_array));
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
                        .map(|command| !Self::is_codebuddy_checkpoint_command(command))
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
                    .map(|matcher| matcher == CODEBUDDY_CATCH_ALL_MATCHER)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| {
                blocks.push(json!({
                    "matcher": CODEBUDDY_CATCH_ALL_MATCHER,
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
                                .map(|command| !Self::is_codebuddy_checkpoint_command(command))
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

impl HookInstaller for CodeBuddyInstaller {
    fn name(&self) -> &str {
        "CodeBuddy"
    }

    fn id(&self) -> &str {
        "codebuddy"
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        if !Self::tool_installed() {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let settings_path = Self::settings_path();
        if !settings_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&settings_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
        let desired_cmd = Self::hook_command(&params.binary_path);
        let (hooks_installed, hooks_up_to_date) = Self::hook_status(&existing, &desired_cmd);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["codebuddy", "CodeBuddy", "CodeBuddy CN", "CodeBuddyCN"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::install_hooks_at(&Self::settings_path(), params, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::uninstall_hooks_at(&Self::settings_path(), dry_run)
    }
}

#[cfg(feature = "test-support")]
pub fn render_codebuddy_hook_command_for_test(binary_path: &Path) -> String {
    CodeBuddyInstaller::hook_command(binary_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".codebuddy").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        (temp_dir, settings_path)
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: PathBuf::from("/usr/local/bin/git-ai"),
        }
    }

    fn expected_cmd() -> String {
        CodeBuddyInstaller::hook_command(&params().binary_path)
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
                        .map(|matcher| matcher == CODEBUDDY_CATCH_ALL_MATCHER)
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
            CodeBuddyInstaller::hook_command(binary),
            render_hook_command(binary, CODEBUDDY_HOOK_ARGS, expected_shell)
        );
    }

    #[test]
    fn process_names_include_codebuddy_cn() {
        assert!(CodeBuddyInstaller.process_names().contains(&"CodeBuddy CN"));
        assert!(CodeBuddyInstaller.process_names().contains(&"CodeBuddyCN"));
    }

    #[test]
    fn fresh_install_creates_pre_and_post_catch_all_hooks() {
        let (_temp_dir, settings_path) = setup_test_env();
        fs::remove_file(&settings_path).ok();

        let diff = CodeBuddyInstaller::install_hooks_at(&settings_path, &params(), false).unwrap();
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
        let settings_path = temp_dir.path().join(".codebuddy").join("settings.json");

        assert!(
            CodeBuddyInstaller::install_hooks_at(&settings_path, &params(), true)
                .unwrap()
                .is_some()
        );
        assert!(!settings_path.parent().unwrap().exists());
    }

    #[test]
    fn windows_app_candidates_cover_international_and_cn_products() {
        let home = Path::new("/Users/admin");
        let candidates =
            CodeBuddyInstaller::windows_app_candidates(home, &[PathBuf::from("/Program Files")]);

        assert!(candidates.contains(&PathBuf::from("/Program Files/CodeBuddy/CodeBuddy.exe")));
        assert!(candidates.contains(&PathBuf::from(
            "/Program Files/CodeBuddy CN/CodeBuddy CN.exe"
        )));
        assert!(candidates.contains(&PathBuf::from(
            "/Program Files/CodeBuddy CN/CodeBuddyCN.exe"
        )));
        assert!(candidates.contains(&PathBuf::from("/Users/admin/.codebuddy/CodeBuddy.exe")));
        assert!(candidates.contains(&PathBuf::from("/Users/admin/.codebuddycn/CodeBuddy CN.exe")));
    }

    #[test]
    fn tasklist_detection_recognizes_both_codebuddy_products() {
        assert!(CodeBuddyInstaller::tasklist_contains_codebuddy(
            br#""CodeBuddy.exe","123","Console","1","100 K""#
        ));
        assert!(CodeBuddyInstaller::tasklist_contains_codebuddy(
            br#""CodeBuddy CN.exe","456","Console","1","100 K""#
        ));
        assert!(!CodeBuddyInstaller::tasklist_contains_codebuddy(
            br#""CodeBuddy Helper.exe","789","Console","1","100 K""#
        ));
    }

    #[test]
    fn hook_status_requires_pre_and_post_catch_all() {
        let settings = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/git-ai checkpoint codebuddy --hook-input stdin"}]
                }]
            }
        });

        assert_eq!(
            CodeBuddyInstaller::hook_status(&settings, &expected_cmd()),
            (true, false)
        );
    }

    #[test]
    fn install_preserves_user_hooks_and_moves_git_ai_to_catch_all() {
        let (_temp_dir, settings_path) = setup_test_env();
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Edit|Write",
                        "hooks": [
                            {"type": "command", "command": "echo user-hook"},
                            {"type": "command", "command": "/old/git-ai checkpoint codebuddy --hook-input stdin"}
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        CodeBuddyInstaller::install_hooks_at(&settings_path, &params(), false).unwrap();
        let settings = read_settings(&settings_path);

        let post_blocks = settings["hooks"]["PostToolUse"].as_array().unwrap();
        let old_block = post_blocks
            .iter()
            .find(|block| {
                block.get("matcher").and_then(|matcher| matcher.as_str()) == Some("Edit|Write")
            })
            .unwrap();
        let old_hooks = old_block["hooks"].as_array().unwrap();
        assert_eq!(old_hooks.len(), 1);
        assert_eq!(
            old_hooks[0]
                .get("command")
                .and_then(|command| command.as_str()),
            Some("echo user-hook")
        );

        let catch_all = catch_all_hooks(&settings, "PostToolUse");
        assert_eq!(catch_all.len(), 1);
        assert_eq!(
            catch_all[0]
                .get("command")
                .and_then(|command| command.as_str()),
            Some(expected_cmd().as_str())
        );
    }

    #[test]
    fn legacy_git_bash_hooks_migrate_once_and_preserve_other_hooks() {
        let (_temp_dir, settings_path) = setup_test_env();
        let params = HookInstallerParams {
            binary_path: PathBuf::from(r"C:\Users\Test User\.git-ai\bin\git-ai.exe"),
        };
        let legacy_cmd =
            render_hook_command(&params.binary_path, CODEBUDDY_HOOK_ARGS, HookShell::GitBash);
        let desired_cmd = CodeBuddyInstaller::hook_command(&params.binary_path);
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
            CodeBuddyInstaller::hook_status(&settings, &desired_cmd),
            (true, false)
        );
        assert!(
            CodeBuddyInstaller::install_hooks_at(&settings_path, &params, false)
                .unwrap()
                .is_some()
        );

        let migrated = read_settings(&settings_path);
        for event in ["PreToolUse", "PostToolUse"] {
            let commands = event_commands(&migrated, event);
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| CodeBuddyInstaller::is_codebuddy_checkpoint_command(command))
                    .count(),
                1,
                "{event}: expected exactly one CodeBuddy Hook"
            );
            assert!(commands.contains(&desired_cmd.as_str()), "{event}");
            assert!(commands.contains(&"echo user hook"), "{event}");
            assert!(
                commands.contains(&"git-ai checkpoint custom-agent --hook-input stdin"),
                "{event}"
            );
        }
        assert_eq!(
            CodeBuddyInstaller::hook_status(&migrated, &desired_cmd),
            (true, true)
        );
        assert!(
            CodeBuddyInstaller::install_hooks_at(&settings_path, &params, false)
                .unwrap()
                .is_none(),
            "reinstalling an up-to-date config must be idempotent"
        );
    }

    #[test]
    fn uninstall_removes_codebuddy_hooks_only() {
        let (_temp_dir, settings_path) = setup_test_env();
        let params = HookInstallerParams {
            binary_path: PathBuf::from(r"C:\Users\Test User\.git-ai\bin\git-ai.exe"),
        };
        let legacy_cmd =
            render_hook_command(&params.binary_path, CODEBUDDY_HOOK_ARGS, HookShell::GitBash);
        let desired_cmd = CodeBuddyInstaller::hook_command(&params.binary_path);
        let mut settings = json!({ "hooks": {} });

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
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(
            CodeBuddyInstaller::uninstall_hooks_at(&settings_path, false)
                .unwrap()
                .is_some()
        );
        let uninstalled = read_settings(&settings_path);
        for event in ["PreToolUse", "PostToolUse"] {
            let commands = event_commands(&uninstalled, event);
            assert!(
                !commands
                    .iter()
                    .any(|command| CodeBuddyInstaller::is_codebuddy_checkpoint_command(command)),
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
