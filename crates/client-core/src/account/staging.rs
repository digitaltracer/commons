//! Files waiting in a composer. Each starts uploading as soon as it is staged, straight to the
//! server rather than through the outbox, and all of them go out with the composer's next
//! message. Staged files live in memory: a restart drops them, and the server keeps their
//! unsent uploads, as it does for any upload that is never sent.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use proto::model::Entity;
use proto::view::{StagedFile, StagedState};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Account, AccountEvent, SyncError};
use crate::transport::{Progress, TransportError};

/// The server's limit on files per message.
pub const MAX_FILES: usize = 10;

/// A composer: its conversation, and the thread root it replies in as the UI keys it.
pub type DraftKey = (Uuid, Option<Uuid>);

pub(super) type Lists = HashMap<DraftKey, Vec<Item>>;

pub(super) struct Item {
    view: StagedFile,
    path: PathBuf,
    /// A copy the core made of pasted bytes, deleted once the file is sent or removed.
    temp: bool,
    cancel: CancellationToken,
    /// Bytes sent at the last progress event.
    shown: u64,
}

/// What a failed upload says on its chip.
fn upload_error(e: &SyncError) -> String {
    match e {
        SyncError::Transport(TransportError::Status { body, .. }) => body
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("The server refused the file")
            .to_string(),
        SyncError::Transport(TransportError::Network(_)) => {
            "Couldn't reach the server. Retry when you're back online.".into()
        }
        SyncError::SignedOut => "Signed out. Sign in again, then retry.".into(),
        other => other.to_string(),
    }
}

/// The last component of a pasted file's name, so it can't point outside its folder.
fn file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." {
        "pasted file".into()
    } else {
        base.to_string()
    }
}

