//! Account media disk usage, without reading file contents or following links.

use std::path::Path;

use super::{Archive, MediaJob, Result};

/// Keep the daily sweep bounded. Large archives are retained until a paged
/// reference index can establish ownership without an unbounded snapshot.
pub(crate) const RETENTION_REFERENCE_LIMIT: usize = 20_000;

pub(crate) struct CacheReferences {
    pub paths: Vec<std::path::PathBuf>,
    pub jobs: Vec<MediaJob>,
    pub export_roots: Vec<std::path::PathBuf>,
}

impl Archive {
    /// Metadata only. Every path is protected, including locked chats and paths
    /// in future content families. A truncated snapshot must never authorize
    /// deletion. Jobs also protect crash-published files not yet referenced.
    pub(crate) fn cache_retention_references(&self) -> Result<Option<CacheReferences>> {
        let limit = (RETENTION_REFERENCE_LIMIT + 1) as i64;
        let messages: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM messages LIMIT ?1)",
            [limit],
            |row| row.get(0),
        )?;
        if messages > RETENTION_REFERENCE_LIMIT as i64 {
            return Ok(None);
        }
        for table in ["stickers", "managed_exports", "media_jobs"] {
            let count: i64 = self.connection.query_row(
                &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} LIMIT ?1)"),
                [limit],
                |row| row.get(0),
            )?;
            if count > RETENTION_REFERENCE_LIMIT as i64 {
                return Ok(None);
            }
        }
        let paths: Vec<std::path::PathBuf> = self
            .connection
            .prepare(
                "SELECT path FROM (
                 SELECT DISTINCT tree.value AS path FROM messages m,json_tree(m.content) tree
                 WHERE tree.key='path' AND tree.type='text'
                 UNION SELECT path FROM stickers WHERE path IS NOT NULL
                 UNION SELECT json_extract(destination,'$') FROM managed_exports
                 ) LIMIT ?1",
            )?
            .query_map([limit], |row| {
                Ok(std::path::PathBuf::from(row.get::<_, String>(0)?))
            })?
            .collect::<Result<_>>()?;
        if paths.len() > RETENTION_REFERENCE_LIMIT {
            return Ok(None);
        }
        let jobs: Vec<MediaJob> = self
            .connection
            .prepare("SELECT chat,id,card FROM media_jobs LIMIT ?1")?
            .query_map([limit], |row| {
                Ok(MediaJob {
                    chat: row.get(0)?,
                    id: row.get(1)?,
                    card: row.get(2)?,
                })
            })?
            .collect::<Result<_>>()?;
        if jobs.len() > RETENTION_REFERENCE_LIMIT {
            return Ok(None);
        }
        // A previous user-selected folder remains an export folder after the
        // setting changes. Never reinterpret it as disposable managed cache.
        let export_roots = self
            .connection
            .prepare("SELECT DISTINCT json_extract(root,'$') FROM managed_exports LIMIT ?1")?
            .query_map([limit], |row| {
                Ok(std::path::PathBuf::from(row.get::<_, String>(0)?))
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(CacheReferences {
            paths,
            jobs,
            export_roots,
        }))
    }
}

pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions and other reparse points can redirect directory traversal
        // too, although they are not necessarily reported as symbolic links.
        metadata.file_attributes() & 0x400 != 0
    }
}

/// Measure the managed media folder, including staging and orphaned files.
/// Fail closed on unreadable entries so recovery cannot bypass a storage limit.
pub fn managed_media_bytes(root: &Path) -> std::io::Result<u64> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if is_link(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Managed media folder cannot be a symbolic link",
        ));
    }
    let mut folders = vec![root.to_path_buf()];
    let mut total = 0u64;
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder)? {
            let entry = entry?;
            let metadata = entry.path().symlink_metadata()?;
            if is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                folders.push(entry.path());
            } else if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| std::io::Error::other("Managed media size overflow"))?;
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_actual_nested_files_and_unreferenced_staging() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(managed_media_bytes(&root.path().join("absent")).unwrap(), 0);
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/file"), b"1234").unwrap();
        std::fs::write(root.path().join(".staging"), b"123").unwrap();
        assert_eq!(managed_media_bytes(root.path()).unwrap(), 7);
        assert!(root.path().join("nested/file").exists());
    }

    #[test]
    fn retention_protects_locked_nested_paths_stickers_exports_and_all_job_states() {
        let archive = Archive::in_memory().unwrap();
        archive.ensure_chat("fixture", "Synthetic").unwrap();
        archive
            .connection
            .execute("UPDATE chats SET locked=1", [])
            .unwrap();
        archive.connection.execute(
            "INSERT INTO messages(chat,id,sender,from_me,timestamp,content,raw)
             VALUES('fixture','fixture','fixture',0,1,?1,X'00')",
            [r#"{"kind":"future","nested":{"path":"/cache/locked-image.jpg"},"cards":[{"path":"/cache/card-image.jpg"}]}"#],
        ).unwrap();
        archive.connection.execute(
            "INSERT INTO stickers(hash,raw,path) VALUES('fixture',X'00','/cache/saved-sticker.webp')", []
        ).unwrap();
        archive.connection.execute(
            "INSERT INTO managed_exports(chat,id,card,root,destination) VALUES('fixture','fixture',-1,'\"/exports\"','\"/exports/saved-export.jpg\"')", []
        ).unwrap();
        for state in 0..4 {
            archive
                .connection
                .execute(
                    "INSERT INTO media_jobs(chat,id,card,state) VALUES('fixture',?1,-1,?2)",
                    rusqlite::params![format!("job-{state}"), state],
                )
                .unwrap();
        }
        let references = archive.cache_retention_references().unwrap().unwrap();
        assert_eq!(
            references.export_roots,
            vec![std::path::PathBuf::from("/exports")]
        );
        let paths: std::collections::HashSet<_> = references.paths.into_iter().collect();
        for expected in [
            "/cache/locked-image.jpg",
            "/cache/card-image.jpg",
            "/cache/saved-sticker.webp",
            "/exports/saved-export.jpg",
        ] {
            assert!(paths.contains(Path::new(expected)));
        }
        assert_eq!(references.jobs.len(), 4);
    }

    #[test]
    fn retention_refuses_a_truncated_reference_snapshot() {
        let archive = Archive::in_memory().unwrap();
        archive
            .connection
            .execute(
                "WITH RECURSIVE ids(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM ids WHERE n<?1)
             INSERT INTO media_jobs(chat,id,card) SELECT 'fixture',CAST(n AS TEXT),-1 FROM ids",
                [RETENTION_REFERENCE_LIMIT as i64],
            )
            .unwrap();
        assert!(archive.cache_retention_references().unwrap().is_none());
    }

    #[test]
    fn retention_limits_stickers_and_exports_before_collecting_paths() {
        for insert in [
            "INSERT INTO stickers(hash,raw,path) SELECT CAST(n AS TEXT),X'00','/cache/fixture.jpg' FROM ids",
            "INSERT INTO managed_exports(chat,id,card,root,destination) SELECT 'fixture',CAST(n AS TEXT),-1,'\"/exports\"','\"/exports/fixture.jpg\"' FROM ids",
        ] {
            let archive = Archive::in_memory().unwrap();
            archive.connection.execute(
                &format!("WITH RECURSIVE ids(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM ids WHERE n<?1) {insert}"),
                [RETENTION_REFERENCE_LIMIT as i64],
            ).unwrap();
            assert!(archive.cache_retention_references().unwrap().is_none());
        }
    }
}
