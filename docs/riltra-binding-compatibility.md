# Code binding compatibility

An export containing code bindings is tied to the canonical path of its
registered repository. Import validates that path digest and the repository
name. It cannot be imported into a clone registered at a different path or on
another machine. Record-only exports do not have this constraint; bindings are
not automatically relocated or rebound.

Knowledge stores at schema versions 1 and 2 can be read and migrated on a write
that needs newer features. Schema version 3 requires a binary that understands
it; an older binary rejects it even when the store has no bindings. Backups of
schema 1 or 2 stores keep manifest version 1. A schema 3 backup uses manifest
version 2 and is not a downgrade path.

`knowledge at` returns `page-empty` when the requested page has no rows but
matching bindings exist at other offsets. When anchor resolution fails,
`unresolved_reason` describes the failure. Callers should also inspect
freshness and pagination before drawing conclusions.

The code-index schema version is unchanged. A live database open adds the
lookup index; read-only MCP queries do not modify stores.