impl Account {
    /// The files waiting in a composer, in the order they were added.
    pub fn staged(&self, key: DraftKey) -> Vec<StagedFile> {
        self.staging
            .lock()
            .map(|l| {
                l.get(&key)
                    .map(|v| v.iter().map(|i| i.view.clone()).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Stages a file on disk and starts uploading it. The file itself is never changed.
    pub fn stage_file(&self, key: DraftKey, path: &Path) -> anyhow::Result<Uuid> {
        self.stage(key, path.to_path_buf(), false)
    }

    /// Stages pasted bytes: they are copied into the account's folder first, so the upload
    /// can be retried.
    pub fn stage_bytes(&self, key: DraftKey, name: &str, bytes: &[u8]) -> anyhow::Result<Uuid> {
        let dir = self.staging_dir.join(Uuid::now_v7().to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file_name(name));
        let staged = std::fs::write(&path, bytes)
            .map_err(anyhow::Error::from)
            .and_then(|()| self.stage(key, path, true));
        if staged.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        staged
    }

    fn stage(&self, key: DraftKey, path: PathBuf, temp: bool) -> anyhow::Result<Uuid> {
        let meta = std::fs::metadata(&path)?;
        if meta.is_dir() {
            anyhow::bail!("Folders can't be attached");
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("not a file: {}", path.display()))?;
        let id = Uuid::now_v7();
        {
            let mut lists = self
                .staging
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let list = lists.entry(key).or_default();
            if list.len() >= MAX_FILES {
                anyhow::bail!("A message carries at most {MAX_FILES} files");
            }
            list.push(Item {
                view: StagedFile {
                    id,
                    name,
                    size: meta.len(),
                    sent: 0,
                    state: StagedState::Uploading,
                },
                path,
                temp,
                cancel: self.cancel.child_token(),
                shown: 0,
            });
        }
        self.emit_staged(key);
        self.start_upload(key, id);
        Ok(id)
    }

    fn with_item<T>(&self, key: DraftKey, id: Uuid, f: impl FnOnce(&mut Item) -> T) -> Option<T> {
        let mut lists = self.staging.lock().ok()?;
        lists
            .get_mut(&key)?
            .iter_mut()
            .find(|i| i.view.id == id)
            .map(f)
    }

    fn start_upload(&self, key: DraftKey, id: Uuid) {
        let Some((path, cancel)) = self.with_item(key, id, |i| (i.path.clone(), i.cancel.clone()))
        else {
            return;
        };
        let account = self.clone();
        tokio::spawn(async move {
            let reporter = account.clone();
            let progress: Progress = Arc::new(move |sent| reporter.staged_progress(key, id, sent));
            let result = tokio::select! {
                r = account.upload_file_with(key.0, &path, progress) => r,
                () = cancel.cancelled() => return,
            };
            account.staged_done(key, id, result.map(|f| f.id).map_err(|e| upload_error(&e)));
        });
    }

    /// Progress events come at most every 2% (and every 64 KiB at least), not per chunk.
    fn staged_progress(&self, key: DraftKey, id: Uuid, sent: u64) {
        let show = self.with_item(key, id, |i| {
            i.view.sent = sent.min(i.view.size);
            let step = (i.view.size / 50).max(64 * 1024);
            let show = matches!(i.view.state, StagedState::Uploading)
                && (i.view.sent >= i.shown + step || i.view.sent == i.view.size);
            if show {
                i.shown = i.view.sent;
            }
            show
        });
        if show == Some(true) {
            self.emit_staged(key);
        }
    }

    fn staged_done(&self, key: DraftKey, id: Uuid, result: Result<Uuid, String>) {
        let found = self.with_item(key, id, |i| {
            i.view.state = match result {
                Ok(file_id) => {
                    i.view.sent = i.view.size;
                    StagedState::Ready { file_id }
                }
                Err(error) => StagedState::Failed { error },
            };
        });
        if found.is_some() {
            self.emit_staged(key);
        }
    }

    /// Uploads a failed file again.
    pub fn retry_staged(&self, key: DraftKey, id: Uuid) {
        let child = self.cancel.child_token();
        let retry = self.with_item(key, id, |i| {
            let failed = matches!(i.view.state, StagedState::Failed { .. });
            if failed {
                i.view.state = StagedState::Uploading;
                i.view.sent = 0;
                i.shown = 0;
                i.cancel = child;
            }
            failed
        });
        if retry == Some(true) {
            self.emit_staged(key);
            self.start_upload(key, id);
        }
    }

    /// Takes a file out of the composer, stopping its upload if it is still running.
    pub fn remove_staged(&self, key: DraftKey, id: Uuid) {
        let removed = self.staging.lock().ok().and_then(|mut lists| {
            let list = lists.get_mut(&key)?;
            let at = list.iter().position(|i| i.view.id == id)?;
            let item = list.remove(at);
            if list.is_empty() {
                lists.remove(&key);
            }
            Some(item)
        });
        if let Some(item) = removed {
            item.cancel.cancel();
            self.drop_copy(&item);
            self.emit_staged(key);
        }
    }

    /// Queues `body` with every file staged in the composer, then clears them. Refused while
    /// a file is still uploading or has failed, so nothing is dropped without the user
    /// seeing it. Files that finished uploading go out even offline, through the outbox.
    pub fn send_with_staged(
        &self,
        key: DraftKey,
        body: String,
        thread_root_id: Option<Uuid>,
        picked: Vec<Entity>,
    ) -> anyhow::Result<Uuid> {
        let mut lists = self
            .staging
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        let list = lists.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        if list
            .iter()
            .any(|i| matches!(i.view.state, StagedState::Uploading))
        {
            anyhow::bail!("Wait for the files to finish uploading");
        }
        if list
            .iter()
            .any(|i| matches!(i.view.state, StagedState::Failed { .. }))
        {
            anyhow::bail!("Remove or retry the file that didn't upload");
        }
        let files: Vec<Uuid> = list
            .iter()
            .filter_map(|i| match i.view.state {
                StagedState::Ready { file_id } => Some(file_id),
                _ => None,
            })
            .collect();
        if body.trim().is_empty() && files.is_empty() {
            anyhow::bail!("Nothing to send");
        }
        let id = self.send_message_with(key.0, body, thread_root_id, files, picked)?;
        let sent = lists.remove(&key).unwrap_or_default();
        drop(lists);
        if !sent.is_empty() {
            for item in &sent {
                self.drop_copy(item);
            }
            self.emit_staged(key);
        }
        Ok(id)
    }

    /// Deletes the core's own copy of a pasted file.
    fn drop_copy(&self, item: &Item) {
        if item.temp
            && let Some(dir) = item.path.parent()
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Removes copies of pasted files left by an earlier run.
    pub fn clear_staging_dir(&self) {
        let _ = std::fs::remove_dir_all(&self.staging_dir);
    }

    fn emit_staged(&self, key: DraftKey) {
        (self.events)(
            self.id,
            AccountEvent::Staged {
                conversation_id: key.0,
                thread_root_id: key.1,
                files: self.staged(key),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_names_stay_in_their_folder() {
        assert_eq!(file_name("shot.png"), "shot.png");
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("a\\b.txt"), "b.txt");
        assert_eq!(file_name(".."), "pasted file");
        assert_eq!(file_name("  "), "pasted file");
    }
}
