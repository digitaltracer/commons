# Deploying Commons behind an existing nginx

Two ways to run Commons behind a host's own nginx, which terminates TLS. The example host is `commons.adarshnb.com`.
- **Docker compose** (the self-host path, below): PostgreSQL and `commonsd` in containers.
- **systemd on an existing host** (at the end): `commonsd` as a service against a PostgreSQL the host already runs. `commons.adarshnb.com` uses this one.

What the compose stack runs:
- `postgres` (16-alpine): no published ports, on an internal-only network, data in the `pgdata` volume.
- `commonsd`: published on `127.0.0.1:8090` only, files in the `files` volume. It runs its migrations at startup and waits for Postgres to be healthy first.
- Both are capped at 1 vCPU and 2 GB each, 2 vCPU and 4 GB in all.

## Prerequisites

- A Linux x86_64 host with Docker Engine and the compose plugin (`docker compose version` works).
- nginx and certbot, with certbot's nginx plugin (`python3-certbot-nginx` on Ubuntu).
- A DNS A (and AAAA, if the host has IPv6) record for `commons.adarshnb.com` pointing at the host.
- Port 8090 free on 127.0.0.1. To use another port, change both `deploy/compose.yaml` and the nginx site.
- About 3 GB of disk for the first build (the Rust build image and its cache), plus data.

## 1. Copy the repo to the server

From a checkout on your machine, with no build outputs or local secrets:

```sh
rsync -az --delete \
  --exclude .git --exclude target --exclude .agents \
  --exclude apps/desktop/gen --exclude apps/desktop-ui/dist \
  --exclude deploy/.env \
  ./ vps:/opt/commons/
```

The rest of this runbook runs on the server, in `/opt/commons`.

## 2. Create the env file

```sh
cd /opt/commons
cp deploy/env.example deploy/.env
chmod 600 deploy/.env
openssl rand -hex 24   # paste as POSTGRES_PASSWORD
```

Edit `deploy/.env`:
- `POSTGRES_PASSWORD`: the generated value. Keep it URL-safe, because it goes into the database URL.
- `COMMONS_PUBLIC_URL=https://commons.adarshnb.com`.
- `COMMONS_REGISTRATION`: `open`, `closed` or `allowlist`. With `allowlist`, set `COMMONS_REGISTRATION_ALLOWLIST` to the comma-separated emails that may register. Refused registrations get a 403 with a message the desktop app shows.
- `COMMONS_MAX_FILE_BYTES=104857600` (100 MiB) and `COMMONS_USER_QUOTA_BYTES=2147483648` (2 GiB) for a test box. If you change the file limit, change `client_max_body_size` in the nginx site to match.
- `COMMONS_SMTP_URL` is left empty, so **password-reset email is off**: reset requests are accepted and logged, but no email goes out and no link is kept. Set it, and `COMMONS_MAIL_FROM`, to turn reset on.
- `COMMONS_TRUSTED_PROXIES`: login and reset are limited per email and per client IP. The client IP comes from nginx's `X-Real-IP` only when the connection is from loopback or a listed proxy. Under compose, nginx connects through the `edge` network's gateway, so list that address (`docker network inspect commons_edge`); otherwise every client shares one per-IP limit.

`deploy/.env` is git-ignored and excluded from the Docker build context. Postgres reads its password only on first start, when the volume is empty. To change it later, run `ALTER ROLE commons PASSWORD '…'` and update the file.

## 3. Start the stack

```sh
docker compose -f deploy/compose.yaml up -d --build
docker compose -f deploy/compose.yaml ps        # both services become "healthy"
curl -fsS http://127.0.0.1:8090/v1/health      # prints: ok
```

The first build compiles `commonsd` in release mode and takes several minutes. Later builds reuse the BuildKit cache.

## 4. Install the nginx site and get a certificate

```sh
sudo cp deploy/nginx/commons.adarshnb.com.conf /etc/nginx/sites-available/
sudo ln -s /etc/nginx/sites-available/commons.adarshnb.com.conf /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d commons.adarshnb.com
```

The site is a plain port-80 block. `certbot --nginx` adds the 443 listener, the certificate and the redirect to it, and certbot's timer renews the certificate. The site proxies `/v1/socket` as a WebSocket with 1-hour idle timeouts, and streams uploads to `commonsd` (`proxy_request_buffering off`).

## 5. Check it

```sh
curl -fsS https://commons.adarshnb.com/v1/health   # ok
curl -fsS https://commons.adarshnb.com/v1/meta     # protocol and server versions
```

Then add the account in the desktop app with the server URL `https://commons.adarshnb.com`. A working socket shows as the account staying connected; nginx's access log shows `GET /v1/socket` with status 101.

## Upgrading

Copy the new tree as in step 1. Then, on the server:

```sh
cd /opt/commons
docker compose -f deploy/compose.yaml up -d --build
```

This rebuilds `commonsd` and replaces its container. Migrations run on startup. Clients see a short disconnect and reconnect by themselves. Take a backup first.

## Backup

The database and the files volume are both needed. Back them up together:

```sh
cd /opt/commons
stamp=$(date +%F-%H%M)
docker compose -f deploy/compose.yaml exec -T postgres \
  pg_dump -U commons -d commons -Fc > commons-db-$stamp.dump
docker compose -f deploy/compose.yaml exec -T commonsd \
  tar -czf - -C /var/lib/commons/files . > commons-files-$stamp.tgz
```

