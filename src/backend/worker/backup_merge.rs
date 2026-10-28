use super::{Event, Worker};

impl Worker {
    pub(super) async fn prepare_backup_merge(
        &mut self,
        passphrase: crate::archive::backup::BackupPassphrase,
    ) {
        self.pending_backup_merge = None;
        let selected = tokio::task::spawn_blocking(|| {
            rfd::FileDialog::new()
                .add_filter("Encrypted Zap Fasterk account", &["zfbackup"])
                .pick_file()
        })
        .await;
        let path = match selected {
            Ok(Some(path)) => path,
            Ok(None) => {
                self.emit(Event::BackupMergePreview(Ok(None)));
                return;
            }
            Err(_) => {
                self.emit(Event::BackupMergePreview(Err(
                    "Could not open the backup file picker".into(),
                )));
                return;
            }
        };
        let parent = self.dirs.state.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            crate::archive::backup::prepare_restore(&path, passphrase.expose(), &parent)
        })
        .await;
        let result = match prepared {
            Ok(Ok(prepared)) => prepared
                .preview_merge(&self.archive)
                .map_err(|error| error.to_string()),
            _ => Err(
                "Could not authenticate and prepare this backup. Check its passphrase and file."
                    .into(),
            ),
        };
        match result {
            Ok(prepared) => {
                self.emit(Event::BackupMergePreview(Ok(Some(
                    prepared.preview.clone(),
                ))));
                self.pending_backup_merge = Some(prepared);
            }
            Err(error) => self.emit(Event::BackupMergePreview(Err(error))),
        }
    }

    pub(super) fn confirm_backup_merge(&mut self) {
        let result = self
            .pending_backup_merge
            .take()
            .ok_or_else(|| anyhow::anyhow!("Prepare a merge preview first."))
            .and_then(|prepared| prepared.apply(&self.archive, &self.dirs.media_cache_dir()));
        match result {
            Ok(preview) => {
                self.load_state();
                self.emit(Event::BackupMergeFinished(Ok(preview)));
            }
            Err(error) => self.emit(Event::BackupMergeFinished(Err(error.to_string()))),
        }
    }
}
