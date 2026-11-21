# Upgrading WebDep

## Before you upgrade

WebDep backs up its control database automatically before it applies new database migrations.
The dump is written to `data/backups/control-db/pre-upgrade-v<from>-to-v<to>-<time>.dump`
(the five newest are kept). If the backup can't be taken, the upgrade stops and nothing is
changed. Set `WEBDEP_SKIP_DB_BACKUP=1` only if you have your own backup.

Installs that use an external `DATABASE_URL` are not backed up automatically: back that
database up yourself first.

Update every VPS together (**Admin Panel › Nodes › Update Fleet**). A worker running an older
version shows **Update needed**; commands it doesn't understand fail with a message telling you
to update it instead of failing silently.

## Rolling back

1. Stop WebDep.
2. Restore the dump taken before the upgrade (replace the port, user, and database with the
   values in `data/control-pg/conn.json`, and the file name with your dump):

   ```bash
   pg_restore --clean --if-exists -h 127.0.0.1 -p <port> -U <user> -d <database> data/backups/control-db/<dump file>
   ```

3. Start the older WebDep version.

From this release on, an older build also starts on a database that a newer build already
migrated: migrations it doesn't know are ignored instead of stopping the panel. Restoring the
dump is still the safest way back, because the newer build may have stored data the older one
doesn't read.

## What changes when you upgrade to the user-management release

- **Accounts**: users sign in with a username or email. People can request an account from the
  sign-in page (turn this off in **Platform Settings › Accounts**). Requests wait in
  **Admin Panel › Requests**.
- **Permissions move to each app**: every user's existing permissions are copied once, on first
  start, to exactly the apps they could reach before. Nobody gains or loses access.
- **Deleting an app is its own permission** ("Delete app (owner)"). Users who could delete apps
  before (they had the website, service, or database manage permission) keep that right
  automatically.
- **Permissions from earlier versions** that the new editor doesn't offer (for example platform
  settings or system powers) are kept and listed under *Permissions from an earlier version* in
  **Admin Panel › Users › Edit**, where an admin can remove them.
- **Moving apps between nodes** is now admin-only.
- **Account security** (Platform Settings › Accounts): limit account requests to certain email
  domains and require two-factor authentication for admins or everyone. A rule that covers you
  can only be turned on once your own account has 2FA.
- **Forgotten passwords**: an admin creates a one-time reset link in Users › Edit › Reset link
  (valid 24 hours). The main admin's password is still set with `ADMIN_PASSWORD` in `.env`.
- **Sessions**: My Account lists where you're signed in; admins see and end any user's sessions
  (Users › Edit › Sessions).
- **Sign-up bot check**: the request form runs a short proof-of-work check in the browser, and
  requests are capped at 30 per hour in total. Behind a reverse proxy on the same machine (Caddy),
  the real client address comes from `X-Forwarded-For`; from anywhere else the header is ignored.
- **Port ranges**: nodes can be given a port range in **Admin Panel › IP Addresses**. Nodes
  without one keep the old automatic ports.
