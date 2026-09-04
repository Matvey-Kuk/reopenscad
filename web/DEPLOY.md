# Deploying ReOpenSCAD to Google Cloud Run

Workspaces live in Cloud SQL for PostgreSQL. Rendered meshes and exports are
never stored — they are recomputed on demand from the workspace source, so the
database holds only documents, and there is no cache to invalidate.

These commands have been run end to end against a real GCP project
in `us-central1`. Where the first draft was wrong,
the corrected form is below.

Two environment prerequisites, both of which bite in a non-interactive shell:

```sh
# Domain mapping (§6) lives in the beta surface, which is not installed by
# default. Without --quiet the installer prompts and aborts.
gcloud components install beta --quiet

# gcloud's OAuth token expires; every command below fails with
# "Reauthentication failed" until you re-run this.
gcloud auth login
```

---

## 1. What the service needs from its environment

| Variable | Required | Purpose |
| --- | --- | --- |
| `PORT` | injected by Cloud Run | Listening port. Defaults to 8080 in the image. |
| `HOST` | set in the image | `0.0.0.0`. Cloud Run discards a container that only binds loopback. |
| `DATABASE_URL` | **yes** | Presence of this selects the Postgres backend. Absent means per-instance local files, which on Cloud Run means silent data loss. |
| `REOPENSCAD_PUBLIC_ORIGIN` | **yes** | The service's own origin, e.g. `https://reopenscad-abc123-uc.a.run.app`. See below — get this wrong and *every* request is answered 403. |
| `REOPENSCAD_TRUSTED_PROXY_HOPS` | recommended | `1` behind the `run.app` URL, `2` behind an external HTTPS load balancer — and at `2`, ingress **must** be restricted (see §5). Without it, per-IP rate limiting collapses into a single global bucket. |
| `REOPENSCAD_MAX_WORKSPACES` | recommended | Global retained-workspace ceiling, default 512. Raise it — see §7, this one can cost users their work. |
| `REOPENSCAD_SHUTDOWN_GRACE_MS` | optional | Drain window after SIGTERM, default 9000. |
| `REOPENSCAD_WEB_ROOT` | set in the image | Where the browser shell and icons live. |
| `REOPENSCAD_DATA_DIR` | unused with Postgres | Only consulted by the filesystem backend. |

### `REOPENSCAD_PUBLIC_ORIGIN` is a deployment trap

The server validates every request's `Host` header against this value and
**refuses to start** on a non-loopback address without it — that is what stops
DNS rebinding. But the URL only exists after the service does, so the first
deploy is a chicken-and-egg problem.

The image therefore bakes in the placeholder
`REOPENSCAD_PUBLIC_ORIGIN=http://localhost:8080`. It makes the container
*start*; every request on the real URL is answered 403 until you override it.
The §4 sequence deploys once, reads the assigned URL, and sets it — the second
command is not optional, and a 403-ing service is not a failed first deploy.

On a custom domain, use the custom origin. Only one origin can be authorised;
if you serve both, the other 403s.

---

## 2. One-time project setup

```sh
export PROJECT_ID=your-project-id
export REGION=us-central1
export SERVICE=reopenscad
export SQL_INSTANCE=reopenscad-db
export REPO=reopenscad

gcloud config set project "$PROJECT_ID"

gcloud services enable \
  run.googleapis.com \
  sqladmin.googleapis.com \
  artifactregistry.googleapis.com \
  cloudbuild.googleapis.com \
  secretmanager.googleapis.com

gcloud artifacts repositories create "$REPO" \
  --repository-format=docker \
  --location="$REGION" \
  --description="ReOpenSCAD container images"
```

---

## 3. Cloud SQL

The smallest tier is plenty: every query is a primary-key lookup or a bounded
sweep over small text documents.

```sh
# Generate a password locally; keep it out of shell history and out of env vars
# on the service. Hex is alphanumeric, so it needs no quoting in a libpq
# connection string, and 20 bytes is 160 bits of entropy.
DB_PASSWORD="$(openssl rand -hex 20)"

gcloud sql instances create "$SQL_INSTANCE" \
  --database-version=POSTGRES_16 \
  --tier=db-f1-micro \
  --edition=enterprise \
  --region="$REGION" \
  --storage-size=10GB \
  --storage-auto-increase \
  --backup

gcloud sql databases create reopenscad --instance="$SQL_INSTANCE"

gcloud sql users create reopenscad \
  --instance="$SQL_INSTANCE" \
  --password="$DB_PASSWORD"

export INSTANCE_CONNECTION_NAME="$(gcloud sql instances describe "$SQL_INSTANCE" \
  --format='value(connectionName)')"
echo "$INSTANCE_CONNECTION_NAME"        # PROJECT:REGION:INSTANCE
```

