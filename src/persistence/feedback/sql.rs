// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Explicit column list in the order [`read_row`] expects. Node uses `SELECT *`; naming the
/// columns keeps the mapping independent of the `ALTER TABLE` order migrations 12-19 produced.
pub(super) const COLUMNS: &str = "id, artifact_id, org, viewer_email, body, artifact_revision, parent_id, \
     anchor_path, anchor_x, anchor_y, anchor_w, anchor_h, anchor_approx, anchor_page, created_at, \
     resolved_at, resolved_by, author_source, external_author_id, external_author_display, \
     external_created_at, external_edited_at, external_deleted_at, anchor_kind, anchor_node_id, \
     anchor_quote";

/// The same list qualified for the `feedback f JOIN artifacts a` listings.
pub(super) const JOINED_COLUMNS: &str = "f.id, f.artifact_id, f.org, f.viewer_email, f.body, \
     f.artifact_revision, f.parent_id, f.anchor_path, f.anchor_x, f.anchor_y, f.anchor_w, \
     f.anchor_h, f.anchor_approx, f.anchor_page, f.created_at, f.resolved_at, f.resolved_by, \
     f.author_source, f.external_author_id, f.external_author_display, f.external_created_at, \
     f.external_edited_at, f.external_deleted_at, f.anchor_kind, f.anchor_node_id, \
     f.anchor_quote";

/// `insertStmt` — [lib/feedback.js:45-48]
pub(super) const INSERT_SQL: &str = "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, \
     artifact_revision, parent_id, anchor_path, anchor_x, anchor_y, anchor_w, anchor_h, \
     anchor_approx, anchor_page, anchor_kind, anchor_node_id, anchor_quote) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)";

/// `resolveStmt` — [lib/feedback.js:53-55]
pub(super) const RESOLVE_SQL: &str = "UPDATE feedback SET resolved_at = datetime('now'), resolved_by = ?1 \
     WHERE id = ?2 AND resolved_at IS NULL";

/// `reopenStmt` — [lib/feedback.js:56-58]
pub(super) const REOPEN_SQL: &str = "UPDATE feedback SET resolved_at = NULL, resolved_by = NULL \
     WHERE id = ?1 AND resolved_at IS NOT NULL";

/// `deleteStmt` — [lib/feedback.js:59]. Replies cascade via the migration-13 self FK.
pub(super) const DELETE_SQL: &str = "DELETE FROM feedback WHERE id = ?1";
