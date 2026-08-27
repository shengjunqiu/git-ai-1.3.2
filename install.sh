#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

# ============================================================
# Ensure HOME is set when running via MDMs (e.g. JAMF) or other environments where HOME may be unbound.
# ============================================================
INSTALL_USER=""

if [ -z "${HOME:-}" ]; then
    if command -v scutil >/dev/null 2>&1; then
        CURRENT_USER=$( /usr/sbin/scutil <<< "show State:/Users/ConsoleUser" | awk '/Name :/ { print $3 }' || true )
        if [ -n "${CURRENT_USER:-}" ] && [ "$CURRENT_USER" != "loginwindow" ] && [ "$CURRENT_USER" != "_mbsetupuser" ]; then
            export HOME=$( /usr/bin/dscl . -read "/Users/$CURRENT_USER" NFSHomeDirectory | awk '{print $2}' )
            INSTALL_USER="$CURRENT_USER"
        else
            echo "Error: No console user logged in. Deferring installation." >&2
            exit 1
        fi
    elif id -un >/dev/null 2>&1; then
        INSTALL_USER="$(id -un)"
        export HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
        if [ -z "$HOME" ]; then
            export HOME="/root"
        fi
    else
        export HOME="/root"
    fi
fi

# Ensure SHELL is set (also may be unbound in JAMF)
if [ -z "${SHELL:-}" ]; then
    if command -v zsh >/dev/null 2>&1; then
        SHELL="$(command -v zsh)"
    elif command -v bash >/dev/null 2>&1; then
        SHELL="$(command -v bash)"
    else
        SHELL="/bin/sh"
    fi
    export SHELL
fi

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# GitHub repository details
# Replaced during release builds with the actual repository (e.g., "git-ai-project/git-ai")
# When set to __REPO_PLACEHOLDER__, defaults to "git-ai-project/git-ai"
REPO="__REPO_PLACEHOLDER__"
if [ "$REPO" = "__REPO_PLACEHOLDER__" ]; then
    REPO="git-ai-project/git-ai"
fi

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
PINNED_VERSION="__VERSION_PLACEHOLDER__"

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"

# Enterprise release source placeholders. Enterprise release generation replaces
# both values; public GitHub release generation leaves them untouched.
ENTERPRISE_RELEASE_BASE_URL="__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__"
ENTERPRISE_RELEASE_CHANNEL="__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__"

# Enterprise API endpoint. Every install and upgrade enforces this value,
# replacing any api_base_url previously saved by the user.
ENTERPRISE_API_BASE_URL="http://117.147.213.234:38080"

# Function to print error messages
error() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

warn() {
    echo -e "${YELLOW}Warning: $1${NC}" >&2
}

# Function to print success messages
success() {
    echo -e "${GREEN}$1${NC}"
}

# Function to verify checksum of downloaded binary
verify_checksum() {
    local file="$1"
    local binary_name="$2"

    # Skip verification if no checksums are embedded
    if [ "$EMBEDDED_CHECKSUMS" = "__CHECKSUMS_PLACEHOLDER__" ]; then
        return 0
    fi

    # Extract expected checksum for this binary
    local expected=""
    local old_ifs="$IFS"
    IFS='|' read -ra CHECKSUM_ENTRIES <<< "$EMBEDDED_CHECKSUMS"
    IFS="$old_ifs"
    for entry in "${CHECKSUM_ENTRIES[@]}"; do
        if [[ "$entry" =~ ^[[:xdigit:]]+[[:space:]]+$binary_name$ ]]; then
            expected=$(echo "$entry" | awk '{print $1}')
            break
        fi
    done

    if [ -z "$expected" ]; then
        error "No checksum found for $binary_name"
    fi

    # Calculate actual checksum
    local actual=""
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$file" | awk '{print $1}')
    else
        rm -f "$file" 2>/dev/null || true
        error "Cannot verify $binary_name: neither sha256sum nor shasum is installed. Install a SHA-256 tool and run the installer again."
    fi

    if [ "$expected" != "$actual" ]; then
        rm -f "$file" 2>/dev/null || true
        error "Checksum verification failed for $binary_name\nExpected: $expected\nActual:   $actual"
    fi

    success "Checksum verified for $binary_name"
}

