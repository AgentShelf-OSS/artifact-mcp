# Feature inventory & browser-test coverage

Every user-reachable feature, and whether the Playwright harness exercises it. Derived from the 40
HTTP routes in `lib/app.js` and the 19 frozen MCP tools.

Status: **✅ covered** · **🔸 partial** · **⬜ not covered**

## 1. Gallery / index
| Feature | Status | Notes |
|---|---|---|
| Renders artifacts for the viewer's org | ✅ | |
| Artifact cards show title / description | ✅ | |
| Category grouping & filter chips | ✅ | |
| Favorites filter | ✅ | |
| Thumbnails (`GET /thumbnails/:id`) | 🔸 | renderer disabled in harness; placeholder path only |
| View counts / analytics surface | 🔸 | asserted present, not value-checked |
| Notification badge + `POST /notifications/seen` | ✅ | |
| Org switcher (admin sees all orgs) | ✅ | |

## 2. Artifact viewer shell
| Feature | Status | Notes |
|---|---|---|
| Shell renders with sandboxed iframe | ✅ | asserts `sandbox` present, no `allow-same-origin` |
| Prev / next navigation | ✅ | |
| Download button | ✅ | |
| Open raw (`/raw/:id`) | ✅ | asserts CSP sandbox header |
| Bundle artifact + subpath (`/raw/:id/*`) | ✅ | |
| Revision history panel (`/:id/history`) | ✅ | |
| Restore a revision (`POST /:id/restore`) | ✅ | |
| View a historical revision (`/raw/:id/rev/:n`) | ✅ | |

## 3. Reactions
| Feature | Status | Notes |
|---|---|---|
| Favorite toggle (heart) | ✅ | click must fire request AND flip `aria-pressed` |
| Upvote / downvote | ✅ | |
| Reaction persists across reload | ✅ | |

## 4. Feedback
| Feature | Status | Notes |
|---|---|---|
| Add comment (`POST /:id/feedback`) | ✅ | |
| List comments (`GET /:id/feedback`) | ✅ | |
| Threaded reply (parent) | ✅ | |
| Resolve / reopen | ✅ | |
| Delete comment | ✅ | |
| Anchored feedback (coords on a bundle page) | ⬜ | needs iframe bridge interaction |

## 5. Sharing
| Feature | Status | Notes |
|---|---|---|
| Create share (`never` / `24h` / date) | ✅ | |
| List shares | ✅ | |
| Revoke share | ✅ | |
| Public share page loads without auth (`/s/:token`) | ✅ | |
| Revoked / invalid token → indistinguishable 404 | ✅ | |

## 6. Categories
| Feature | Status | Notes |
|---|---|---|
| Assign category to artifact | ✅ | |
| Assigned category appears in Settings | ✅ | regression guard for the registration bug |
| Create category in Settings | ✅ | |
| Delete category in Settings | ✅ | |

## 7. Visibility & tenancy
| Feature | Status | Notes |
|---|---|---|
| Hide / unhide (`POST /:id/visibility`) | ✅ | |
| Move artifact between orgs (admin) | ✅ | |
| Cross-org artifact is concealed as 404 | ✅ | invariant 3 |
| Signed-out viewer sees the sign-in page | ✅ | |
| Non-admin cannot reach Settings | ✅ | |

## 8. Settings / administration
| Feature | Status | Notes |
|---|---|---|
| Create / delete organization | ✅ | |
| Add / remove domain | ✅ | |
| Add / remove explicit email member | ✅ | |
| Set org colour | ✅ | |
| Create publisher key (secret shown once) | ✅ | |
| Revoke publisher key | ✅ | |
| Add / delete webhook | ✅ | |
| Send test webhook | ⬜ | would post to a real Discord endpoint |

## 9. MCP surface (19 tools)
Covered byte-for-byte by `conformance/` (`--impl both`, 23 cases incl. the frozen `tools/list`
golden), not re-tested here. The harness uses `publish_artifact` / `publish_bundle` only as fixture
setup.

## Deliberately out of scope
- **Live production instance** — mutating tests would create and delete real artifacts. The `node`
  project runs prod's exact code against throwaway data instead; a separate read-only smoke checks
  the live instance.
- **Send-test-webhook** — posts to a real external endpoint.
- **Anchored feedback** — depends on the artifact-iframe bridge; needs a dedicated fixture.