`instances create` returns before the instance is usable. It sits in
`PENDING_CREATE` for roughly ten minutes, and `databases create` fails until it
reaches `RUNNABLE`, so wait for it rather than running straight on:

```sh
until [ "$(gcloud sql instances describe "$SQL_INSTANCE" \
            --format='value(state)')" = RUNNABLE ]; do sleep 15; done
```

`db-f1-micro` is a shared-core Enterprise-edition machine type; it is rejected
under `--edition=enterprise-plus`, which is why the edition is pinned above.

The instance keeps its default public IP but with **no authorized networks**,
so nothing on the internet can open a socket to it. Cloud Run reaches it
through the Cloud SQL connector, which authenticates with IAM and brings its
own TLS. Do not add `--authorized-networks`.

There is no schema step: the server creates and migrates its own tables at
startup, idempotently, under a Postgres advisory lock so cold-starting
instances cannot race. `backend/sql/schema.sql` reproduces the statements for
review; applying it by hand makes the startup migration a no-op.

### The connection string

Attaching the instance mounts a unix socket at
`/cloudsql/PROJECT:REGION:INSTANCE`, which needs no TLS configuration of its
own. The whole string is a secret, because it contains the password:

```sh
printf 'host=/cloudsql/%s user=reopenscad password=%s dbname=reopenscad' \
  "$INSTANCE_CONNECTION_NAME" "$DB_PASSWORD" \
  | gcloud secrets create reopenscad-database-url --data-file=-
```

The runtime service account needs to read that secret and open the connector.
Substitute a dedicated service account for the default compute one if you have
one:

```sh
export RUNTIME_SA="$(gcloud projects describe "$PROJECT_ID" \
  --format='value(projectNumber)')-compute@developer.gserviceaccount.com"

gcloud secrets add-iam-policy-binding reopenscad-database-url \
  --member="serviceAccount:$RUNTIME_SA" \
  --role=roles/secretmanager.secretAccessor \
  --condition=None

gcloud projects add-iam-policy-binding "$PROJECT_ID" \
  --member="serviceAccount:$RUNTIME_SA" \
  --role=roles/cloudsql.client \
  --condition=None
```

`--condition=None` is not optional in a script. Without it `gcloud` prompts to
choose a binding condition, and a non-interactive shell fails the command
rather than defaulting to an unconditional binding.

Storing the whole string rather than just the password keeps the password out
of the service's environment entirely — `gcloud run services describe` shows
secret *references*, not values.

---

## 4. Build and deploy

Cloud Run runs x86-64 only; Cloud Build already does, which is why it is the
path below. Building on an Apple Silicon laptop means emulation and is far
slower.

### `gcloud builds submit` ignores `.dockerignore`

It reads **`.gcloudignore`**, and when there is neither that file nor a git
repository to derive one from, it uploads the entire build context. Here that
meant `backend/target/` went along for the ride: a 94,000-file, 8.2 GiB tarball
for an image whose real input is 1.2 MiB. `web/.gcloudignore` now exists and
mirrors `.dockerignore`; keep the two in sync, because Docker never gets the
chance to apply `.dockerignore` to files gcloud already refused to upload.

The builder also needs to read back the source tarball it just wrote. On a
project whose Cloud Storage bucket carries only the legacy project-role
bindings, the runtime service account is not a project editor and the build
fails with `storage.objects.get denied`:

```sh
gcloud projects add-iam-policy-binding "$PROJECT_ID" \
  --member="serviceAccount:$RUNTIME_SA" \
  --role=roles/cloudbuild.builds.builder --condition=None

gcloud storage buckets add-iam-policy-binding "gs://${PROJECT_ID}_cloudbuild" \
  --member="serviceAccount:$RUNTIME_SA" \
  --role=roles/storage.objectAdmin
```

