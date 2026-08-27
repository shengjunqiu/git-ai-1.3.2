#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVER="${GIT_AI_RELEASE_SERVER:-http://117.147.213.234:38080}"
SERVER="${SERVER%/}"
VERSION="${GIT_AI_RELEASE_VERSION:-$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT_DIR/Cargo.toml")}"
CHANNEL="${GIT_AI_RELEASE_CHANNEL:-latest}"
VERSION_CHANNEL="${GIT_AI_RELEASE_VERSION_CHANNEL:-$VERSION}"
ASSET_DIR="${GIT_AI_RELEASE_ASSET_DIR:-$HOME/Downloads/git-ai release}"
OUTPUT_DIR="${GIT_AI_RELEASE_OUTPUT_DIR:-$ASSET_DIR/enterprise-$VERSION}"
ADMIN_KEY_FILE="${GIT_AI_ADMIN_KEY_FILE:-/tmp/git-ai-admin-key}"
CREDENTIAL_FILE="${GIT_AI_CREDENTIAL_FILE:-$HOME/.git-ai/internal/credentials}"
PREPARE_ONLY="${GIT_AI_RELEASE_PREPARE_ONLY:-0}"

BINARY_FILES=(
  git-ai-linux-x64
  git-ai-linux-arm64
  git-ai-windows-x64.exe
  git-ai-windows-arm64.exe
  git-ai-macos-x64
  git-ai-macos-arm64
)

GENERATED_FILES=(
  install.sh
  install.ps1
)

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Required command not found: $1" >&2
    exit 2
  fi
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

asset_path() {
  case "$1" in
    install.sh|install.ps1|SHA256SUMS)
      printf '%s/%s' "$OUTPUT_DIR" "$1"
      ;;
    *)
      printf '%s/%s' "$ASSET_DIR" "$1"
      ;;
  esac
}

require_command awk
require_command curl
require_command jq

