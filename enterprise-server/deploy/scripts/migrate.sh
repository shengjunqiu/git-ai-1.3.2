#!/bin/bash
# ============================================================
# Git AI Enterprise Server - Database Migration Script
# ============================================================
# Usage: ./migrate.sh [--init|--upgrade]
# Both modes run the API image's embedded, idempotent migrations.
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }

# Check if .env exists
if [ ! -f "$DEPLOY_DIR/.env" ]; then
    error ".env file not found. Copy .env.example to .env and configure it first."
fi

MODE="${1:---upgrade}"
COMPOSE=(docker compose --env-file "$DEPLOY_DIR/.env" -f "$DEPLOY_DIR/docker-compose.yml")

case "$MODE" in
    --init|--upgrade)
        if [ -z "$("${COMPOSE[@]}" ps -q postgres)" ]; then
            error "PostgreSQL container is not running. Start it first with docker compose up -d postgres"
        fi
        info "Running embedded database migrations..."
        "${COMPOSE[@]}" run --rm --no-deps api \
            /usr/local/bin/git-ai-enterprise-server --migrate
        info "Migrations completed successfully."
        ;;
    *)
        error "Unknown option: $MODE. Use --init or --upgrade"
        ;;
esac
