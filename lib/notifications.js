// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Gallery notification projections derived from tenant-scoped viewer feedback.
import db from "./db.js";

const EPOCH = "1970-01-01 00:00:00";

export function createNotificationStore({ db: database }) {
  const recentAdmin = database.prepare(`
    SELECT f.id, f.artifact_id, a.title AS artifact_title, a.org,
           f.body, f.viewer_email, f.author_source, f.external_author_id,
           f.external_author_display, f.created_at, f.parent_id,
           (f.resolved_at IS NOT NULL) AS resolved,
           (f.anchor_x IS NOT NULL AND f.anchor_y IS NOT NULL) AS has_anchor,
           (f.created_at > COALESCE(r.seen_at, @epoch)) AS unread
    FROM feedback f
    JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
    LEFT JOIN notification_reads r ON r.viewer_email = @email
    WHERE (f.viewer_email IS NULL OR f.viewer_email <> @email)
    ORDER BY f.created_at DESC, f.id DESC
    LIMIT @limit
  `);
  const recentMember = database.prepare(`
    SELECT f.id, f.artifact_id, a.title AS artifact_title, a.org,
           f.body, f.viewer_email, f.author_source, f.external_author_id,
           f.external_author_display, f.created_at, f.parent_id,
           (f.resolved_at IS NOT NULL) AS resolved,
           (f.anchor_x IS NOT NULL AND f.anchor_y IS NOT NULL) AS has_anchor,
           (f.created_at > COALESCE(r.seen_at, @epoch)) AS unread
    FROM feedback f
    JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
    LEFT JOIN notification_reads r ON r.viewer_email = @email
    WHERE a.org = @org AND (f.viewer_email IS NULL OR f.viewer_email <> @email)
    ORDER BY f.created_at DESC, f.id DESC
    LIMIT @limit
  `);
  const countAdmin = database.prepare(`
    SELECT COUNT(*) AS count
    FROM feedback f
    JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
    LEFT JOIN notification_reads r ON r.viewer_email = @email
    WHERE (f.viewer_email IS NULL OR f.viewer_email <> @email)
      AND f.created_at > COALESCE(r.seen_at, @epoch)
  `);
  const countMember = database.prepare(`
    SELECT COUNT(*) AS count
    FROM feedback f
    JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
    LEFT JOIN notification_reads r ON r.viewer_email = @email
    WHERE a.org = @org AND (f.viewer_email IS NULL OR f.viewer_email <> @email)
      AND f.created_at > COALESCE(r.seen_at, @epoch)
  `);
  const markSeenStmt = database.prepare(`
    INSERT INTO notification_reads (viewer_email, seen_at) VALUES (?, datetime('now'))
    ON CONFLICT(viewer_email) DO UPDATE
    SET seen_at = MAX(notification_reads.seen_at, excluded.seen_at)
  `);

  function params(viewer, limit) {
    return {
      email: String(viewer.email || ""),
      org: String(viewer.org || ""),
      epoch: EPOCH,
      limit: Math.max(1, Math.min(100, Number(limit) || 30))
    };
  }

  function recentForViewer({ email, org, isAdmin, limit = 30 }) {
    const values = params({ email, org }, limit);
    const rows = isAdmin ? recentAdmin.all(values) : recentMember.all(values);
    return rows.map(({ viewer_email, author_source, external_author_id, external_author_display, ...row }) => ({
      ...row,
      author: author_source === "discord"
        ? { source: "discord", external_author_id, external_author_display }
        : { source: "artifact", viewer_email }
    }));
  }

  function unreadCount(viewer) {
    const values = params(viewer, 30);
    return Number((viewer.isAdmin ? countAdmin : countMember).get(values).count || 0);
  }

  function markSeen(email) {
    markSeenStmt.run(String(email || ""));
  }

  return { recentForViewer, unreadCount, markSeen };
}

const notifications = createNotificationStore({ db });

export const { recentForViewer, unreadCount, markSeen } = notifications;
