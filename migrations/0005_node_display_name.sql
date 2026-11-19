-- Optional operator-chosen label for a node. Empty means "use the node's own name".
-- Kept separate from nodes.name, which is the node's stable identity (unique, set at enrollment).
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT '';