```sh
IMAGE="$REGION-docker.pkg.dev/$PROJECT_ID/$REPO/$SERVICE:$(date +%Y%m%d-%H%M%S)"

# Build context is web/. Run from the repository root.
gcloud builds submit web --tag "$IMAGE"

# Alternative, building and pushing from your own machine:
#   gcloud auth configure-docker "$REGION-docker.pkg.dev"
#   docker buildx build --platform linux/amd64 -t "$IMAGE" --push web

gcloud run deploy "$SERVICE" \
  --image="$IMAGE" \
  --region="$REGION" \
  --platform=managed \
  --allow-unauthenticated \
  --add-cloudsql-instances="$INSTANCE_CONNECTION_NAME" \
  --set-secrets=DATABASE_URL=reopenscad-database-url:latest \
  --set-env-vars=REOPENSCAD_TRUSTED_PROXY_HOPS=1,REOPENSCAD_MAX_WORKSPACES=200000 \
  --cpu=2 \
  --memory=2Gi \
  --concurrency=40 \
  --timeout=180 \
  --min-instances=0 \
  --max-instances=4 \
  --execution-environment=gen2

# That deploy succeeds — the image carries a placeholder origin so the
# container starts — but the service answers 403 to everything until the real
# URL, which only exists now, is set. This second command is not optional.
export SERVICE_URL="$(gcloud run services describe "$SERVICE" \
  --region="$REGION" --format='value(status.url)')"

gcloud run services update "$SERVICE" \
  --region="$REGION" \
  --update-env-vars="REOPENSCAD_PUBLIC_ORIGIN=$SERVICE_URL"

curl -sS "$SERVICE_URL/api/health"      # {"ok":true,...}
```

Redeploys after the first only need the `gcloud builds submit` and
`gcloud run deploy` pair; the env vars and the Cloud SQL attachment persist.

---

## 5. Sizing, and why

**`--cpu=2`.** The server admits at most `MAX_CONCURRENT_JOBS = 2` renders per
instance; anything past that is rejected, not queued. A render pins one core
(measured: 99% of one CPU for 6.9 s meshing a `$fn=64` six-sphere difference),
so two vCPUs is exactly the instance's maximum parallel workload. A third would
never be used.

**`--memory=2Gi`, or `4Gi` for boolean-heavy work.** Steady state is nothing —
4 MiB resident at idle, 44 MiB during that render. The headroom is for the
exact CSG kernel, which holds BSP trees and intermediate meshes for the whole
boolean tree at once and scales with model complexity far faster than the
sampled mesher; two of those can be in flight, and each response is buffered
whole up to `MAX_OUTPUT = 64 MiB`. Running out is not graceful — Cloud Run
kills the instance and the user sees a dropped connection, not an error. Start
at 2 GiB, watch the memory metric against real models, jump to 4 GiB rather
than tuning finely.

**`--concurrency=40`.** Set by the long poll, not the renders: each open tab
holds one `/events` request continuously and the server caps pollers at
`MAX_CONCURRENT_POLLS = 32` (503 beyond that). Forty leaves eight slots for
renders, exports, static files and MCP alongside 32 subscribed tabs. Raising it
does not raise throughput, only which component says no.

**`--timeout=180`.** `RENDER_DEADLINE` is 120 s, and the platform timeout must
sit above it so a slow render returns a real error the UI can display instead
of having the connection cut from underneath it.

**`--max-instances=4`** is a cost fence, not a capacity estimate.

### Long polling prevents scale-to-zero — read this

`/events` parks a request for up to 5.2 s and the browser reconnects
immediately. Cloud Run bills instance-time whenever any request is in flight,
so **a single open browser tab keeps an instance billable continuously**, at
the full 2 vCPU / 2 GiB, rendering or not. Scale-to-zero only happens when
nobody has the page open.

Shortening the poll (`26 × 200 ms` in `handle_workspace_events`) does not help:
the tab just reconnects sooner. Neither does a WebSocket, which bills
identically. The two real choices are to accept it — for a handful of users
that is one small always-on instance, and seeing an MCP agent's edit within a
second is the feature — or to set `--min-instances=1`, which costs the same and
removes cold starts.

### Rate limiting behind the front end

`peer_addr()` inside a Cloud Run container is Google's front end, not the
caller. Left alone, every visitor shares one token bucket and the per-IP
limiter becomes a global one — the first burst locks everybody out.

