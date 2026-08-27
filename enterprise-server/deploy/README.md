# Git AI Enterprise Server - 部署指南

## 系统要求

- **操作系统**: Linux (推荐 Ubuntu 22.04+ / CentOS 8+)
- **Docker**: 24.0+ 
- **Docker Compose**: v2.0+
- **内存**: 最低 2GB，推荐 4GB+
- **磁盘**: 最低 10GB
- **端口**: 8080 (API), 5433 (PostgreSQL), 9000/9001 (MinIO)；Redis 仅在 Docker 网络内访问

## 快速部署

### 1. 解压部署包

```bash
mkdir -p git-ai-enterprise-server-deploy
tar xzf git-ai-enterprise-<version>-linux-amd64.tar.gz \
  -C git-ai-enterprise-server-deploy
cd git-ai-enterprise-server-deploy
```

### 2. 配置环境变量

```bash
cp .env.example .env
# 编辑 .env，至少修改以下项：
#   JWT_SECRET      - 随机密钥，至少 32 字符
#   POSTGRES_PASSWORD - 数据库密码
vi .env
```

### 3. 加载镜像

```bash
(cd images && sha256sum -c SHA256SUMS)
docker load -i images/git-ai-enterprise-stack.tar
```

使用 `scripts/deploy.sh` 一键部署时会自动执行该校验。

### 4. 一键部署

```bash
bash scripts/deploy.sh
```

或者手动执行：

```bash
# 启动离线部署包中的服务，不尝试从公网拉取镜像
docker compose up -d --pull never

# 等待服务就绪 (约 15 秒)
sleep 15
```

API 启动时会自动执行镜像内嵌的数据库迁移。

### 5. 验证服务

```bash
# 健康检查
curl http://localhost:8080/health

# 预期返回: {"service":"git-ai-enterprise-server","status":"ok","version":"0.1.0"}
```

## 服务端口

| 服务 | 端口 | 说明 |
|------|------|------|
| API | 8080 | REST API 服务 |
| PostgreSQL | 5433 | 数据库 (外部访问) |
| Redis | 不暴露 | 仅供 Compose 网络内的 API 使用 |
| MinIO API | 9000 | S3 兼容存储 |
| MinIO Console | 9001 | 存储管理界面 |

## 数据持久化

数据通过 Docker Volume 持久化：

- `pgdata` - PostgreSQL 数据
- `minio_data` - MinIO 对象存储数据

> **重要**: 删除容器不会丢失数据，但 `docker compose down -v` 会清除所有数据！

## 常用运维命令

```bash
# 查看服务状态
docker compose ps

# 查看日志
docker compose logs -f api          # API 日志
docker compose logs -f postgres     # 数据库日志

# 最近 200 行 API 日志（隐藏 Compose 服务名前缀）
docker compose logs -f --tail=200 --no-log-prefix api

# 重启服务
docker compose restart api

# 停止所有服务
docker compose down

# 停止并清除数据 (危险!)
docker compose down -v
```

### 日志格式与保留

API 日志默认使用适合终端阅读的紧凑单行格式。每个 HTTP 请求的完成日志包含
`request_id`、`method`、`path`、`status` 和 `elapsed_ms`；响应中的
`X-Request-Id` 与日志一致，便于定位一次请求的全部记录。查询字符串不会写入日志。

通过 `.env` 调整格式和级别：

```dotenv
# compact（默认）适合 docker compose logs；json 适合 Loki/ELK 等采集系统
LOG_FORMAT=compact
RUST_LOG=git_ai_enterprise_server=info,tower_http=warn

# Docker json-file 日志轮转
DOCKER_LOG_MAX_SIZE=20m
DOCKER_LOG_MAX_FILES=5
```

修改后重新创建 API 容器：

```bash
docker compose up -d --force-recreate api
```

使用 JSON 格式时，可通过 `jq` 在终端美化：

```bash
docker compose logs -f --no-log-prefix api | jq -RrC 'fromjson? // .'
```

## 升级部署

```bash
# 1. 加载新镜像
docker load -i images/git-ai-enterprise-stack.tar

# 2. 重启 API 容器
docker compose up -d --pull never api
```

## 数据库迁移脚本

`migrations/` 目录包含所有 SQL 迁移脚本，按编号顺序执行：

