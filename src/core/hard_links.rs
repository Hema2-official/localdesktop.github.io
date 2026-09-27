//! proot emulates hard links with symlinks (its `link2symlink` extension). The file's data moves
//! to a hidden `.proot.l2s.<name>NNNN.CCCC` file (`CCCC` is the link count), an intermediate
//! `.proot.l2s.<name>NNNN` symlink points at it, and every name of the file is a symlink to that
//! intermediate. All of them hold absolute host paths.
//!
//! With `PROOT_L2S_DIR`, proot keeps new data files in one store directory. Hard links created
//! before keep theirs next to the file's first name, where `-H` hides them and removing that
//! directory fails while another name still uses the data. [`migrate`] moves them into the store,
//! once per rootfs.

use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

/// The store directory, relative to the rootfs.
pub const STORE_DIR: &str = ".l2s";

const PREFIX: &str = ".proot.l2s.";
const MIGRATED_MARKER: &str = ".proot-migrated";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Migration {
    /// Data files moved into the store.
    pub moved: usize,
    /// File names repointed at the moved data.
    pub relinked: usize,
    /// Hard links that couldn't be moved, and why.
    pub failed: Vec<(PathBuf, String)>,
}

/// Move hard link data kept next to the first link into the store. Returns `None` if the rootfs
/// was already migrated.
pub fn migrate(fs_root: &Path) -> io::Result<Option<Migration>> {
    let store = fs_root.join(STORE_DIR);
    let marker = store.join(MIGRATED_MARKER);
    if marker.exists() {
        return Ok(None);
    }
    fs::create_dir_all(&store)?;

    let mut intermediates = Vec::new();
    let mut names_by_target: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut directories = vec![fs_root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if path != store {
                    directories.push(path);
                }
            } else if file_type.is_symlink() {
                let Ok(target) = fs::read_link(&path) else {
                    continue;
                };
                if !is_l2s_name(&target) {
                    continue;
                }
                if is_l2s_name(&path) {
                    if directory != store {
                        intermediates.push((path, target));
                    }
                } else {
                    names_by_target.entry(target).or_default().push(path);
                }
            }
        }
    }

    let mut migration = Migration::default();
    for (intermediate, data) in intermediates {
        let names = names_by_target.remove(&intermediate).unwrap_or_default();
        match move_into_store(&store, &intermediate, &data, &names) {
            Ok(()) => {
                migration.moved += 1;
                migration.relinked += names.len();
            }
            Err(error) => migration.failed.push((intermediate, error.to_string())),
        }
    }

    fs::write(&marker, b"")?;
    Ok(Some(migration))
}

