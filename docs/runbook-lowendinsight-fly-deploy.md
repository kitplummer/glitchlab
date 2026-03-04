# Runbook: LowEndInsight fly.io Deployment

**App:** `lowendinsight`
**Platform:** fly.io
**Stack:** Elixir/Phoenix, Postgres, Redis, Oban
**Last updated:** 2026-03-04

---

## 1. Overview

LowEndInsight (LEI) is an Elixir/Phoenix application deployed on fly.io as a Docker container. The app analyzes open-source repositories for risk signals. It runs behind fly.io's edge proxy with TLS termination, backed by fly.io Postgres and Upstash Redis.

The deploy process: build a Docker image from the repo, push to fly.io's registry, rolling deploy replaces old machines, TCP health checks gate traffic cutover, automatic rollback if health checks fail.

---

## 2. Prerequisites

- [ ] `flyctl` CLI installed and authenticated (`fly auth whoami`)
- [ ] Access to the `lowendinsight` app (`fly apps list` shows it)
- [ ] On the correct branch (typically `main` or the release branch)
- [ ] CI passing on the branch being deployed
- [ ] No active incidents or ongoing maintenance windows

---

## 3. Pre-Deploy Checks

Run these before every deploy:

```bash
# 1. Confirm branch and CI status
git branch --show-current
# Verify CI is green for this commit

# 2. Check current app status
fly status --app lowendinsight

# 3. Verify fly.toml is unchanged (or changes are intentional)
git diff HEAD~1 -- apps/lowendinsight_get/fly.toml

# 4. Check Oban queue depth (optional — jobs are crash-safe)
# If queue is large, consider waiting for drain before deploy.
fly logs --app lowendinsight | grep -i oban | tail -5
```

**Go/no-go decision:**
- CI green + app healthy + no config changes → **go**
- CI failing or app unhealthy → **stop, investigate**
- fly.toml changed → **review changes carefully, then go**

---

## 4. Deploy

```bash
# From the repo root (where fly.toml is accessible)
fly deploy --app lowendinsight
```

**What happens:**
1. Docker image built from `apps/lowendinsight_get/Dockerfile`
2. Image pushed to fly.io registry
3. Rolling deploy: new machines start, old machines drain
4. Ecto migrations run in the release phase (before app starts)
5. TCP health checks on port 8080 gate traffic cutover
6. If health checks fail within the grace period, fly.io auto-rollbacks

**Expected output:** "deployed successfully" with machine IDs and health status.

**Typical duration:** 2-5 minutes (Docker build + push + rolling deploy).

---

## 5. Post-Deploy Smoke Tests

Run immediately after deploy completes.

### Required Checks (must pass)

| # | Method | Path | Expected Status | Expected Body | Purpose |
|---|--------|------|-----------------|---------------|---------|
| 1 | GET | `/` | 200 | Contains "LowEndInsight" | App is serving |
| 2 | GET | `/v1/cache/stats` | 200 | Valid JSON | Cache layer is connected |

### Optional Checks (informational)

| # | Method | Path | Expected Status | Purpose |
|---|--------|------|-----------------|---------|
| 3 | GET | `/doc` | 200 | API docs are accessible |
| 4 | POST | `/v1/analyze` | 200 or 202 | Analysis endpoint is functional |

### Running Smoke Tests

```bash
BASE_URL="https://lowendinsight.fly.dev"

# Required: App is serving
curl -s -o /dev/null -w "%{http_code}" "$BASE_URL/"
# Expected: 200

# Required: Cache stats
curl -s "$BASE_URL/v1/cache/stats" | head -c 200
# Expected: valid JSON response

# Optional: API docs
curl -s -o /dev/null -w "%{http_code}" "$BASE_URL/doc"
# Expected: 200

# Optional: Analyze endpoint (use a small public repo)
curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE_URL/v1/analyze" \
  -H "Content-Type: application/json" \
  -d '{"urls": ["https://github.com/kitplummer/xmpp4rails"]}'
# Expected: 200 or 202
```

### Decision Matrix