| 编号 | 说明 |
|------|------|
| 001 | 初始 Schema |
| 002 | 仓库访问列表 |
| 003 | 企业功能 |
| 004 | 数据隔离 |
| 005 | Linewell 组织及测试数据 |

PostgreSQL 容器首次启动时会读取 `migrations/` 目录；API 启动时也会按迁移记录自动执行镜像内嵌迁移。`migrate.sh` 仅用于诊断或显式手工迁移，不是正常部署步骤。

## API Key 管理

API Key 通过数据库管理，创建方式：

```bash
# 进入 PostgreSQL 容器
docker compose exec postgres psql -U gitai -d gitai_enterprise

# 查看现有 Key
SELECT key_prefix, name FROM api_keys WHERE revoked_at IS NULL;
```

建议通过 API 接口创建新的 API Key，以便获取明文密钥。

## Metrics 和 Dashboard Rollup

建议生产部署显式开启：

```env
METRICS_WRITE_ROLLUPS=true
DASHBOARD_USE_ROLLUPS=true
```

`METRICS_WRITE_ROLLUPS=true` 会在 metrics 上传时同步写入 `metrics_daily_rollups`，让 dashboard 查询有预聚合数据可用。`DASHBOARD_USE_ROLLUPS=true` 会让 summary、trends、tools 和 agent comparison 优先读取 rollup 表，避免在大数据量下反复扫描 `metrics_events` 明细。

开启前请先确认数据库迁移已执行到 `016_metrics_daily_rollups` 和 `017_metrics_tool_model_events`，并确认 `metrics_daily_rollups` 有数据：

```bash
docker compose exec postgres psql -U ${POSTGRES_USER:-gitai} -d ${POSTGRES_DB:-gitai_enterprise} \
  -c "SELECT COUNT(*) FROM metrics_daily_rollups;"
```

如果 dashboard 数据异常或需要快速回退，先将 `.env` 中的 `DASHBOARD_USE_ROLLUPS=false`，然后重启 API：

```bash
docker compose up -d --force-recreate api
```

如 metrics 写入压力异常，可临时设置 `METRICS_WRITE_ROLLUPS=false` 并重启 API；这只会关闭 daily rollup 写入，不会停止 metrics 明细写入。

## API 限流配置

API 限流通过 Redis 共享计数；Redis 不可用时会自动回退到单实例内存计数。生产部署建议保留默认值起步，再根据压测和 429 日志调整：

```env
RATE_LIMIT_METRICS_MAX_REQUESTS=60
RATE_LIMIT_METRICS_WINDOW_SECONDS=60
RATE_LIMIT_CAS_UPLOAD_MAX_REQUESTS=30
RATE_LIMIT_CAS_UPLOAD_WINDOW_SECONDS=60
RATE_LIMIT_CAS_READ_MAX_REQUESTS=100
RATE_LIMIT_CAS_READ_WINDOW_SECONDS=60
RATE_LIMIT_OAUTH_MAX_REQUESTS=600
RATE_LIMIT_OAUTH_WINDOW_SECONDS=60
RATE_LIMIT_AUTH_MAX_REQUESTS=300
RATE_LIMIT_AUTH_WINDOW_SECONDS=60
RATE_LIMIT_ADMIN_MAX_REQUESTS=30
RATE_LIMIT_ADMIN_WINDOW_SECONDS=60
RATE_LIMIT_DEFAULT_MAX_REQUESTS=300
RATE_LIMIT_DEFAULT_WINDOW_SECONDS=60
```

路由分层：

| Tier | 默认值 | 覆盖范围 |
|------|--------|----------|
| `metrics` | 60/min | `/worker/metrics/*` |
| `cas_upload` | 30/min | `/worker/cas/upload*` |
| `cas_read` | 100/min | `/worker/cas*` |
| `oauth` | 600/min | `/worker/oauth/*` |
| `auth` | 300/min | `/auth/*`, `/login`, `/logout`, `/verify` |
| `admin` | 30/min | `/api/admin/*` |
| `default` | 300/min | 其他 API |

多人同时注册、网页登录或 CLI 登录时，优先观察 `auth` 和 `oauth` tier 的 429 日志。如果 OAuth 设备码轮询较密集，可以提高 `RATE_LIMIT_OAUTH_MAX_REQUESTS`；如果注册登录入口被撞库或爆破，应降低 `RATE_LIMIT_AUTH_MAX_REQUESTS`，并配合更细粒度的账号/IP 风控。

