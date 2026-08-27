# Git-AI Enterprise Server 部署教程

本文档说明如何把当前仓库里的 `enterprise-server` 部署到一台 Linux 服务器，并让开发者客户端把 AI 代码追踪数据上传到 dashboard。

注意：`docs/guides/server-deployment.md` 和 `docs/guides/report-summary-guide.md` 主要描述旧版 `git-ai report server`，默认端口 `8787`，SQLite 存储，不是当前企业服务。部署企业 dashboard、登录、Postgres、Redis、MinIO、CAS 和 metrics 时，以本文档为准。

## 目标架构

推荐生产结构：

```text
developer machine
  -> git-ai client
  -> HTTPS domain
  -> reverse proxy
  -> enterprise-server api:8080
  -> Postgres
  -> Redis
  -> MinIO/S3
```

核心组件：

| 组件 | 作用 |
| --- | --- |
| `enterprise-server` API | OAuth 登录、metrics/CAS/report 接收、dashboard 页面和聚合 API。 |
| Postgres | 用户、组织、metrics、report、dashboard 数据。 |
| Redis | 限流等运行时状态。 |
| MinIO/S3 | CAS prompt 内容和对象存储。 |
| Nginx/Caddy | HTTPS、域名、反向代理。 |

## 部署路线选择

有两种方式：

| 方式 | 适用场景 | 说明 |
| --- | --- | --- |
| 源码 compose 部署 | 内网测试、快速验证 | 在服务器上 clone 仓库，`docker compose up -d --build`。 |
| 部署包部署 | 正式服务器、离线服务器 | 在构建机打包 Docker 镜像和 `enterprise-server/deploy/`，服务器只负责加载镜像和运行。 |

如果只是先把系统跑起来，推荐先用“源码 compose 部署”。正式上线再切到“部署包部署”。

## 一、服务器准备

推荐系统：

- Ubuntu 22.04+ 或 CentOS 8+
- Docker 24+
- Docker Compose v2+
- 内存至少 2GB，推荐 4GB+
- 磁盘至少 10GB，生产按数据量扩容

检查 Docker：

```bash
docker --version
docker compose version
```

如果第二条命令失败，或执行 `docker compose up -d --build` 时出现 `unknown shorthand flag: 'd' in -d`，说明服务器当前只有 Docker Engine/CLI，没有可用的 Docker Compose v2 插件。先安装 Compose v2，或临时使用旧命令 `docker-compose`。

防火墙建议：

| 端口 | 是否公网开放 | 说明 |
| --- | --- | --- |
| `80` | 是 | HTTP，通常用于 ACME/跳转 HTTPS。 |
| `443` | 是 | HTTPS 入口。 |
| `8080` | 内网或本机 | API 容器端口。若无反向代理，测试时可临时开放。 |
| `5433` | 否 | Postgres 宿主机映射端口。 |
| `6379` | 否 | Redis 宿主机映射端口。 |
| `9000` | 否 | MinIO S3 API。 |
| `9001` | 否 | MinIO 管理控制台。 |

## 二、源码 Compose 快速部署

### 1. 获取代码

```bash
git clone <your-repo-url>
cd git-ai-1.3.2/enterprise-server
```

### 2. 配置环境变量

```bash
cp .env.example .env
openssl rand -hex 32
vi .env
```

至少修改：

```env
JWT_SECRET=<openssl rand -hex 32 生成的值>
BASE_URL=https://git-ai.example.com
```

如果只在隔离开发网络测试且已明确接受 HTTP 风险，可以临时显式放行：

```env
BASE_URL=http://<server-ip>:8080
ALLOW_INSECURE_PUBLIC_URL=true
```

生产环境必须配置浏览器和 CLI 都可访问、证书有效的 HTTPS Origin：

```env
BASE_URL=https://git-ai.example.com
ALLOW_INSECURE_PUBLIC_URL=false
```

这里的 `BASE_URL` 是服务端信任的公开地址，必须只包含 scheme、host 和可选
port，不能带路径、查询参数、片段或凭据。服务端不会信任请求的 `Host` 或
`X-Forwarded-*` 来生成安装命令；`git-ai login`、Dashboard 帮助页和发布下载
都会使用这个地址。

当前 `enterprise-server/docker-compose.yml` 是偏开发配置，Postgres 和 MinIO 默认密码写在 compose 文件里。正式生产建议改成部署包方式，或单独维护生产 compose 文件。

### 3. 启动服务