`REOPENSCAD_TRUSTED_PROXY_HOPS` fixes this, and it is a hop *count* rather than
a "trust `X-Forwarded-For`" boolean on purpose. A caller may send its own
`X-Forwarded-For` and a compliant front end appends to it rather than replacing
it, so trusting the leftmost entry hands an attacker unlimited distinct
buckets. Only the right-hand entries were written by infrastructure, so the
server counts from the right:

- `1` — service reached on its `*.run.app` URL. The front end appended the
  caller's address, so it is the last entry.
- `2` — behind an external HTTPS load balancer, which appends its own entry
  after the front end's.
- `0` (the default) — no proxy: the header is ignored entirely and the socket
  address is used, exactly as it is in local development.

Too high fails safe (a proxy address, i.e. one shared bucket again); too low
fails open (the caller chooses its bucket). Confirm it against your topology.

**At `2`, you must also close direct ingress.** Cloud Run's default ingress is
`all`, so even behind a load balancer the `*.run.app` URL still answers the open
internet — and a request arriving that way has only *one* genuine appended
entry. A caller sending `X-Forwarded-For: 1.1.1.1, 2.2.2.2` produces a
three-entry chain whose second-from-the-right is its own `2.2.2.2`: exactly the
forgeable bucket the hop count exists to prevent. Restrict ingress so the
balancer is the only path in:

```sh
gcloud run services update "$SERVICE" --region="$REGION" \
  --ingress=internal-and-cloud-load-balancing
```

With hops `1` and the `run.app` URL there is no equivalent hole: the rightmost
entry is always the one Google's front end appended, and nothing the caller
sends can end up to the right of it.

### Shutdown

The container handles SIGTERM: it stops accepting new connections, lets
in-flight requests finish, and exits 0. Cloud Run allows 10 s before SIGKILL,
and the default drain window is 9 s to stay inside it.

A render can legitimately run the full 120 s `RENDER_DEADLINE`, which no 10 s
window accommodates, so an instance replaced mid-render drops that render. That
is a retry for the user, not lost data: a render commits nothing, and the
source was saved before it started.

---

## 6. A custom domain

Cloud Run domain mappings are available in `us-central1`. The mapping serves
through the same Google front end as the `run.app` URL, so it adds **no**
extra `X-Forwarded-For` hop: `REOPENSCAD_TRUSTED_PROXY_HOPS` stays `1`.

### Ownership must be verified first

`domain-mappings create` fails unless the domain is already verified to the
calling account. Check, and verify through Search Console if it is missing:

```sh
gcloud domains list-user-verified
```

Search Console offers to do this by granting Google OAuth access to the whole
DNS account at the registrar. Prefer the manual route — pick **"Any DNS
provider"** in the *Instructions for* dropdown, which yields a single
`google-site-verification=…` TXT record on `@` to add by hand. It grants Google
nothing beyond proof of ownership.

### Create the mapping, then add the records it prints

```sh
gcloud beta run domain-mappings create \
  --service="$SERVICE" --domain=example.com --region="$REGION"
```

For a regional service this is always the same eight records on `@` — four A
and four AAAA:

```
A     216.239.32.21   216.239.34.21   216.239.36.21   216.239.38.21
AAAA  2001:4860:4802:32::15  2001:4860:4802:34::15
      2001:4860:4802:36::15  2001:4860:4802:38::15
```

At a registrar whose apex already carries a parking or site-builder A record,
that record must be replaced, not merely supplemented, or traffic keeps landing
on the old host half the time.

Then point the origin at the custom domain. **`REOPENSCAD_PUBLIC_ORIGIN` holds
exactly one origin**, so this is a switch, not an addition — the `run.app` URL
starts answering 403 the moment it takes effect:

```sh
gcloud run services update "$SERVICE" --region="$REGION" \
  --update-env-vars=REOPENSCAD_PUBLIC_ORIGIN=https://example.com
```

Certificate issuance begins only once the records resolve and takes anywhere
from a few minutes to about an hour. Watch it with:

```sh
gcloud beta run domain-mappings describe --domain=example.com \
  --region="$REGION" --format=json \
  | python3 -c "import sys,json;[print(c['type'],c['status'],c.get('message','')) \
      for c in json.load(sys.stdin)['status']['conditions']]"
```

`DomainRoutable: True` means the DNS is right; `Ready: True` means the
certificate is live and the domain is actually serving.

### `www` does not come for free

