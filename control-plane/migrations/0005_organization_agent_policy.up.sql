CREATE TABLE organization_agent_policies (
    organization_id uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    schema_version smallint NOT NULL,
    rules jsonb NOT NULL,
    updated_by_subject text NOT NULL,
    updated_at timestamptz NOT NULL
);