## 注册登录密码计算

注册和登录的 Argon2 密码 hash/verify 会在 blocking worker 上执行，并通过 semaphore 限制并发：

```env
AUTH_PASSWORD_CONCURRENCY=8
```

默认值 `8` 适合先作为通用起点。登录高峰时如果 API CPU 仍有余量但登录 p95 偏高，可小幅提高；如果 CPU 已接近打满或其它接口被拖慢，应降低该值。该配置不会降低 Argon2 参数，只控制同一 API 实例内同时执行的密码计算数量。

建议灰度方式：

- 默认从 `AUTH_PASSWORD_CONCURRENCY=8` 开始。
- 4-8 核实例可灰度到 `12`，但必须同时观察登录 p95/p99、API CPU、内存和 `/health` 延迟。
- 不建议直接设置 `16+`；本地历史压测显示过高并发度可能恶化尾延迟。
- 回滚方式是调回 `8` 并重启 API。

登录压测模板：

```bash
AUTH_PASSWORD_CONCURRENCY=12 docker compose up -d --force-recreate api

python3 scripts/benchmarks/enterprise/bench_auth_login.py \
  --base-url http://127.0.0.1:8080 \
  --mode login \
  --login-user-count 100 \
  --login-email-domain example.com \
  --login-email-prefix bench-login-pool \
  --login-run-id 20260709-001 \
  --login-password correct-horse-battery \
  --requests 1000 \
  --concurrency 100 \
  --client-ip-mode pool \
  --client-ip-pool-size 200
```

压测时把 API 日志级别临时打开到 `git_ai_enterprise_server::services::passwords=debug`，查看 `password operation timing` 中的 `acquire_wait_ms`、`argon_ms` 和 `total_ms`。如果 `acquire_wait_ms` p95 长期高于 `argon_ms` p95，说明主要瓶颈是 Argon2 semaphore 排队。

## Postgres 慢查询观测

部署包的 `docker-compose.yml` 已为 PostgreSQL 启用：

```text
shared_preload_libraries=pg_stat_statements
pg_stat_statements.track=all
```

首次启用或修改该配置后需要重启 PostgreSQL：

```bash
docker compose restart postgres
docker compose restart api
```

在目标数据库中创建 extension：

```bash
docker compose exec postgres psql -U ${POSTGRES_USER:-gitai} -d ${POSTGRES_DB:-gitai_enterprise} \
  -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"
```

压测后查看最慢查询：

```sql
SELECT
    calls,
    ROUND(mean_exec_time::numeric, 2) AS mean_exec_ms,
    ROUND(max_exec_time::numeric, 2) AS max_exec_ms,
    ROUND(total_exec_time::numeric, 2) AS total_exec_ms,
    rows,
    LEFT(REGEXP_REPLACE(query, '\s+', ' ', 'g'), 240) AS query
FROM pg_stat_statements
WHERE dbid = (
    SELECT oid FROM pg_database WHERE datname = current_database()
)
ORDER BY mean_exec_time DESC
LIMIT 20;
```

查看连接状态：

```sql
SELECT COALESCE(state, 'unknown') AS state, COUNT(*) AS connections
FROM pg_stat_activity
WHERE datname = current_database()
GROUP BY COALESCE(state, 'unknown')
ORDER BY state;
```

仓库中的 `scripts/benchmarks/enterprise/postgres_observability.sql` 包含完整检查 SQL，可在压测后执行并保存输出到发布记录或 PR。

## 故障排查

### API 启动失败

```bash
# 查看 API 日志
docker compose logs api

# 常见问题:
# - DATABASE_URL 连接失败 -> 检查 PostgreSQL 是否健康
# - Redis 连接失败 -> 检查 Redis 是否健康
```

### 数据库连接失败

```bash
# 检查 PostgreSQL 健康
docker compose exec postgres pg_isready -U gitai -d gitai_enterprise

# 手动连接测试
docker compose exec postgres psql -U gitai -d gitai_enterprise
```

### MinIO 连接问题

```bash
# 访问 MinIO 管理界面
# http://<server-ip>:9001
# 默认账号: minioadmin / minioadmin
```