The single-origin rule means a `www` mapping would answer 403 on every request,
which is worse than not existing. Serving both needs a redirect in front —
registrar-level forwarding of `www` to the apex is the cheap option — not a
second domain mapping.

### Ingress must stay `all` for a domain mapping

`--ingress=internal-and-cloud-load-balancing` admits only Google-internal
traffic and *customer* HTTP(S) load balancers. A Cloud Run domain mapping is
neither, so restricting ingress silently breaks the custom domain. It is also
unnecessary at one hop: §5 explains that the forgeable-bucket hole exists only
at hops `2`, behind a balancer that a caller can bypass by going straight to
`run.app`. At hops `1` the rightmost `X-Forwarded-For` entry is always the one
Google's front end appended, and nothing a caller sends can get to the right of
it — so `--ingress=all` is both required and safe here.

---

## 7. Operations

```sh
# Logs
gcloud run services logs read "$SERVICE" --region="$REGION" --limit=100

# Connect to the database
gcloud sql connect "$SQL_INSTANCE" --user=reopenscad --database=reopenscad

# How much is actually stored
#   SELECT count(*), pg_size_pretty(pg_total_relation_size('workspaces'))
#     FROM workspaces;
```

Retention runs itself: every ten minutes a sweeper thread deletes workspaces
idle for more than 14 days and trims the table to the newest
`REOPENSCAD_MAX_WORKSPACES`, with two set-based `DELETE`s.

### Request logs contain workspace credentials

A workspace URL is the only key to that workspace, and Cloud Run's request log
records the full request path — so `/workspaces/<id>` and
`/api/workspaces/<id>` land in Cloud Logging on every hit, readable by anyone
holding `roles/logging.viewer` on the project for the whole retention window
(30 days by default). The server itself never logs a path, an id or any source
code; this is the platform's log, not the application's.

Treat project log access as workspace access, and shorten the exposure:

```sh
# Drop Cloud Run request logs for this service; the app's own stdout/stderr
# (startup banner, storage errors) is unaffected and stays queryable.
gcloud logging sinks update _Default --log-filter='
  NOT (logName:"run.googleapis.com%2Frequests"
       AND resource.labels.service_name="'"$SERVICE"'")
'

# Or keep them and let them expire sooner.
gcloud logging buckets update _Default --location=global --retention-days=7
```

`gcloud logging sinks describe _Default` first — the update **replaces** the
filter rather than appending to it.

One application log line can carry an id: `describe()` in `pgstore.rs` prints
the raw Postgres error, and a constraint violation renders its `DETAIL` with
the offending key. That needs a genuine primary-key collision on a 112-bit id
to happen, so it is left in place — the operator value of the real driver error
outweighs it. Redact it if project log access ever widens.

### Set `REOPENSCAD_MAX_WORKSPACES` before you take real traffic

The 512 default is inherited from the filesystem store, where it capped one
developer's disk. In a shared database the cap is **global across every user**,
workspace creation is **unauthenticated**, and the sweeper evicts the *least
recently updated* rows. A burst of 512 fresh workspaces — within reach of a
distributed client, which per-IP rate limiting only slows — permanently deletes
other people's work, oldest first, with no undo: the URL was the only handle
anyone had on it.

Rows are small (bounded at 2 MiB of source, usually a few kilobytes), so the
ceiling is cheap to raise:

```sh
gcloud run services update "$SERVICE" --region="$REGION" \
  --update-env-vars=REOPENSCAD_MAX_WORKSPACES=200000
```

Pick a number far above any plausible legitimate total and watch
`SELECT count(*) FROM workspaces` against it. Lowering it below 512 is ignored.
Beyond a toy deployment, put authentication in front of `POST /api/workspaces`
rather than relying on a number here.

The 14-day idle TTL is compiled in as `WORKSPACE_TTL_MS`
(`backend/src/main.rs`). It only deletes untouched workspaces, but it does mean
a bookmarked URL expires.

Backups are the Cloud SQL automated ones enabled in §3. Verify a restore before
relying on it — given the eviction hazard above, it is the actual safety net.

---

## 8. Teardown

```sh
gcloud run services delete "$SERVICE" --region="$REGION"
gcloud sql instances delete "$SQL_INSTANCE"
gcloud secrets delete reopenscad-database-url
gcloud artifacts repositories delete "$REPO" --location="$REGION"
```
