#!/bin/bash
# ============================================================
# Git AI Enterprise Server - Deployment Script
# ============================================================
# This script deploys the enterprise server on a target machine.
#
# Prerequisites:
#   - Docker and Docker Compose installed
#   - At least 2GB RAM, 10GB disk
#   - Configured API, PostgreSQL, and MinIO host ports available
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }

# Step 1: Load Docker images
info "Loading Docker images..."
if [ -d "$DEPLOY_DIR/images" ]; then
    if [ -f "$DEPLOY_DIR/images/SHA256SUMS" ]; then
        info "Verifying Docker image checksums..."
        (cd "$DEPLOY_DIR/images" && sha256sum -c SHA256SUMS)
    fi
    for img in "$DEPLOY_DIR"/images/*.tar; do
        if [ -f "$img" ]; then
            info "Loading: $(basename "$img")"
            docker load -i "$img"
        fi
    done
    info "Images loaded successfully"
else
    warn "No images directory found. Make sure images are already loaded."
fi

# Step 2: Check .env
if [ ! -f "$DEPLOY_DIR/.env" ]; then
    warn ".env not found. Creating from .env.example..."
    cp "$DEPLOY_DIR/.env.example" "$DEPLOY_DIR/.env"
    error "Please edit .env with your configuration, then re-run this script."
fi

COMPOSE=(docker compose --env-file "$DEPLOY_DIR/.env" -f "$DEPLOY_DIR/docker-compose.yml")

# Step 3: Start services
info "Starting services..."
"${COMPOSE[@]}" up -d --pull never

# Step 4: Wait for health checks
info "Waiting for the API to become healthy..."
API_ADDRESS=$("${COMPOSE[@]}" port api 8080)
API_PORT="${API_ADDRESS##*:}"
API_HEALTH=""
for _ in {1..30}; do
    API_HEALTH=$(curl -sf "http://127.0.0.1:${API_PORT}/health" 2>/dev/null || true)
    if echo "$API_HEALTH" | grep -q '"status":"ok"'; then
        break
    fi
    sleep 2
done

if echo "$API_HEALTH" | grep -q '"status":"ok"'; then
    info "API is healthy: $API_HEALTH"
else
    warn "API health check did not become ready"
    warn "Check logs: docker compose --env-file $DEPLOY_DIR/.env -f $DEPLOY_DIR/docker-compose.yml logs api"
fi

# Database migrations are embedded in the API image and run during startup.

info "======================================"
info "Deployment complete!"
info "API:      http://localhost:${API_PORT}"
info "MinIO and PostgreSQL ports: run docker compose port for details"
info "======================================"