```bash
docker compose up -d --build
```

查看状态：

```bash
docker compose ps
docker compose logs -f api
```

健康检查：

```bash
curl http://127.0.0.1:8080/health
```

预期返回类似：

```json
{"service":"git-ai-enterprise-server","status":"ok","version":"0.1.0"}
```

### 4. 确认 MinIO bucket 已创建

启动 API 前，需要在 MinIO 或外部 S3 中预先创建默认 bucket：

```text
git-ai-cas
```

如果后续 CAS 上传报 `NoSuchBucket`，请通过 MinIO Console 或对象存储管理工具创建该 bucket。

## 三、部署包方式

这种方式适合把镜像在本地或 CI 构建好，再上传到服务器。

### 1. 在构建机生成镜像包

在仓库根目录执行：

```bash
cd enterprise-server
docker build -t git-ai-enterprise-server-api:latest .
mkdir -p deploy/images
docker save git-ai-enterprise-server-api:latest -o deploy/images/git-ai-enterprise-server-api.tar
tar -czhf git-ai-enterprise-server-deploy.tar.gz -C deploy .
```

上传到服务器：

```bash
scp git-ai-enterprise-server-deploy.tar.gz user@server:/opt/
```

### 2. 在服务器解压

```bash
cd /opt
tar xzf git-ai-enterprise-server-deploy.tar.gz
cd git-ai-enterprise-server-deploy
```

如果解压出来不是这个目录名，而是直接出现 `docker-compose.yml`、`scripts/`、`migrations/`，就在当前解压目录继续操作。

### 3. 配置 `.env`

```bash
cp .env.example .env
openssl rand -hex 32
vi .env
```

建议至少配置：

```env
JWT_SECRET=<随机长密钥>
POSTGRES_PASSWORD=<强数据库密码>
BASE_URL=https://git-ai.example.com
API_PORT=8080
DATABASE_MAX_CONNECTIONS=20
DATABASE_MIN_CONNECTIONS=1
DATABASE_ACQUIRE_TIMEOUT_SECONDS=5
CAS_UPLOAD_CONCURRENCY=8

S3_ACCESS_KEY=<强随机值>
S3_SECRET_KEY=<强随机值>
S3_BUCKET=git-ai-cas
```

数据库连接池需要按 Postgres 容量和 API 实例数设置，不要简单把单实例连接数调大。建议从下面公式开始：

```text
DATABASE_MAX_CONNECTIONS =
  floor((Postgres max_connections - 预留连接数) / API 实例数)
```

例如 Postgres `max_connections=200`，为运维、迁移和后台任务预留 40 个连接，部署 4 个 API 实例时，每个 API 实例建议不超过 40 个连接。`DATABASE_MIN_CONNECTIONS` 通常保持 `1`，`DATABASE_ACQUIRE_TIMEOUT_SECONDS` 可先保持 `5`，在压测中观察 pool acquire timeout 后再调整。

### 4. 加载镜像

```bash
docker load -i images/git-ai-enterprise-server-api.tar
```

### 5. 启动服务

确保配置的 S3 bucket 已预先创建，然后启动服务：

```bash
docker compose up -d postgres redis minio
docker compose up -d api
```

检查：

```bash
docker compose ps
curl http://127.0.0.1:${API_PORT:-8080}/health
```

如果 HTTPS 反向代理对外监听 `38080`、转发到 API 的 `8080`，`.env` 应设置：

```env
BASE_URL=https://git-ai.example.com:38080
API_PORT=8080
```

`API_PORT=8080` 表示服务在服务器本机监听 `8080`；运维层把外部 `38080` 转发到服务器 `8080`。如果没有运维层转发，而是希望 Docker 直接监听宿主机 `38080`，则改成：

```env
BASE_URL=https://git-ai.example.com:38080
API_PORT=38080
```

## 四、首次初始化用户和组织

当前 `/verify` 授权页面需要数据库里至少已有一个用户。否则 `git-ai login` 授权后会报：

```text
No users found. Create a user first via admin API.
```

因为 admin API 本身需要管理员身份，第一次建议直接通过 SQL 初始化首个 owner 用户。

进入部署目录后执行：

