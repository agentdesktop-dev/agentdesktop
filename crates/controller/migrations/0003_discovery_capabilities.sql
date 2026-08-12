ALTER TABLE discoveries ADD COLUMN mcp_servers_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE discoveries ADD COLUMN skills_json TEXT NOT NULL DEFAULT '[]';