# Function to detect all shells with existing config files
# Returns shell configurations in format: "shell_name|config_file" (one per line)
detect_all_shells() {
    local shells=""
    
    # Check for bash configs (prefer .bashrc over .bash_profile)
    if [ -f "$HOME/.bashrc" ]; then
        shells="${shells}bash|$HOME/.bashrc\n"
    elif [ -f "$HOME/.bash_profile" ]; then
        shells="${shells}bash|$HOME/.bash_profile\n"
    fi
    
    # Check for zsh config
    if [ -f "$HOME/.zshrc" ]; then
        shells="${shells}zsh|$HOME/.zshrc\n"
    fi
    
    # Check for fish config
    if [ -f "$HOME/.config/fish/config.fish" ]; then
        shells="${shells}fish|$HOME/.config/fish/config.fish\n"
    fi
    
    # If no configs found, fall back to $SHELL detection and create config for that shell only
    if [ -z "$shells" ]; then
        local login_shell=""
        if [ -n "${SHELL:-}" ]; then
            login_shell=$(basename "$SHELL")
        fi
        case "$login_shell" in
            fish)
                shells="fish|$HOME/.config/fish/config.fish"
                ;;
            zsh)
                shells="zsh|$HOME/.zshrc"
                ;;
            bash|*)
                shells="bash|$HOME/.bashrc"
                ;;
        esac
    fi
    
    # Remove trailing newline and output
    printf '%b' "$shells" | sed '/^$/d'
}