```bash
docker compose exec -T postgres psql -U gitai -d gitai_enterprise <<'SQL'
WITH org AS (
  INSERT INTO organizations (name, slug)
  VALUES ('Linewell', 'linewell.com')
  ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name
  RETURNING id
),
usr AS (
  INSERT INTO users (email, name, personal_org_id)
  SELECT 'admin@linewell.com', 'Admin', id FROM org
  ON CONFLICT (email) DO UPDATE SET
    name = EXCLUDED.name,
    personal_org_id = COALESCE(users.personal_org_id, EXCLUDED.personal_org_id),
    updated_at = now()
  RETURNING id
)
INSERT INTO org_members (user_id, org_id, role)
SELECT usr.id, org.id, 'owner' FROM usr, org
ON CONFLICT (user_id, org_id) DO UPDATE SET role = 'owner';
SQL
```

把以下值换成你的实际信息：

| 值 | 示例 |
| --- | --- |
| 组织名 | `Linewell` |
| 组织 slug | `linewell.com` |
| 管理员邮箱 | `admin@linewell.com` |
| 管理员姓名 | `Admin` |

验证用户存在：

```bash
docker compose exec -T postgres psql -U gitai -d gitai_enterprise \
  -c "SELECT email, name FROM users ORDER BY created_at;"
```

如果执行上面的多行 SQL 时终端一直显示 `>`，说明 shell 还在等待 heredoc 结束。处理方式：

1. 按 `Ctrl+C` 取消当前输入。
2. 重新执行时，确保最后一行是单独的 `SQL`，前面没有空格，也不要复制提示符里的 `>`。
3. 如果仍然容易贴错，可以改用交互式 `psql`：

   ```bash
   docker compose exec postgres psql -U gitai -d gitai_enterprise
   ```

   进入 `psql` 后粘贴 SQL 内容，确认最后有分号 `;`，执行完成后输入：

   ```sql
   \q
   ```

## 五、配置 HTTPS 反向代理

生产环境建议不要让用户直接访问 `http://server-ip:8080`，而是使用 HTTPS 域名。

如果暂时没有域名，只能在隔离开发网络通过显式例外先跑通；HTTP 会暴露登录
凭据和安装内容，不能作为生产配置：

```env
BASE_URL=http://<server-ip>:38080
ALLOW_INSECURE_PUBLIC_URL=true
```

客户端也使用同一个外部地址：

```bash
git-ai login --server http://<server-ip>:38080
```

登录成功后客户端会自动保存服务地址，不需要额外执行 `git-ai config set`。

没有域名也可以做 HTTPS，但有两个限制：

1. **公网可信 IP 证书**：需要证书机构支持 IP address certificate，并且通常要通过标准 ACME challenge 验证。实际部署一般要求公网 `80` 或 `443` 能到达你的反向代理；只有 `38080 -> 8080` 映射时通常不够方便。
2. **自签证书**：可以给 IP 签自签证书，但浏览器和 `git-ai` 客户端默认不会信任它。每台客户端都要安装你的自签 CA 到系统信任库，否则登录和上传容易失败。

因此生产建议仍然是让运维分配一个域名或子域名，例如：

```text
git-ai.company.com -> 203.0.113.10
```

然后开放 `443`，由 Caddy/Nginx 终止 HTTPS，再反向代理到本机 `8080`。

### Caddy 示例

安装 Caddy 后，写入 `/etc/caddy/Caddyfile`：

```caddyfile
git-ai.example.com {
  reverse_proxy 127.0.0.1:8080
}
```

重载：

```bash
sudo systemctl reload caddy
```

`.env` 中要设置：

```env
BASE_URL=https://git-ai.example.com
```

重启 API：

```bash
docker compose up -d api
```

### Nginx 示例

```nginx
server {
    listen 80;
    server_name git-ai.example.com;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

生产中请再配 Let’s Encrypt 或公司证书，把 80 跳转到 443。

## 六、开发者客户端接入

开发者机器上先确认 `git` 走的是 git-ai shim：

```bash
command -v git
git-ai --version
```

登录并选择企业服务地址：

```bash
git-ai login --server https://git-ai.example.com
```

浏览器授权成功后，CLI 会自动保存该地址并将凭据绑定到该服务器。

终端会显示授权和注册地址。用户在浏览器中注册或登录后确认授权，
浏览器再通过本地 callback 将 authorization code 交给 CLI。

检查身份：

```bash
git-ai whoami
```

打开 dashboard：

```text
https://git-ai.example.com/me
```

注意：CLI 登录会把 token 存在开发者本机。浏览器访问 `/me` 时，如果没有浏览器 cookie，页面会要求输入 API key 或 Bearer token。

## 七、让数据进入 Dashboard

`git commit` 会在本地生成 `refs/notes/ai`，但 `git push` 默认不会把追踪数据上传到 enterprise-server。

目前最可靠的数据上传方式是 report upload：

```bash
git-ai report upload . --range HEAD^..HEAD --server https://git-ai.example.com
```

查看本地是否能扫描到数据：

```bash
git-ai report scan . --range HEAD^..HEAD --json
```

查看当前 commit 是否有 authorship note：

```bash
git notes --ref=refs/notes/ai show HEAD
```

如果使用 metrics 自动上传链路，需要 daemon 正常运行且客户端已登录或配置 API key。失败时可以手动补传：

```bash
git-ai flush-metrics-db
git-ai flush-cas
```

## 八、CI/CD 自动上传

推荐在 push 到主分支后由 CI 执行 `git-ai report upload`，这样 dashboard 不依赖开发者手动上传。

### GitHub Actions 示例

```yaml
name: Upload Git-AI Report

