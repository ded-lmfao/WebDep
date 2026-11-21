-- Port exposure and firewall sync state.
--
-- A pool port can be open (assignable to apps) without being reachable from the internet:
-- apps behind Caddy only need localhost or the mesh. `public` controls the firewall rule.
-- Existing pool ports start private: only Caddy's 80/443 stay publicly reachable.

ALTER TABLE node_ports ADD COLUMN IF NOT EXISTS public BOOLEAN NOT NULL DEFAULT FALSE;

-- 'ok' after a successful firewall sync, 'pending' when changes still have to reach the
-- node (offline, never synced), 'failed' when the last sync errored (see firewall_error).
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS firewall_state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS firewall_error TEXT;
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS firewall_synced_at BIGINT;
