//! Shared artifact metadata columns and by-name SQLite row decoding.

use crate::model::{ArtifactId, ArtifactMeta, ClientId, OrgId, Timestamp};

/// Artifact metadata columns shared by lifecycle and feedback queries.
pub(crate) const META_COLUMNS: &str = "id, client_id, org, title, description, bytes, created_at, updated_at, \
     uploader_label, owner_email, is_bundle, entry, revision, category, hidden, body_sha256, \
     artifact_key";

pub(crate) fn meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactMeta> {
    Ok(ArtifactMeta {
        id: ArtifactId(row.get("id")?),
        client_id: ClientId(row.get("client_id")?),
        org: OrgId(row.get("org")?),
        title: row.get("title")?,
        description: row.get("description")?,
        bytes: nonnegative_u64(row, "bytes")?,
        created_at: Timestamp(row.get("created_at")?),
        updated_at: Timestamp(row.get("updated_at")?),
        uploader_label: row.get("uploader_label")?,
        owner_email: row.get("owner_email")?,
        is_bundle: row.get::<_, i64>("is_bundle")? != 0,
        entry: row.get("entry")?,
        revision: nonnegative_u64(row, "revision")?,
        category: row.get("category")?,
        hidden: row.get::<_, i64>("hidden")? != 0,
        body_sha256: row.get("body_sha256")?,
        artifact_key: row.get("artifact_key")?,
    })
}

fn nonnegative_u64(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<u64> {
    let index = row.as_ref().column_index(name)?;
    let value: i64 = row.get(name)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::*;

    const REORDERED_COLUMNS: &str = "SELECT ?1 AS bytes, ?2 AS revision, \
        'project/report' AS artifact_key, 'report0001' AS id, 'acme' AS org, \
        'publisher' AS client_id, 'Report' AS title, 'Description' AS description, \
        'created' AS created_at, 'updated' AS updated_at, 'Publisher' AS uploader_label, \
        'owner@example.test' AS owner_email, 1 AS is_bundle, 'report.html' AS entry, \
        'Reports' AS category, 1 AS hidden, 'digest' AS body_sha256";

    #[test]
    fn artifact_metadata_uses_column_names_and_preserves_the_key() {
        let conn = Connection::open_in_memory().expect("database");
        let meta = conn
            .query_row(REORDERED_COLUMNS, params![42, 7], meta_from_row)
            .expect("metadata with reordered columns");
        let expected = ArtifactMeta {
            title: "Report".to_owned(),
            description: "Description".to_owned(),
            bytes: 42,
            created_at: Timestamp("created".to_owned()),
            updated_at: Timestamp("updated".to_owned()),
            uploader_label: "Publisher".to_owned(),
            owner_email: Some("owner@example.test".to_owned()),
            is_bundle: true,
            entry: "report.html".to_owned(),
            revision: 7,
            category: "Reports".to_owned(),
            hidden: true,
            body_sha256: "digest".to_owned(),
            artifact_key: Some("project/report".to_owned()),
            ..ArtifactMeta::fixture("report0001", "acme")
        };
        assert_eq!(meta, expected);
    }

    #[test]
    fn negative_metadata_counters_report_the_selected_column_index() {
        let conn = Connection::open_in_memory().expect("database");
        for (bytes, revision, expected_index) in [(-1, 7, 0), (42, -1, 1)] {
            let error = conn
                .query_row(REORDERED_COLUMNS, params![bytes, revision], meta_from_row)
                .expect_err("negative metadata counter");
            assert!(matches!(
                error,
                rusqlite::Error::IntegralValueOutOfRange(index, -1) if index == expected_index
            ));
        }
    }
}