on:
  push:
    branches:
      - main

jobs:
  upload-git-ai-report:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Install git-ai
        run: |
          # 替换成你们内部的安装方式
          curl -sSL https://usegitai.com/install.sh | bash

      - name: Upload report
        env:
          GIT_AI_API_BASE_URL: ${{ secrets.GIT_AI_API_BASE_URL }}
          GIT_AI_API_KEY: ${{ secrets.GIT_AI_API_KEY }}
        run: |
          git-ai report upload . \
            --range "${{ github.event.before }}..${{ github.sha }}" \
            --server "${{ secrets.GIT_AI_API_BASE_URL }}"
```

如果 `github.event.before` 是空值或全 0，说明可能是首次 push 或特殊事件，可以改成上传最近一次提交：

```bash
git-ai report upload . --range HEAD^..HEAD --server "$GIT_AI_API_BASE_URL"
```

## 九、创建 API Key

API key 适合 CI/CD 使用。推荐通过 dashboard 的管理员页面创建。

如果需要通过 API 创建，需要先有管理员 Bearer token，然后调用：

```bash
curl -s -X POST https://git-ai.example.com/api/admin/api-keys \
  -H "Authorization: Bearer <admin-token>" \
  -H "Content-Type: application/json" \
  --data '{
    "name": "github-actions-main",
    "scopes": ["metrics:write", "cas:write", "cas:read", "reports:write"]
  }'
```

返回中的 `key` 只显示一次，请保存到 CI secret：

```text
GIT_AI_API_KEY=gai_...
GIT_AI_API_BASE_URL=https://git-ai.example.com
```

## 十、运维命令

查看服务：

```bash
docker compose ps
```

查看 API 日志：

```bash
docker compose logs -f api
```

查看数据库状态：

```bash
docker compose exec postgres pg_isready -U gitai -d gitai_enterprise
```

重启 API：

```bash
docker compose restart api
```

停止服务但保留数据：

```bash
docker compose down
```

不要在生产环境随意执行：

```bash
docker compose down -v
```

这个命令会删除 Docker volumes，包括 Postgres 和 MinIO 数据。

## 十一、备份和恢复

### Postgres 备份

```bash
mkdir -p backups
docker compose exec -T postgres pg_dump -U gitai -d gitai_enterprise \
  > backups/gitai_enterprise_$(date +%Y%m%d_%H%M%S).sql
```

恢复：

```bash
docker compose exec -T postgres psql -U gitai -d gitai_enterprise \
  < backups/gitai_enterprise_YYYYMMDD_HHMMSS.sql
```

### MinIO 数据

MinIO 数据在 Docker volume：

```text
minio_data
```

生产环境建议使用对象存储备份策略，或定期备份 Docker volume。CAS 数据可由 hash 去重，但丢失后会影响 prompt 内容读取。

## 十二、升级

### 源码 compose 升级

```bash
git pull
cd enterprise-server
docker compose up -d --build api
docker compose logs -f api
```

API 启动时会自动运行内嵌迁移。

### 部署包升级

在构建机重新生成镜像包，上传到服务器后：

```bash
docker load -i images/git-ai-enterprise-server-api.tar
docker compose up -d api
docker compose logs -f api
```

升级前先备份数据库。

## 十三、常见故障

### 1. `port is already allocated`

说明宿主机端口已被其他容器或进程占用。

查看占用：

```bash
docker ps
sudo lsof -i :6379
sudo lsof -i :8080
```

处理方式：

- 停掉已有服务。
- 或修改 compose 里的宿主机端口映射，例如 Redis 改成 `6380:6379`。
- 如果已有外部 Redis/Postgres，可以改 API 的 `REDIS_URL` / `DATABASE_URL` 指向外部服务。

如果报错是：

```text
Bind for 0.0.0.0:6379 failed: port is already allocated
```

优先推荐把 `enterprise-server/docker-compose.yml` 里的 Redis 宿主机端口映射删掉或注释掉：

```yaml
redis:
  image: redis:7-alpine
  # ports:
  #   - "6379:6379"
  healthcheck:
    test: ["CMD", "redis-cli", "ping"]
