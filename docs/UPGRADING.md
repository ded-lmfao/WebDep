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

An older build also starts on a database that a newer build already
migrated: migrations it doesn't know are ignored instead of stopping the panel. Restoring the
dump is still the safest way back, because the newer build may have stored data the older one
doesn't read.