resolve_std_git_candidate() {
    local candidate="${1:-}"
    local target=""
    local candidate_dir=""
    local link_count=0

    [ -n "$candidate" ] || return 1

    # Resolve symlinks portably. macOS readlink does not support -f, and the
    # existing git-og link must resolve to the real Git before we recreate it.
    while [ -L "$candidate" ] && [ "$link_count" -lt 20 ]; do
        target=$(readlink "$candidate" 2>/dev/null) || return 1
        case "$target" in
            /*) candidate="$target" ;;
            *)
                candidate_dir=$(cd -P "$(dirname "$candidate")" 2>/dev/null && pwd) || return 1
                candidate="$candidate_dir/$target"
                ;;
        esac
        link_count=$((link_count + 1))
    done

    [ "$link_count" -lt 20 ] || return 1
    [ -f "$candidate" ] && [ -x "$candidate" ] || return 1

    candidate_dir=$(cd -P "$(dirname "$candidate")" 2>/dev/null && pwd) || return 1
    candidate="$candidate_dir/$(basename "$candidate")"

    # Never select git-ai itself as the underlying Git implementation.
    case "$(basename "$candidate")" in
        git-ai|git-ai.exe) return 1 ;;
    esac
    case "$candidate" in
        "$HOME/.git-ai/bin/git") return 1 ;;
    esac

    "$candidate" --version >/dev/null 2>&1 || return 1
    printf '%s\n' "$candidate"
}

detect_std_git() {
    local git_path=""
    local candidate=""
    local path_entry=""
    local saved_ifs="$IFS"
    local cfg_json="$HOME/.git-ai/config.json"
    local cfg_git_path=""

    # A previous installation already recorded the real Git behind our shim.
    if git_path=$(resolve_std_git_candidate "$HOME/.git-ai/bin/git-og"); then
        printf '%s\n' "$git_path"
        return 0
    fi

    # Recover from the persisted config before searching PATH.
    if [ -f "$cfg_json" ]; then
        # Extract git_path without requiring jq during bootstrap.
        cfg_git_path=$(sed -n 's/.*"git_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$cfg_json" | head -n1 || true)
        if git_path=$(resolve_std_git_candidate "$cfg_git_path"); then
            printf '%s\n' "$git_path"
            return 0
        fi
    fi

    # Search every PATH entry instead of stopping when the first `git` is our
    # own shim. This is the normal reinstall/update case.
    IFS=":"
    for path_entry in $PATH; do
        [ -n "$path_entry" ] || path_entry="."
        candidate="$path_entry/git"
        if git_path=$(resolve_std_git_candidate "$candidate"); then
            IFS="$saved_ifs"
            printf '%s\n' "$git_path"
            return 0
        fi
    done
    IFS="$saved_ifs"

    # PATH may be minimal under MDM/root installs, so check common locations.
    for candidate in \
        /usr/bin/git \
        /usr/local/bin/git \
        /opt/homebrew/bin/git \
        /opt/local/bin/git; do
        if git_path=$(resolve_std_git_candidate "$candidate"); then
            printf '%s\n' "$git_path"
            return 0
        fi
    done

    error "Could not detect a standard git binary on PATH or in standard locations. Please ensure you have Git installed. If you believe this is a bug with the installer, please file an issue at https://github.com/git-ai-project/git-ai/issues."
}

# Detect standard git path (needed early for install)
STD_GIT_PATH=$(detect_std_git)

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)


# Map architecture to binary name
case $ARCH in
    "x86_64")
        ARCH="x64"
        ;;
    "aarch64"|"arm64")
        ARCH="arm64"
        ;;
    *)
        error "Unsupported architecture: $ARCH"
        ;;
esac

# Map OS to binary name
case $OS in
    "darwin")
        OS="macos"
        ;;
    "linux")
        OS="linux"
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

# Determine binary name
BINARY_NAME="git-ai-${OS}-${ARCH}"

# Determine release tag
# Priority: 1. Local binary override, 2. Bundled binary, 3. Enterprise release
# source, 4. Pinned GitHub version, 5. Environment override, 6. GitHub latest.

# Check for bundled binary in the same directory as the install script
_BUNDLED_BINARY=""
_INSTALL_SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
_INSTALL_SCRIPT_DIR=""
if [ -n "$_INSTALL_SCRIPT_SOURCE" ] && [ -f "$_INSTALL_SCRIPT_SOURCE" ]; then
    _INSTALL_SCRIPT_DIR="$(cd "$(dirname "$_INSTALL_SCRIPT_SOURCE")" && pwd)"
fi
if [ -n "$_INSTALL_SCRIPT_DIR" ]; then
    _BUNDLED_CANDIDATE="${_INSTALL_SCRIPT_DIR}/${BINARY_NAME}"
    if [ -f "$_BUNDLED_CANDIDATE" ]; then
        _BUNDLED_BINARY="$_BUNDLED_CANDIDATE"
    fi
fi

if [ -n "${GIT_AI_LOCAL_BINARY:-}" ]; then
    RELEASE_TAG="local"
    DOWNLOAD_URL=""
elif [ -n "$_BUNDLED_BINARY" ]; then
    RELEASE_TAG="local"
    GIT_AI_LOCAL_BINARY="$_BUNDLED_BINARY"
    DOWNLOAD_URL=""
elif [ "$ENTERPRISE_RELEASE_BASE_URL" != "__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__" ]; then
    RELEASE_TAG="${PINNED_VERSION:-$ENTERPRISE_RELEASE_CHANNEL}"
    DOWNLOAD_URL="${ENTERPRISE_RELEASE_BASE_URL%/}/worker/releases/${ENTERPRISE_RELEASE_CHANNEL}/download/${BINARY_NAME}"
elif [ "$PINNED_VERSION" != "__VERSION_PLACEHOLDER__" ]; then
    # Version-pinned install script from a release
    RELEASE_TAG="$PINNED_VERSION"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
elif [ -n "${GIT_AI_RELEASE_TAG:-}" ] && [ "${GIT_AI_RELEASE_TAG:-}" != "latest" ]; then
    # Environment variable override
    RELEASE_TAG="$GIT_AI_RELEASE_TAG"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
else
    # Default to latest
    RELEASE_TAG="latest"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}"
fi

# Install into the user's bin directory ~/.git-ai/bin
INSTALL_DIR="$HOME/.git-ai/bin"

# Create directory if it doesn't exist
mkdir -p "$INSTALL_DIR"

# Download and install
TMP_FILE="${INSTALL_DIR}/git-ai.tmp.$$"
if [ -n "${GIT_AI_LOCAL_BINARY:-}" ]; then
    echo "Using local git-ai binary (release: ${RELEASE_TAG})..."
    if [ ! -f "$GIT_AI_LOCAL_BINARY" ]; then
        error "Local binary not found at $GIT_AI_LOCAL_BINARY"
    fi
    cp "$GIT_AI_LOCAL_BINARY" "$TMP_FILE"
else
    echo "Downloading git-ai (release: ${RELEASE_TAG})..."
    if ! curl --fail --location --silent --show-error -o "$TMP_FILE" "$DOWNLOAD_URL"; then
        rm -f "$TMP_FILE" 2>/dev/null || true
        error "Failed to download binary (HTTP error)"
    fi
fi

# Basic validation: ensure file is not empty
if [ ! -s "$TMP_FILE" ]; then
    rm -f "$TMP_FILE" 2>/dev/null || true
    error "Downloaded file is empty"
fi

# Verify checksum if embedded (release builds only)
verify_checksum "$TMP_FILE" "$BINARY_NAME"

mv -f "$TMP_FILE" "${INSTALL_DIR}/git-ai"

# Make executable
chmod +x "${INSTALL_DIR}/git-ai"
# Symlink git to git-ai
ln -sf "${INSTALL_DIR}/git-ai" "${INSTALL_DIR}/git"

# Symlink git-og to the detected standard git path
ln -sf "$STD_GIT_PATH" "${INSTALL_DIR}/git-og"

# Remove quarantine attribute on macOS
if [ "$OS" = "macos" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/git-ai" 2>/dev/null || true
fi

# Create ~/.local/bin/git-ai symlink for systems where ~/.local/bin is already on PATH
LOCAL_BIN_DIR="$HOME/.local/bin"
if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null && ln -sf "${INSTALL_DIR}/git-ai" "${LOCAL_BIN_DIR}/git-ai" 2>/dev/null; then
    success "Created symlink at ${LOCAL_BIN_DIR}/git-ai"
else
    warn "Failed to create ~/.local/bin/git-ai symlink. This is non-fatal."
fi

success "Successfully installed git-ai into ${INSTALL_DIR}"
success "You can now run 'git-ai' from your terminal"

# Print installed version
INSTALLED_VERSION=$(${INSTALL_DIR}/git-ai --version 2>&1 || echo "unknown")
echo "Installed git-ai ${INSTALLED_VERSION}"

# Login user with install token if provided
NEED_LOGIN=false
if [ -n "${INSTALL_NONCE:-}" ] && [ -n "${API_BASE:-}" ]; then
    if ! ${INSTALL_DIR}/git-ai exchange-nonce; then
        NEED_LOGIN=true
    fi
fi

echo "Setting up IDE/agent hooks..."
if ! ${INSTALL_DIR}/git-ai install-hooks; then
    warn "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
else
    success "Successfully set up IDE/agent hooks"
fi

# Write JSON config at ~/.git-ai/config.json (only if it doesn't exist)
CONFIG_DIR="$HOME/.git-ai"
CONFIG_JSON_PATH="$CONFIG_DIR/config.json"
mkdir -p "$CONFIG_DIR"

if [ ! -f "$CONFIG_JSON_PATH" ]; then
    TMP_CFG="$CONFIG_JSON_PATH.tmp.$$"
    cat >"$TMP_CFG" <<EOF
{
  "git_path": "${STD_GIT_PATH}",
  "feature_flags": {
    "async_mode": true
  }
}
EOF
    mv -f "$TMP_CFG" "$CONFIG_JSON_PATH"
fi

if ! "${INSTALL_DIR}/git-ai" config set api_base_url "$ENTERPRISE_API_BASE_URL"; then
    error "Failed to configure enterprise API server: $ENTERPRISE_API_BASE_URL"
fi
success "Configured enterprise API server: $ENTERPRISE_API_BASE_URL"

if ! "${INSTALL_DIR}/git-ai" config set disable_version_checks false; then
    error "Failed to enable automatic update checks"
fi

if ! "${INSTALL_DIR}/git-ai" config set disable_auto_updates true; then
    error "Failed to configure manual update installation"
fi
success "Configured automatic update checks with manual installation"

# Add to PATH in all detected shell configurations
SHELLS_CONFIGURED=""
SHELLS_ALREADY_CONFIGURED=""
CREATED_SHELL_PATHS=""

while IFS='|' read -r shell_name config_file; do
    [ -z "$shell_name" ] && continue
    
    # Generate shell-appropriate PATH command
    if [ "$shell_name" = "fish" ]; then
        path_cmd="fish_add_path -g \"$INSTALL_DIR\""
        # Create fish config directory if it doesn't exist (for fallback case)
        config_dir="$(dirname "$config_file")"
        if [ ! -d "$config_dir" ]; then
            mkdir -p "$config_dir"
            CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_dir}\n"
        fi
    else
        path_cmd="export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
    
    # Create config file if it doesn't exist (for fallback case when no configs found)
    if [ ! -f "$config_file" ]; then
        CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_file}\n"
    fi
    touch "$config_file"
    
    # Append if not already present
    if ! grep -qsF "$INSTALL_DIR" "$config_file"; then
        echo "" >> "$config_file"
        echo "# Added by git-ai installer on $(date)" >> "$config_file"
        echo "$path_cmd" >> "$config_file"
        SHELLS_CONFIGURED="${SHELLS_CONFIGURED}${shell_name}|${config_file}\n"
    else
        SHELLS_ALREADY_CONFIGURED="${SHELLS_ALREADY_CONFIGURED}${shell_name}|${config_file}\n"
    fi
done <<< "$(detect_all_shells)"

# Display results to user
if [ -n "$SHELLS_CONFIGURED" ]; then
    echo ""
    echo "Updated shell configurations:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        success "  ✓ $config_file"
    done
    
    echo ""
    echo "To apply changes immediately:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        if [ "$shell_name" = "fish" ]; then
            echo "  - For fish: source $config_file"
        else
            echo "  - For $shell_name: source $config_file"
        fi
    done
fi

if [ -n "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Already configured (no changes needed):"
    printf '%b' "$SHELLS_ALREADY_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        echo "  ✓ $config_file"
    done
fi

if [ -z "$SHELLS_CONFIGURED" ] && [ -z "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Could not detect any shell config files."
    echo "Please add the following line to your shell config and restart:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# Fix file ownership when running as root for a different user (MDM deployments)
if [ "$(id -u)" = "0" ] && [ -n "$INSTALL_USER" ]; then
    chown -R "$INSTALL_USER" "$HOME/.git-ai" 2>/dev/null || true
    if [ -n "$CREATED_SHELL_PATHS" ]; then
        printf '%b' "$CREATED_SHELL_PATHS" | while IFS= read -r created_path; do
            [ -z "$created_path" ] && continue
            chown "$INSTALL_USER" "$created_path" 2>/dev/null || true
        done
    fi
fi

echo ""
echo -e "${YELLOW}Close and reopen your terminal and IDE sessions to use git-ai.${NC}"

# If nonce exchange failed, run interactive login
if [ "$NEED_LOGIN" = true ]; then
    echo ""
    echo "Launching login..."
    ${INSTALL_DIR}/git-ai login
fi