| Required Checks | Optional Checks | Assessment | Action |
|-----------------|-----------------|------------|--------|
| All pass | All pass | **Healthy** | Done |
| All pass | Some fail | **Degraded** | Investigate, no rollback |
| Any fail | — | **Unhealthy** | Rollback immediately |
| No data (deploy failed) | — | **Deploy failed** | Rollback immediately |

---

## 6. Rollback

If smoke tests indicate unhealthy or deploy failed:

```bash
# 1. List recent releases to find the rollback target
fly releases --app lowendinsight

# 2. Rollback to the previous healthy release
fly releases rollback --app lowendinsight

# 3. Re-run smoke tests to verify rollback succeeded
curl -s -o /dev/null -w "%{http_code}" "https://lowendinsight.fly.dev/"
# Expected: 200
```

**Important:** Rollback reverts the application code only. Database migrations are NOT rolled back — Ecto migrations are forward-only. If a migration caused the issue, a new forward migration must be written to fix it.

---

## 7. Monitoring

### Live Logs

```bash
fly logs --app lowendinsight
```

### App Status

```bash
fly status --app lowendinsight
fly machines list --app lowendinsight
```

### BEAM-Specific Log Patterns to Watch

| Pattern | Severity | Meaning |
|---------|----------|---------|
| `erl_crash.dump` | Critical | Erlang VM crash — immediate investigation |
| `** (EXIT)` | High | Process crash — check for cascading failures |
| `DBConnection` error | Medium | Postgres connection issue |
| `Redix` error | Medium | Redis connection lost — cache degraded |
| `Oban` error | Medium | Background job failure |
| `BEAM heartbeat` timeout | Critical | VM is unresponsive |
| `[error]` spike | High | Unusual error rate — investigate |

### Periodic Checks

- Health endpoint: every 5 minutes
- Machine status: every 15 minutes
- Oban queue depth: every 15 minutes
- TLS certificate expiry: daily

---

## 8. Configuration Reference

### fly.toml Settings

| Setting | Value | Notes |
|---------|-------|-------|
| `app` | `lowendinsight` | App name |
| `internal_port` | `8080` | Phoenix listens here |
| `auto_rollback` | `true` | Fly.io auto-rollbacks on health failure |
| `kill_signal` | `SIGINT` | Graceful BEAM shutdown |
| `kill_timeout` | `5` | Seconds before SIGKILL |
| `hard_limit` | `25` | Max connections per machine |
| `soft_limit` | `20` | Target connections per machine |
| `force_https` | `true` | All HTTP redirected to HTTPS |

### Environment Variables

| Variable | Set By | Notes |
|----------|--------|-------|
| `PORT` | fly.toml | `8080` — configured in fly.toml `[env]` |
| `ERL_AFLAGS` | fly.toml | `-proto_dist inet6_tcp` — IPv6 for fly.io internal network |
| `DATABASE_URL` | fly secret | Postgres connection string |
| `REDIS_URL` | fly secret | Redis connection string |
| `SECRET_KEY_BASE` | fly secret | Phoenix secret key |

**Secrets are managed by humans only.** Never set secrets through automated tooling. Use `fly secrets set` manually when needed.

---

## 9. Troubleshooting

### Deploy Hangs

- Check `fly logs --app lowendinsight` for build errors
- Check `fly machines list --app lowendinsight` for stuck machines
- If the Docker build is slow, check if dependencies changed (mix deps rebuild)

### Health Checks Pass but Errors in Logs

- Common during rolling deploy — old machines logging disconnect errors
- Wait 60 seconds after deploy completes, then re-check
- If errors persist, check `fly logs` for the specific error pattern

### No Running Machines

```bash
fly machines list --app lowendinsight
# If empty or all stopped:
fly machines start --app lowendinsight <machine-id>
```

### Postgres Connection Exhaustion

```bash
# Check connection count (requires fly postgres access)
fly postgres connect --app lowendinsight-db
# In psql: SELECT count(*) FROM pg_stat_activity;
```

If connections are near the limit, check for connection leaks in the app or consider scaling the Postgres instance.

---

*This runbook is Phase 0 of the [Delivery and SRE ADR](./adr-delivery-and-sre-ops-org.md).*