Copy both files off the host.

Restore into an empty stack, for example after `docker compose … down -v`:

```sh
docker compose -f deploy/compose.yaml up -d postgres
docker compose -f deploy/compose.yaml exec -T postgres \
  pg_restore -U commons -d commons --clean --if-exists < commons-db-STAMP.dump
docker compose -f deploy/compose.yaml run --rm -T --no-deps --entrypoint tar commonsd \
  -xzf - -C /var/lib/commons/files < commons-files-STAMP.tgz
docker compose -f deploy/compose.yaml up -d
```

## Logs

```sh
docker compose -f deploy/compose.yaml logs -f commonsd
docker compose -f deploy/compose.yaml logs --since 1h postgres
```

Logs rotate at 10 MB, keeping 5 files per service. Set `RUST_LOG` in `deploy/.env` (for example `info,server=debug`) and re-run `up -d` to change the log level. nginx logs go to `/var/log/nginx/`.

## Stopping

```sh
docker compose -f deploy/compose.yaml down      # keeps the volumes
docker compose -f deploy/compose.yaml down -v   # deletes the database and files
```

# systemd on an existing host

For a host that already runs PostgreSQL 16 (here on `127.0.0.1:5433`) and nginx, without Docker. `commonsd` runs as the `commons` system user under `deploy/systemd/commonsd.service`. The unit caps it at 2 vCPU (`CPUQuota=200%`) and 4 GB (`MemoryMax=4G`), and confines writes to `/var/lib/commons`.

## Prerequisites

- Ubuntu 24.04 x86_64 with PostgreSQL 16. Ubuntu's `postgresql-16` package ships `unaccent`, which the search migration creates.
- A Rust toolchain of 1.90 or later (`rustup`), plus `build-essential` and `pkg-config`.
- nginx, certbot and the DNS record, as in the compose path.
- Port 8090 free on 127.0.0.1.

## 1. Build the binary on the host

Copy the repo as in the compose path's step 1, then:

```sh
cd /opt/commons
cargo build --release --locked -p server --bin commonsd
```

This builds only the server and its dependencies. It takes several minutes the first time.

## 2. Create the role and database

```sh
openssl rand -hex 24   # the role's password
sudo -u postgres psql -p 5433 <<'SQL'
CREATE ROLE commons LOGIN PASSWORD 'PASTE-PASSWORD';
CREATE DATABASE commons OWNER commons;
SQL
```

The role needs no superuser rights. `commonsd` runs its migrations at startup. `unaccent` is a trusted extension, so the database owner can create it. Check that `pg_hba.conf` allows password login (`scram-sha-256`) for `commons` from `127.0.0.1`. Ubuntu's default does.

## 3. Install the binary, the unit and the env file

```sh
sudo useradd --system --user-group --home-dir /var/lib/commons --shell /usr/sbin/nologin commons
sudo install -m 0755 target/release/commonsd /usr/local/bin/commonsd
sudo install -m 0644 deploy/systemd/commonsd.service /etc/systemd/system/commonsd.service
sudo install -d -m 0750 -o root -g commons /etc/commons
sudo install -m 0640 -o root -g commons deploy/systemd/commonsd-env.example /etc/commons/commonsd.env
sudoedit /etc/commons/commonsd.env
```

In `/etc/commons/commonsd.env`:
- `COMMONS_DATABASE_URL=postgres://commons:PASSWORD@127.0.0.1:5433/commons`, with the role's password;
- `COMMONS_BIND=127.0.0.1:8090` and `COMMONS_FILES_DIR=/var/lib/commons/files`;
- `COMMONS_PUBLIC_URL`, the registration mode and the limits, as in the compose path's step 2. SMTP stays empty, so password-reset email is off.

systemd creates `/var/lib/commons` (the unit's `StateDirectory`), owned by `commons`, and `commonsd` creates `files/` inside it.

## 4. Start it

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now commonsd
systemctl status commonsd
curl -fsS http://127.0.0.1:8090/v1/health      # ok
```

## 5. nginx and certbot

These are the same as the compose path's step 4, using the same site file, `deploy/nginx/commons.adarshnb.com.conf`. Then check as in step 5.

## Upgrading

```sh
cd /opt/commons            # after copying the new tree
cargo build --release --locked -p server --bin commonsd
sudo install -m 0755 target/release/commonsd /usr/local/bin/commonsd
sudo systemctl restart commonsd
```

Take a backup first. Migrations run on startup.

## Backup

```sh
stamp=$(date +%F-%H%M)
sudo -u postgres pg_dump -p 5433 -d commons -Fc > commons-db-$stamp.dump
sudo tar -czf commons-files-$stamp.tgz -C /var/lib/commons/files .
```

To restore, stop `commonsd`, then run `pg_restore -p 5433 -d commons --clean --if-exists` as `postgres` and untar into `/var/lib/commons/files`. Then run `chown -R commons:commons` on that directory and start the service.

## Logs

```sh
journalctl -u commonsd -f
journalctl -u commonsd --since "1 hour ago"
```

Set `RUST_LOG` in `/etc/commons/commonsd.env` and run `sudo systemctl restart commonsd` to change the log level.
