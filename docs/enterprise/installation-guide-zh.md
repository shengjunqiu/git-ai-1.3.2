# Git AI 企业版安装与使用指南

本文适用于公司内部 Git AI CLI，支持：

- macOS：Apple Silicon（M1/M2/M3/M4）和 Intel
- Windows：x64 和 ARM64
- Linux：x64 和 ARM64

文中的 `https://git-ai.example.com` 是部署示例。管理员必须把它替换为
服务端 `BASE_URL` 配置的同一个 HTTPS Origin。不要从聊天、邮件或未经验证的
网页复制其他下载地址，也不要把 HTTP 下载内容直接通过管道交给 shell。
安装脚本会继续校验客户端二进制；下面的步骤还会先用同源 `SHA256SUMS`
校验安装脚本本身。发布签名尚未实现，参见前端安全审计中的后续任务。

## 最快上手：安装与使用步骤总结

如果只想快速完成安装并开始使用，请按以下步骤操作；遇到问题时再查看后文对应章节。

### 第 1 步：确认环境并注册账号

1. 执行 `git --version`，确认电脑已经安装 Git。
2. 在浏览器中打开 <https://git-ai.example.com/auth/register>，使用公司企业邮箱注册账号，并保持浏览器登录状态。

### 第 2 步：安装 Git AI

macOS/Linux 先下载脚本和校验清单、核对 SHA256，再执行：

```bash
base="https://git-ai.example.com/worker/releases/latest/download"
curl -fSLo install.sh "$base/install.sh"
curl -fSLo SHA256SUMS "$base/SHA256SUMS"
# Linux 使用 sha256sum -c -；macOS 使用 shasum -a 256 -c -
grep '  install.sh$' SHA256SUMS | sha256sum -c -
bash ./install.sh
```

Windows 同样先校验下载后的脚本：

```powershell
$base = "https://git-ai.example.com/worker/releases/latest/download"
Invoke-WebRequest "$base/install.ps1" -OutFile install.ps1
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS
$expected = ((Select-String -Path SHA256SUMS -Pattern "  install.ps1$").Line -split "\s+")[0].ToLower()
$actual = (Get-FileHash .\install.ps1 -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) { throw "install.ps1 SHA256 mismatch" }
& .\install.ps1
```

安装完成后重新打开终端或 PowerShell，并确认安装成功：

```bash
git-ai --version
```

### 第 3 步：登录并授权

```bash
git-ai login --server https://git-ai.example.com
git-ai whoami
```

第一条命令会打开浏览器，请登录并确认授权；第二条命令能显示当前账号即表示授权成功。

### 第 4 步：安装并配置编辑器扩展

1. 下载并安装扩展：<https://git-ai.example.com/static/downloads/git-ai-extension.vsix>
2. 在编辑器设置中开启：

```json
{
  "gitai.enableCheckpointLogging": true,
  "gitai.experiments.aiTabTracking": true
}
```

使用 Trae 时，还需要在 Hooks 设置中手动开启“已配置的 Hooks”。如果扩展或 Hooks 没有自动生效，请执行：

```bash
git-ai install-hooks
```

### 第 5 步：开始使用

无需在每个 Git 仓库中单独初始化。直接使用支持的 AI 工具编辑代码，然后照常执行 `git add`、`git commit` 等 Git 命令；提交时会自动上传 AI 与人工代码归属统计。

查看当前归属统计：

```bash
git-ai status
```

至此即可正常使用。需要更新时执行 `git-ai update`，遇到问题时执行 `git-ai debug` 并将输出发给管理员。


## 1. 安装前准备

安装前请确认：

1. 电脑已经安装 Git。
2. 电脑可以访问公司 Git AI 服务器。
3. 你可以使用浏览器完成账号注册或登录授权。


### 检查 Git

macOS/Linux：

```bash
git --version
```

Windows PowerShell：

```powershell
git --version
```

能够看到类似 `git version 2.x.x` 的输出即可。如果提示找不到 Git，请先安装 Git，再继续安装 Git AI。

### 检查服务器连接

macOS/Linux：

```bash
curl -fsS https://git-ai.example.com/health
```

Windows PowerShell：

```powershell
Invoke-RestMethod https://git-ai.example.com/health
```

服务正常时会返回 `status: ok`。

### 注册账号

在浏览器中打开：

<https://git-ai.example.com/auth/register>

按照页面提示填写姓名、公司企业邮箱和密码，并选择所属组织与部门。注册成功后请保持浏览器登录状态，后续 CLI 授权会直接使用当前账号



## 2. macOS 安装

打开“终端”，下载并校验脚本后执行：

```bash
base="https://git-ai.example.com/worker/releases/latest/download"
curl -fSLo install.sh "$base/install.sh"
curl -fSLo SHA256SUMS "$base/SHA256SUMS"
grep '  install.sh$' SHA256SUMS | shasum -a 256 -c -
bash ./install.sh
```

安装程序会自动识别 Apple Silicon 或 Intel 处理器，不需要手动选择安装包。

安装完成后，关闭并重新打开终端，然后执行：

```bash
git-ai --version
```

## 3. Linux 安装

打开终端，下载并校验脚本后执行：

```bash
base="https://git-ai.example.com/worker/releases/latest/download"
curl -fSLo install.sh "$base/install.sh"
curl -fSLo SHA256SUMS "$base/SHA256SUMS"
grep '  install.sh$' SHA256SUMS | sha256sum -c -
bash ./install.sh
```

安装程序会自动识别 x64 或 ARM64 架构。

安装完成后，重新打开终端，或者执行：

```bash
export PATH="$HOME/.git-ai/bin:$PATH"
```

然后检查版本：

