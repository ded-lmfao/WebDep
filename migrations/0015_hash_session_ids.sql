-- Sessions are stored by the SHA-256 of the id kept in the browser cookie, so a copy of the
-- database or a backup of it cannot be used to sign in. Existing sessions keep working.
UPDATE sessions SET id = encode(sha256(convert_to(id, 'UTF8')), 'hex');