if [[ -z "$VERSION" || ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid release version: $VERSION" >&2
  exit 2
fi

for filename in "${BINARY_FILES[@]}"; do
  if [[ ! -f "$ASSET_DIR/$filename" ]]; then
    echo "Missing release asset: $ASSET_DIR/$filename" >&2
    exit 2
  fi
done

mkdir -p "$OUTPUT_DIR"
binary_manifest="$OUTPUT_DIR/SHA256SUMS.binaries"
manifest="$OUTPUT_DIR/SHA256SUMS"
: > "$binary_manifest"

for filename in "${BINARY_FILES[@]}"; do
  hash="$(sha256_file "$ASSET_DIR/$filename")"
  printf '%s  %s\n' "$hash" "$filename" >> "$binary_manifest"
done

embedded_checksums="$(awk 'BEGIN { first = 1 } { if (!first) printf "|"; printf "%s", $0; first = 0 } END { print "" }' "$binary_manifest")"

awk \
  -v version="$VERSION" \
  -v checksums="$embedded_checksums" \
  -v release_base="$SERVER" \
  -v release_channel="$VERSION_CHANNEL" '
    /^PINNED_VERSION="__VERSION_PLACEHOLDER__"/ { sub(/__VERSION_PLACEHOLDER__/, version) }
    /^EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"/ { sub(/__CHECKSUMS_PLACEHOLDER__/, checksums) }
    /^ENTERPRISE_RELEASE_BASE_URL="__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__"/ { sub(/__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__/, release_base) }
    /^ENTERPRISE_RELEASE_CHANNEL="__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__"/ { sub(/__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__/, release_channel) }
    { print }
  ' "$ROOT_DIR/install.sh" > "$OUTPUT_DIR/install.sh"
chmod +x "$OUTPUT_DIR/install.sh"

awk \
  -v version="$VERSION" \
  -v checksums="$embedded_checksums" \
  -v release_base="$SERVER" \
  -v release_channel="$VERSION_CHANNEL" '
    /^[$]PinnedVersion = .__VERSION_PLACEHOLDER__/ { sub(/__VERSION_PLACEHOLDER__/, version) }
    /^[$]EmbeddedChecksums = .__CHECKSUMS_PLACEHOLDER__/ { sub(/__CHECKSUMS_PLACEHOLDER__/, checksums) }
    /^[$]EnterpriseReleaseBaseUrl = .__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__/ { sub(/__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__/, release_base) }
    /^[$]EnterpriseReleaseChannel = .__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__/ { sub(/__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__/, release_channel) }
    { print }
  ' "$ROOT_DIR/install.ps1" > "$OUTPUT_DIR/install.ps1"

: > "$manifest"
for filename in "${BINARY_FILES[@]}" "${GENERATED_FILES[@]}"; do
  path="$(asset_path "$filename")"
  hash="$(sha256_file "$path")"
  printf '%s  %s\n' "$hash" "$filename" >> "$manifest"
done

manifest_hash="$(sha256_file "$manifest")"

echo "Prepared enterprise release $VERSION"
echo "  Binary directory: $ASSET_DIR"
echo "  Generated files:  $OUTPUT_DIR"
echo "  Version channel:  $VERSION_CHANNEL"
echo "  Publish channel:  $CHANNEL"
echo "  Manifest SHA256:  $manifest_hash"

if [[ "$PREPARE_ONLY" == "1" ]]; then
  echo "Prepare-only mode enabled; nothing was uploaded."
  exit 0
fi

if [[ -f "$CREDENTIAL_FILE" ]]; then
  access_token="$(jq -r '.access_token // empty' "$CREDENTIAL_FILE")"
  auth_header="Authorization: Bearer $access_token"
  credential_auth=true
elif [[ -f "$ADMIN_KEY_FILE" ]]; then
  admin_key="$(<"$ADMIN_KEY_FILE")"
  auth_header="X-API-Key: $admin_key"
else
  echo "No admin API key or CLI credential file found" >&2
  exit 2
fi

if [[ "$auth_header" == *": " ]]; then
  echo "Authentication credential is empty" >&2
  exit 2
fi

echo "Checking admin access at $SERVER"
auth_status="$(curl --silent --output /dev/null --write-out '%{http_code}' \
  -H "$auth_header" "$SERVER/api/admin/releases/assets")"

if [[ "$auth_status" == "401" && "${credential_auth:-false}" == "true" ]]; then
  echo "Refreshing expired or invalid CLI access token"
  refresh_token="$(jq -r '.refresh_token // empty' "$CREDENTIAL_FILE")"
  refresh_response="$(jq -n \
    --arg refresh_token "$refresh_token" \
    '{grant_type:"refresh_token",refresh_token:$refresh_token,client_id:"git-ai-cli"}' | \
    curl --fail-with-body --silent --show-error \
      -H "Content-Type: application/json" \
      --data-binary @- \
      "$SERVER/worker/oauth/token")"

  access_token="$(jq -er '.access_token' <<<"$refresh_response")"
  new_refresh_token="$(jq -er '.refresh_token' <<<"$refresh_response")"
  access_expires_in="$(jq -er '.expires_in' <<<"$refresh_response")"
  refresh_expires_in="$(jq -er '.refresh_expires_in' <<<"$refresh_response")"
  now="$(date +%s)"
  credential_tmp="$(mktemp "${CREDENTIAL_FILE}.tmp.XXXXXX")"
  chmod 600 "$credential_tmp"
  jq -n \
    --arg access_token "$access_token" \
    --arg refresh_token "$new_refresh_token" \
    --argjson access_token_expires_at "$((now + access_expires_in))" \
    --argjson refresh_token_expires_at "$((now + refresh_expires_in))" \
    '{access_token:$access_token,refresh_token:$refresh_token,access_token_expires_at:$access_token_expires_at,refresh_token_expires_at:$refresh_token_expires_at}' \
    > "$credential_tmp"
  mv "$credential_tmp" "$CREDENTIAL_FILE"
  auth_header="Authorization: Bearer $access_token"
  auth_status="$(curl --silent --output /dev/null --write-out '%{http_code}' \
    -H "$auth_header" "$SERVER/api/admin/releases/assets")"
fi

if [[ "$auth_status" != "200" ]]; then
  echo "Admin access check failed with HTTP $auth_status" >&2
  exit 1
fi

upload_asset() {
  local filename="$1"
  local path="$2"
  local hash
  hash="$(sha256_file "$path")"

  echo "Uploading $filename"
  curl --fail-with-body --silent --show-error \
    -H "$auth_header" \
    -F "channel=$CHANNEL" \
    -F "version=$VERSION" \
    -F "filename=$filename" \
    -F "sha256=$hash" \
    -F "file=@$path" \
    "$SERVER/api/admin/releases/upload"
  echo
}

for filename in "${BINARY_FILES[@]}" "${GENERATED_FILES[@]}"; do
  upload_asset "$filename" "$(asset_path "$filename")"
done
upload_asset "SHA256SUMS" "$manifest"

publish_channel() {
  local channel="$1"
  echo "Publishing $channel -> $VERSION"
  jq -n \
    --arg channel "$channel" \
    --arg version "$VERSION" \
    --arg checksum "$manifest_hash" \
    '{channel:$channel,version:$version,checksum:$checksum}' | \
    curl --fail-with-body --silent --show-error \
      -X POST \
      -H "$auth_header" \
      -H "Content-Type: application/json" \
      --data-binary @- \
      "$SERVER/api/admin/releases/channel"
  echo
}

publish_channel "$VERSION_CHANNEL"
if [[ "$CHANNEL" != "$VERSION_CHANNEL" ]]; then
  publish_channel "$CHANNEL"
fi

echo "Verifying public release metadata"
release_metadata="$(curl --fail-with-body --silent --show-error "$SERVER/worker/releases")"
echo "$release_metadata"

published_version="$(jq -r --arg channel "$CHANNEL" '.channels[$channel].version // empty' <<<"$release_metadata")"
if [[ "$published_version" != "$VERSION" ]]; then
  echo "Published channel $CHANNEL points to $published_version, expected $VERSION" >&2
  exit 1
fi

downloaded_manifest="$(mktemp)"
trap 'rm -f "$downloaded_manifest"' EXIT
curl --fail-with-body --silent --show-error \
  -o "$downloaded_manifest" \
  "$SERVER/worker/releases/$CHANNEL/download/SHA256SUMS"
downloaded_manifest_hash="$(sha256_file "$downloaded_manifest")"
if [[ "$downloaded_manifest_hash" != "$manifest_hash" ]]; then
  echo "Downloaded SHA256SUMS hash mismatch" >&2
  exit 1
fi

echo "Enterprise release $VERSION published successfully."
