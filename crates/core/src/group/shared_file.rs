//! `group.json` as several programs see it: this app, and a C# Sender or Receiver that may be running next to
//! it. Loads the file again when somebody else changed it (checked cheaply by write time and size).

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::config::GroupConfig;
use crate::common::app_paths;

/// What the file looked like when it was last read or written; `None` = it did not exist.
type Stamp = Option<(SystemTime, u64)>;

#[derive(Debug)]
pub struct SharedGroupFile {
    path: PathBuf,
    stamp: Stamp,
    current: GroupConfig,
}

impl SharedGroupFile {
    /// Loads `path` (a missing or broken file is an empty, normalized config).
    pub fn new(path: impl Into<PathBuf>) -> SharedGroupFile {
        let path = path.into();
        let stamp = stamp_of(&path);
        let current = GroupConfig::load(&path);
        SharedGroupFile { path, stamp, current }
    }

    /// The file in the settings folder ([`GroupConfig::default_path`]).
    pub fn open_default() -> SharedGroupFile {
        SharedGroupFile::new(GroupConfig::default_path())
    }

    pub fn current(&self) -> &GroupConfig {
        &self.current
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the file changed on disk since the last load (and [`current`](Self::current) was refreshed).
    pub fn refresh_if_changed(&mut self) -> bool {
        let stamp = stamp_of(&self.path);
        if stamp == self.stamp {
            return false;
        }
        self.reload();
        true
    }

    /// Normalizes `config`, writes it and makes it [`current`](Self::current). On an error nothing changes.
    pub fn save(&mut self, config: GroupConfig) -> io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            app_paths::ensure_dir(dir);
        }
        let config = config.normalize();
        config.save(&self.path)?;
        self.current = config;
        self.stamp = stamp_of(&self.path);
        Ok(())
    }

    fn reload(&mut self) {
        // The stamp first: a change that lands between the two steps is then seen by the next check.
        self.stamp = stamp_of(&self.path);
        self.current = GroupConfig::load(&self.path);
    }
}

fn stamp_of(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::invite::Invite;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hdrcast-shared-{tag}-{}", std::process::id())).join("group.json")
    }

    #[test]
    fn a_missing_file_is_an_empty_config_and_saving_creates_it() {
        let path = temp_file("create");
        let mut file = SharedGroupFile::new(&path);
        assert!(!file.current().is_configured());
        assert!(!file.refresh_if_changed(), "still missing: nothing changed");
        let peer = file.current().peer_id.clone();

        let joined = file.current().adopt(&Invite::create("Friends", None).unwrap(), "me");
        file.save(joined.clone()).unwrap();
        assert_eq!(file.current(), &joined);
        assert_eq!(file.current().peer_id, peer);
        assert!(!file.refresh_if_changed(), "our own save is not somebody else's change");
        assert_eq!(GroupConfig::load(&path), joined);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_change_made_by_another_program_is_picked_up_once() {
        let path = temp_file("refresh");
        let mut ours = SharedGroupFile::new(&path);
        let mut theirs = SharedGroupFile::new(&path);
        let first = ours.current().adopt(&Invite::create("First", None).unwrap(), "me");
        ours.save(first).unwrap();

        assert!(theirs.refresh_if_changed());
        assert_eq!(theirs.current().group_name, "First");
        assert!(!theirs.refresh_if_changed());

        // a different group name of a different length: caught even on a file system with coarse timestamps
        let second = theirs.current().adopt(&Invite::create("Second group", None).unwrap(), "them");
        theirs.save(second).unwrap();
        assert!(ours.refresh_if_changed());
        assert_eq!(ours.current().group_name, "Second group");
        assert_eq!(ours.current().display_name, "them");

        // deleted behind our back: back to "no group", with a fresh but valid peer id
        std::fs::remove_file(&path).unwrap();
        assert!(ours.refresh_if_changed());
        assert!(!ours.current().is_configured());
        assert!(!ours.refresh_if_changed());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_save_changes_nothing() {
        // the "file" is a directory: the rename over it fails
        let path = temp_file("fail");
        std::fs::create_dir_all(&path).unwrap();
        let mut file = SharedGroupFile::new(&path);
        let before = file.current().clone();
        let joined = before.adopt(&Invite::create("Friends", None).unwrap(), "me");
        assert!(file.save(joined).is_err());
        assert_eq!(file.current(), &before);
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 1, "no temp file left: {left:?}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