```

API 容器连接 Redis 用的是 Docker 内部服务名 `redis://redis:6379`，不依赖宿主机暴露 `6379`。删掉 `ports` 后，容器内通信仍然正常，也避免和服务器已有 Redis 冲突。

修改后执行：

```bash
docker compose down
docker compose up -d --build
```

如果你确实需要从宿主机访问这个 compose 里的 Redis，则改成不冲突的宿主机端口：

```yaml
ports:
  - "6380:6379"
```

### 2. `unknown shorthand flag: 'd' in -d`

说明服务器没有可用的 Docker Compose v2 插件，`docker compose ...` 没有被正确识别。

先检查：

```bash
docker compose version
docker-compose version
```

如果 `docker-compose version` 可用，可以临时把命令改成：

```bash
docker-compose up -d --build
docker-compose ps
docker-compose logs -f api
```

更推荐安装 Compose v2。Ubuntu/Debian 常见命令：

```bash
sudo apt-get update
sudo apt-get install -y docker-compose-plugin
docker compose version
```

CentOS/RHEL 系常见做法：

```bash
sudo yum install -y docker-compose-plugin
docker compose version
```

如果系统源没有 `docker-compose-plugin`，需要按 Docker 官方仓库方式安装 Docker Engine 和 Compose plugin。

### 3. `No users found`

说明还没有初始化用户。执行“首次初始化用户和组织”里的 SQL。

### 4. Dashboard 空白或没有新提交数据

先确认本地 commit 有 note：

```bash
git notes --ref=refs/notes/ai show HEAD
```

再手动上传一次：

```bash
git-ai report upload . --range HEAD^..HEAD --server https://git-ai.example.com
```

如果上传成功但 dashboard 仍为空，查 API 日志：

```bash
docker compose logs -f api
```

### 5. CAS 上传报 `NoSuchBucket`

说明 MinIO bucket 没初始化：

请通过 MinIO Console 或对象存储管理工具创建配置的 bucket，然后重启 API。

### 6. 登录后浏览器仍显示登录页

CLI 的 `git-ai login` 只保证本机 CLI 有 token。浏览器 dashboard 需要浏览器 cookie，或者在登录页输入 API key / Bearer token。

### 7. `git-ai config set api_base_url` 报 unknown key

说明本机 `git-ai` 版本太旧。先更新本地客户端：

```bash
task dev
```

或安装新版本后确认：

```bash
git-ai config --help
git-ai --version
```

## 十四、当前实现限制

上线前需要特别注意：

1. `git push` 默认不会上传追踪信息到 enterprise-server。要自动出现在 dashboard，需要 CI/CD 跑 `git-ai report upload`，或依赖稳定的 daemon metrics 上传链路。
2. 当前 `/verify` 设备授权页面会把设备授权给数据库中最早创建的用户。多用户生产环境上线前，应补完整网页登录/用户选择流程，或先只在受控内网使用。
3. `enterprise-server/deploy/migrations/005_*` 当前包含 Linewell/test data 初始化逻辑。正式部署前应改成你自己的组织和首个用户初始化，或删除测试数据迁移后用本文档 SQL 初始化。
4. 源码 `enterprise-server/docker-compose.yml` 暴露了 Postgres、Redis、MinIO 端口并使用默认密码，适合测试，不适合直接公网生产。

## 十五、最小上线检查清单

上线前至少确认：

- `curl https://git-ai.example.com/health` 返回 `status: ok`。
- 数据库中至少有一个 owner 用户。
- `git-ai login --server https://git-ai.example.com` 能完成授权。
- `/me` 能打开 dashboard。
- `git-ai report upload . --range HEAD^..HEAD --server https://git-ai.example.com` 能成功。
- dashboard 中能看到对应项目、开发者和 AI 行数。
- Postgres 和 MinIO 有备份策略。
- 公网只开放 80/443，内部组件端口不暴露公网。