fn move_into_store(
    store: &Path,
    intermediate: &Path,
    data: &Path,
    names: &[PathBuf],
) -> io::Result<()> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let intermediate_name = file_name(intermediate).ok_or_else(|| invalid("unnamed link"))?;
    let data_name = file_name(data).ok_or_else(|| invalid("unnamed data file"))?;
    // `.proot.l2s.<name>NNNN` and its data file `.proot.l2s.<name>NNNN.CCCC`.
    let base = intermediate_name
        .len()
        .checked_sub(4)
        .filter(|&split| intermediate_name.is_char_boundary(split))
        .map(|split| intermediate_name.split_at(split))
        .filter(|(_, counter)| counter.bytes().all(|b| b.is_ascii_digit()))
        .map(|(base, _)| base)
        .ok_or_else(|| invalid("unexpected link name"))?;
    let count = data_name
        .strip_prefix(intermediate_name)
        .and_then(|rest| rest.strip_prefix('.'))
        .ok_or_else(|| invalid("unexpected data file name"))?;
    if !fs::symlink_metadata(data)?.is_file() {
        return Err(invalid("data file is not a regular file"));
    }

    let new_intermediate = (1..10_000)
        .map(|n| store.join(format!("{base}{n:04}")))
        .find(|path| fs::symlink_metadata(path).is_err())
        .ok_or_else(|| invalid("no free name in the store"))?;
    let new_data = PathBuf::from(format!("{}.{count}", new_intermediate.display()));

    fs::rename(data, &new_data)?;
    // proot's ownership record for the data file travels with it.
    let record = |file: &Path| {
        file.with_file_name(format!(
            ".proot-meta-file.{}.meta",
            file.file_name().unwrap_or_default().to_string_lossy()
        ))
    };
    if fs::symlink_metadata(record(data)).is_ok() {
        fs::rename(record(data), record(&new_data))?;
    }
    symlink(&new_data, &new_intermediate)?;
    for name in names {
        let temporary = name.with_file_name(format!("{PREFIX}migrating"));
        let _ = fs::remove_file(&temporary);
        symlink(&new_intermediate, &temporary)?;
        fs::rename(&temporary, name)?;
    }
    fs::remove_file(intermediate)
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn is_l2s_name(path: &Path) -> bool {
    file_name(path).is_some_and(|name| name.starts_with(PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Lay out a hard link the way proot does without `PROOT_L2S_DIR`: data next to the first
    /// name, both names pointing at the intermediate.
    fn old_style_link(root: &Path, first: &str, second: &str, content: &str) -> PathBuf {
        let first = root.join(first);
        let second = root.join(second);
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        let name = first.file_name().unwrap().to_str().unwrap();
        let intermediate = first.with_file_name(format!("{PREFIX}{name}0001"));
        let data = PathBuf::from(format!("{}.0002", intermediate.display()));
        fs::write(&data, content).unwrap();
        let record = data.with_file_name(format!(
            ".proot-meta-file.{}.meta",
            data.file_name().unwrap().to_str().unwrap()
        ));
        fs::write(&record, "record").unwrap();
        symlink(&data, &intermediate).unwrap();
        symlink(&intermediate, &first).unwrap();
        symlink(&intermediate, &second).unwrap();
        intermediate
    }

    fn l2s_entries(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(".proot"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn moves_old_links_into_the_store() {
        let root = tempdir().unwrap();
        let root = root.path();
        old_style_link(root, "a/f", "b/f", "shared");

        let migration = migrate(root).unwrap().unwrap();

        assert_eq!(migration.moved, 1);
        assert_eq!(migration.relinked, 2);
        assert!(migration.failed.is_empty());
        assert!(l2s_entries(&root.join("a")).is_empty());
        assert_eq!(fs::read_to_string(root.join("a/f")).unwrap(), "shared");
        assert_eq!(fs::read_to_string(root.join("b/f")).unwrap(), "shared");
        assert_eq!(
            l2s_entries(&root.join(STORE_DIR)),
            [
                ".proot-meta-file..proot.l2s.f0001.0002.meta",
                ".proot-migrated",
                ".proot.l2s.f0001",
                ".proot.l2s.f0001.0002",
            ]
        );
        // The folder of the first name can now go while the other name keeps the data.
        fs::remove_file(root.join("a/f")).unwrap();
        fs::remove_dir(root.join("a")).unwrap();
        assert_eq!(fs::read_to_string(root.join("b/f")).unwrap(), "shared");
    }

    #[test]
    fn picks_a_free_name_in_the_store() {
        let root = tempdir().unwrap();
        let root = root.path();
        old_style_link(root, "a/f", "b/f", "first");
        old_style_link(root, "c/f", "d/f", "second");

        let migration = migrate(root).unwrap().unwrap();

        assert_eq!(migration.moved, 2);
        assert_eq!(fs::read_to_string(root.join("b/f")).unwrap(), "first");
        assert_eq!(fs::read_to_string(root.join("d/f")).unwrap(), "second");
        assert!(root.join(STORE_DIR).join(".proot.l2s.f0002").exists());
    }

    #[test]
    fn runs_once() {
        let root = tempdir().unwrap();
        let root = root.path();
        old_style_link(root, "a/f", "b/f", "data");

        assert!(migrate(root).unwrap().is_some());
        old_style_link(root, "c/g", "d/g", "later");
        assert_eq!(migrate(root).unwrap(), None);
        assert_eq!(l2s_entries(&root.join("c")).len(), 3);
    }

    #[test]
    fn leaves_links_already_in_the_store_alone() {
        let root = tempdir().unwrap();
        let root = root.path();
        let store = root.join(STORE_DIR);
        fs::create_dir_all(&store).unwrap();
        let data = store.join(".proot.l2s.f0001.0002");
        fs::write(&data, "new style").unwrap();
        symlink(&data, store.join(".proot.l2s.f0001")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        symlink(store.join(".proot.l2s.f0001"), root.join("a/f")).unwrap();

        assert_eq!(migrate(root).unwrap().unwrap(), Migration::default());
        assert_eq!(fs::read_to_string(root.join("a/f")).unwrap(), "new style");
    }
}
