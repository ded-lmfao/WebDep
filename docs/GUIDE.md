# Panel guide

- **Accounts**: users sign in with a username or email. People can request an account from the
  sign-in page (turn this off in **Platform Settings › Accounts**). Requests wait in
  **Admin Panel › Requests**.
- **Permissions are per app.** Deleting an app is its own permission ("Delete app (owner)").
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
  without one assign ports automatically.
- **Daily backups**: the control database is backed up once a day to
  `data/backups/control-db/daily-<time>.dump` (seven kept by default).
- **Audit history** is kept for 365 days by default (0 keeps everything). The Audit page loads
  200 entries at a time, filters by user or text on the server, and exports to CSV.
- **Notifications**: set the `notify_webhook_url` setting (Discord, Slack, or anything that
  accepts JSON) to hear about new account requests, a VPS going offline or coming back, a node
  running low on free ports, and apps stopped for going over their disk limit.
- **Creating apps is a permission**: give a user *Create websites*, *Create services*, or
  *Create databases* in **Users › Edit**. They own every app they create (all permissions on
  it, including delete).
- **Several IPs per VPS** (**Admin Panel › IP Addresses**): add the public IPs a VPS has, then
  choose one for each website or service. Caddy then serves its domains on that IP only; apps
  without a choice are served on every IP. Moving an app to another VPS clears the
  choice. An IP can only be removed once no app uses it.
- **Resource limits** (set when creating a website, service, or database; changed later by
  admins in **Admin Panel › App Configs**): CPU (100% = one core), memory, disk, and number of
  backups. Empty or 0 means no limit. CPU and memory are enforced with cgroups v2 on Linux
  (WebDep must run as root, or have write access to `/sys/fs/cgroup`) and with Job Objects on
  Windows (a hard CPU cap and a memory limit for the app and everything it starts). Where limits
  can't be enforced, the App Configs page says why and the app's log notes it. Disk is a soft
  limit checked every minute on this server: an app over
  it is stopped and can't be started until it is under the limit or an admin raises it (apps on
  other nodes are not measured yet). The backup limit refuses a new backup once the app holds
  that many; databases have their own backups.
- **Backups of websites and services**: a backup is one `.tar.gz` under
  `data/backups/apps/project-<id>/` on the server where the app runs (Web-Core for apps on the
  Core server, the VPS itself for apps on other VPSes), outside the app's own folder, with its
  size, file count, creator, status (Creating, Complete, Failed) and SHA-256. It is built in the
  background and includes everything under the app's Files folder (including `node_modules` and
  `.git`) plus its environment variables. A backup cut off by a restart is marked Failed and its
  unfinished file is removed. Deleting a backup deletes its file, and deleting an app removes all
  of its archives. The optional "also keep a copy on
  Web-Core" for apps on other VPSes is off by default and makes a folder copy; a
  backup stored on a VPS can't be downloaded from Core yet.
- **Backups screen** (**Releases › Backups**): each backup shows its note, status (Creating,
  Complete, Failed with the reason), size and file count, when it was made and by whom. The
  buttons restore it, download it as a `.tar.gz`, lock it (a locked backup can't be deleted),
  or delete it. **Create backup** asks for an optional note and optional file or folder names to
  leave out. The header shows how many backups the app holds against its limit. Restoring asks
  you to type the app's name first.
- **Restoring a backup replaces the app's files; it never merges.** WebDep checks the backup's
  checksum, unpacks it beside the app, stops the app, swaps the folders, restores the saved
  environment variables (replacing the current ones), and starts the app again if it was
  running. Files that weren't in the backup are removed. If anything fails before the swap,
  nothing changes and the app keeps running; if the swap itself fails, the old files are put
  back. The files that were replaced are kept for 24 hours in `.pre-restore/` inside the app's
  folder so you can undo. Restoring and deploying can't run at the same time for one app. This
  also applies to apps on other
  VPSes (the VPS does the file work; a VPS running an older WebDep shows the usual "update this
  node" message). A backup that holds only environment variables restores just those and never
  touches the files.
- **Audit history per app** keeps the newest 100 entries for each website, service and database (each has its own 100); older ones
  are deleted as new ones arrive. Panel-wide entries keep the days-based retention.