```bash
git-ai --version
```

## 4. Windows 安装

打开 PowerShell，下载并校验脚本后执行：

```powershell
$base = "https://git-ai.example.com/worker/releases/latest/download"
Invoke-WebRequest "$base/install.ps1" -OutFile install.ps1
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS
$expected = ((Select-String -Path SHA256SUMS -Pattern "  install.ps1$").Line -split "\s+")[0].ToLower()
$actual = (Get-FileHash .\install.ps1 -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) { throw "install.ps1 SHA256 mismatch" }
& .\install.ps1
```

安装程序会自动识别 x64 或 ARM64 架构。

安装完成后，关闭并重新打开 PowerShell，然后执行：

```powershell
git-ai --version
```

如果 PowerShell 提示禁止运行脚本，可以先在当前窗口执行：

```powershell
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
```

然后重新执行安装命令。该设置只对当前 PowerShell 窗口有效。

## 5. 登录和网页授权

使用本文中的命令安装完成后，请执行以下命令开始登录授权：

```bash
git-ai login --server https://git-ai.example.com
```

命令会打开浏览器。如果还没有账号，请在网页中注册；已有账号则直接登录并确认授权。企业登录页面是：

<https://git-ai.example.com/login>


在没有图形界面的 Linux 服务器上，可以执行：

```bash
git-ai login --server https://git-ai.example.com --no-browser
```

然后复制终端显示的授权地址，在有浏览器的电脑上打开并完成授权。

检查当前登录状态：

```bash
git-ai whoami
```

如需切换账号：

```bash
git-ai logout
git-ai login --server https://git-ai.example.com
```

## 6. 安装结果检查

依次执行：

```bash
git-ai --version
git-ai whoami
git-ai config api_base_url
git-ai git-path
```

其中服务器地址应为：

```text
https://git-ai.example.com
```

`git-ai git-path` 应该显示电脑上真正的 Git 路径，例如：

```text
/usr/bin/git
```

Windows 上通常会显示 Git for Windows 的 `git.exe` 路径。

## 7. 更新 Git AI

Git AI 会定期检查企业服务器上是否有新版本。发现新版本后，可以执行：

Windows 必须直接运行 `git-ai update`，不能使用 `git ai update`，否则父级 `git.exe` 可能持续占用需要替换的文件。

```bash
git-ai update
```

查看更新后的版本：

```bash
git-ai --version
```

如果需要重新安装当前最新版：

```bash
git-ai update --force
```

也可以直接重新运行对应系统的安装命令，安装程序会覆盖旧版本并保留现有登录和配置。


## 8. 使用方法

### 8.1 编辑器与 AI 工具集成


本工具支持：VS Code、Cursor、Trae、CodeBuddy、Qoder、JClaude Code、Codex、GitHub Copilot、Windsurf、Gemini CLI、OpenCode、Amp、Pi、Droid、Firebender。

**使用之前编辑器必须安装拓展：git-ai-0.1.22.vsix**

vsix拓展下载地址：https://git-ai.example.com/static/downloads/git-ai-extension.vsix

安装成功后设置拓展：

```json
{
  "gitai.enableCheckpointLogging": true,
  "gitai.experiments.aiTabTracking": true
}
```

**注意：Trae 需要设置找到 hooks 设置，然后手动将“已配置的 Hooks”这个选项开启。**

### 8.2 使用命令

安装和授权完成后，无需为每个 Git 仓库单独初始化。正常使用支持的 AI 工具编辑代码并执行 Git 命令即可。执行 git commit 会将 AI 与人工代码归属统计数据上传至统计后台。

常用命令：

* 查看 AI 与人工代码归属统计

```bash
git-ai status
```

* 更多命令

```bash
git-ai help
```



## 9. 常见问题

### 9.1 提示 `git-ai: command not found`

先关闭并重新打开终端。

macOS/Linux 也可以临时执行：

```bash
export PATH="$HOME/.git-ai/bin:$PATH"
```

然后再次执行：

```bash
git-ai --version
```

### 9.2 提示无法找到标准 Git

先检查：

```bash
git --version
command -v git
```

macOS 系统 Git 通常位于 `/usr/bin/git`。如果 Git 已安装但仍然报错，请把以上两条命令的输出发给管理员。

### 9.3 登录页面没有自动打开

执行：

```bash
git-ai login --server https://git-ai.example.com
```

或者使用：

```bash
git-ai login --server https://git-ai.example.com --no-browser
```

### 9.4 无法连接企业服务器

macOS/Linux：

```bash
curl -v https://git-ai.example.com/health
```

Windows PowerShell：

```powershell
Test-NetConnection 203.0.113.10 -Port 38080
```

如果连接失败，请确认当前网络允许访问 `git-ai.example.com`，必要时联系管理员。

### 9.5 macOS 提示开发者或安全限制

请先确认安装命令来自本文中的企业服务器地址。安装器会尝试移除下载文件的隔离属性；如果仍被系统阻止，请把完整提示截图发给管理员处理。

### 9.6 Windows 安装后仍找不到 `git-ai`

关闭所有 PowerShell 或命令提示符窗口，重新打开后再执行：

```powershell
git-ai --version
```

如果仍然找不到，请检查用户 PATH 中是否包含：

```text
%USERPROFILE%\.git-ai\bin
```

### 9.7 如果安装 CLI 后才安装新的编辑器或 AI 工具，或者 hooks 自动配置失败，请重新执行：

```bash
git-ai install-hooks
```

## 10. 获取诊断信息

遇到问题时，可以执行：

```bash
git-ai debug
```

将输出内容、操作系统版本、`git-ai --version` 结果以及报错截图一起发给管理员，可以更快定位问题。
