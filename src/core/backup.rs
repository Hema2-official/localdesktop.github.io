//! Backups of the Linux system: a standard `.tar.zst`, as `tar` run inside the system would make
//! it, which any of the Local Desktop apps restores.
//!
//! proot keeps what Linux programs see of a file's owner and mode in a record beside it
//! (`.proot-meta-file.<name>.meta`: the mode in octal digits, the owner and the group, a line
//! each), and hard links as symlinks into a store ([`hard_links`]), both in the app's own paths.
//! A backup puts the owners, modes and hard links in the archive's headers instead, and a restore
//! lays them out again for the app that restores it. proot shows a file without a record as owned
//! by whoever looks at it, with its real mode; the archive marks those
//! (`SCHILY.xattr.user.localdesktop.record=none`) so that they come back without one. Symlinks
//! need no records. The archive starts with a description of itself
//! ([`Manifest`]); a restore takes archives without one too (`tar` of a Linux system), and `.tar`,
//! `.tar.gz` and `.tar.xz` as well.

use crate::core::hard_links;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The archive's first entry.
pub const MANIFEST: &str = ".localdesktop-backup.json";
const FORMAT: u32 = 1;
/// The PAX key that marks an entry proot kept no record for: an extended attribute of the user's
/// namespace, which `tar` takes without a warning (and `bsdtar` run as root sets on the file).
const RECORD_KEY: &str = "SCHILY.xattr.user.localdesktop.record";
const RECORD_TAG: &str = ".proot-meta-file.";
const RECORD_SUFFIX: &str = ".meta";
/// Folders whose contents only live while the system runs, or come from proot: the folders are
/// kept, their contents aren't.
const RUNTIME: &[&str] = &["proc", "sys", "dev", "tmp", "run"];
/// The modes a restore gives those folders when the archive has none of them: proot puts its own
/// over some, and setup and programs write into the others.
const RUNTIME_MODES: &[(&str, u32)] = &[
    ("proc", 0o700),
    ("sys", 0o700),
    ("dev", 0o755),
    ("tmp", 0o1777),
    ("run", 0o755),
];
/// pacman's downloads, which the mirrors have too.
pub const PACKAGE_CACHE: &str = "var/cache/pacman/pkg";
/// How hard zstd works: its default, which keeps up with the phone's storage.
const LEVEL: i32 = 3;

/// What a backup holds, so that a restore can say before it starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Manifest {
    pub format: u32,
    /// When it was made, in seconds since 1970.
    pub created: u64,
    /// The app it came from (its package) and that app's version.
    pub app: String,
    pub version: String,
    pub desktop: String,
    pub user: String,
    /// Entries other than folders, and the bytes of the files' contents.
    pub files: u64,
    pub bytes: u64,
    pub package_cache: bool,
}

/// How far a backup or a restore got.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
}

/// What a restore laid out.
#[derive(Debug, Default)]
pub struct Restored {
    /// The backup's description; `None` for an archive made elsewhere.
    pub manifest: Option<Manifest>,
    pub progress: Progress,
    /// Entries a restore doesn't make (device files, paths out of the system).
    pub left_out: u64,
}

/// A file's owner, group and mode as Linux programs see them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Owner {
    mode: u32,
    uid: u32,
    gid: u32,
    /// Whether proot keeps a record for it.
    recorded: bool,
}

fn record_path(path: &Path) -> Option<PathBuf> {
    let mut name = OsString::from(RECORD_TAG);
    name.push(path.file_name()?);
    name.push(RECORD_SUFFIX);
    Some(path.with_file_name(name))
}

/// The mode, owner and group in a file's record.
fn read_record(path: &Path) -> Option<(u32, u32, u32)> {
    let text = fs::read_to_string(record_path(path)?).ok()?;
    let mut fields = text.split_ascii_whitespace();
    let mode = u32::from_str_radix(fields.next()?, 8).ok()?;
    let uid = fields.next()?.parse().ok()?;
    let gid = fields.next()?.parse().ok()?;
    Some((mode, uid, gid))
}

fn write_record(path: &Path, owner: &Owner) -> io::Result<()> {
    let record = record_path(path).ok_or_else(|| invalid("a record for a nameless file"))?;
    let text = format!("{:o}\n{}\n{}\n", owner.mode & 0o7777, owner.uid, owner.gid);
    fs::write(record, text)
}

/// What programs see of `path`: proot puts the record's mode over the real one, but for the
/// real setuid, setgid and sticky bits. Without a record, the owner is whoever looks; the
/// archive gives such entries the owner of their folder.
fn owner_of(path: &Path, metadata: &fs::Metadata, folder: &Owner) -> Owner {
    let real = metadata.mode() & 0o7777;
    match read_record(path) {
        Some((mode, uid, gid)) => Owner {
            mode: (mode & 0o7777) | (real & 0o7000),
            uid,
            gid,
            recorded: true,
        },
        None => Owner {
            mode: real,
            uid: folder.uid,
            gid: folder.gid,
            recorded: false,
        },
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn is_internal(name: &OsStr) -> bool {
    let name = name.as_bytes();
    name.starts_with(RECORD_TAG.as_bytes()) || name.starts_with(hard_links::PREFIX.as_bytes())
}

/// Where a symlink's target is in `root`: proot's hard links name the host path of some app's
/// system (`/data/data/<package>/files/arch/…`), which may be another app's.
fn in_root(root: &Path, link: &Path, target: &Path) -> Option<PathBuf> {
    if target.is_relative() {
        return Some(link.parent()?.join(target));
    }
    if let Ok(inside) = target.strip_prefix(root) {
        return Some(root.join(inside));
    }
    let text = target.as_os_str().as_bytes();
    let marker = b"/files/arch/";
    let at = text.windows(marker.len()).position(|it| it == marker)?;
    Some(root.join(OsStr::from_bytes(&text[at + marker.len()..])))
}

/// The data behind one of proot's hard link names: the name is a symlink to an intermediate
/// symlink, which points at the data.
fn hard_link_data(root: &Path, link: &Path) -> Option<PathBuf> {
    let target = fs::read_link(link).ok()?;
    if !target
        .file_name()
        .is_some_and(|it| it.as_bytes().starts_with(hard_links::PREFIX.as_bytes()))
    {
        return None;
    }
    let intermediate = in_root(root, link, &target)?;
    let data = in_root(root, &intermediate, &fs::read_link(&intermediate).ok()?)?;
    fs::symlink_metadata(&data).ok()?.is_file().then_some(data)
}

/// Whether a folder's contents stay out of the backup.
fn leaves_out_contents(relative: &Path, package_cache: bool) -> bool {
    RUNTIME.iter().any(|it| relative == Path::new(it))
        || (!package_cache && relative == Path::new(PACKAGE_CACHE))
}

/// Every entry of the system, folders before what's in them, in a stable order.
fn walk(
    root: &Path,
    package_cache: bool,
    mut visit: impl FnMut(&Path, &Path, &fs::Metadata, &Owner) -> io::Result<Option<Owner>>,
) -> io::Result<()> {
    let top = Owner {
        mode: 0o755,
        uid: 0,
        gid: 0,
        recorded: true,
    };
    let mut folders = vec![(PathBuf::new(), top)];
    while let Some((folder, owner)) = folders.pop() {
        let mut names = folder_names(&root.join(&folder))?;
        names.sort();
        let mut inner = Vec::new();
        for name in names {
            let relative = folder.join(&name);
            if is_internal(&name) || relative == Path::new(hard_links::STORE_DIR) {
                continue;
            }
            let path = root.join(&relative);
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if let Some(owner) = visit(&path, &relative, &metadata, &owner)? {
                if metadata.is_dir() && !leaves_out_contents(&relative, package_cache) {
                    inner.push((relative, owner));
                }
            }
        }
        folders.extend(inner.into_iter().rev());
    }
    Ok(())
}

/// The names in a folder. Linux's root may list any folder, whatever its real mode: one that keeps
/// the app out gets let in.
fn folder_names(folder: &Path) -> io::Result<Vec<OsString>> {
    let read = || -> io::Result<Vec<OsString>> {
        fs::read_dir(folder)?
            .map(|entry| entry.map(|it| it.file_name()))
            .collect()
    };
    match read() {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let mode = fs::symlink_metadata(folder)?.mode();
            fs::set_permissions(folder, fs::Permissions::from_mode((mode | 0o500) & 0o7777))?;
            read()
        }
        names => names,
    }
}

/// How much a backup would hold: its entries and the bytes of their contents.
pub fn measure(root: &Path, package_cache: bool) -> io::Result<Progress> {
    let mut total = Progress::default();
    let mut shared = HashSet::new();
    walk(root, package_cache, |path, _, metadata, folder| {
        if metadata.is_dir() {
            return Ok(Some(*folder));
        }
        let file_type = metadata.file_type();
        if !(file_type.is_file() || file_type.is_symlink() || file_type.is_fifo()) {
            return Ok(None);
        }
        total.files += 1;
        if metadata.is_file() {
            total.bytes += metadata.len();
        } else if let Some(data) = hard_link_data(root, path) {
            if shared.insert(data.clone()) {
                total.bytes += fs::metadata(&data).map_or(0, |it| it.len());
            }
        }
        Ok(None)
    })?;
    Ok(total)
}

/// The names of the system's users or groups by their numbers, from `/etc/passwd` or
/// `/etc/group`.
fn names(file: &Path) -> HashMap<u32, String> {
    let Ok(file) = File::open(file) else {
        return HashMap::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            let number = fields.nth(1)?.parse().ok()?;
            Some((number, name))
        })
        .collect()
}

enum Kind {
    Folder,
    File(u64),
    Symlink(PathBuf),
    /// A hard link to an earlier entry.
    Link(PathBuf),
    Fifo,
}

/// Reads exactly `left` bytes: zeros if the file got shorter while it was read, never more.
struct Exactly<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exactly<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let wanted = buffer.len().min(self.left.min(usize::MAX as u64) as usize);
        if wanted == 0 {
            return Ok(0);
        }
        let mut read = self.inner.read(&mut buffer[..wanted])?;
        if read == 0 {
            buffer[..wanted].fill(0);
            read = wanted;
        }
        self.left -= read as u64;
        Ok(read)
    }
}

/// One PAX record: its length, itself included, then `key=value`.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let rest = key.len() + value.len() + 3;
    let mut length = rest;
    while length != rest + length.to_string().len() {
        length = rest + length.to_string().len();
    }
    let mut record = format!("{length} {key}=").into_bytes();
    record.extend_from_slice(value);
    record.push(b'\n');
    record
}

/// Writes the archive's entries.
struct Writer<W: Write> {
    builder: tar::Builder<W>,
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
}

impl<W: Write> Writer<W> {
    fn append(
        &mut self,
        name: &[u8],
        kind: Kind,
        owner: &Owner,
        mtime: u64,
        data: &mut dyn Read,
    ) -> io::Result<()> {
        let mut pax = Vec::new();
        let mut header = tar::Header::new_ustar();
        let ustar = header
            .as_ustar_mut()
            .ok_or_else(|| invalid("no ustar header"))?;
        if name.len() <= ustar.name.len() {
            ustar.name[..name.len()].copy_from_slice(name);
        } else {
            pax.extend(pax_record("path", name));
            ustar.name.copy_from_slice(&name[..100]);
        }
        let link = match &kind {
            Kind::Symlink(target) | Kind::Link(target) => Some(target.as_os_str().as_bytes()),
            _ => None,
        };
        if let Some(link) = link {
            if link.len() <= ustar.linkname.len() {
                ustar.linkname[..link.len()].copy_from_slice(link);
            } else {
                pax.extend(pax_record("linkpath", link));
                ustar.linkname.copy_from_slice(&link[..100]);
            }
        }
        if !owner.recorded {
            pax.extend(pax_record(RECORD_KEY, b"none"));
        }
        let (entry_type, size) = match kind {
            Kind::Folder => (tar::EntryType::Directory, 0),
            Kind::File(size) => (tar::EntryType::Regular, size),
            Kind::Symlink(_) => (tar::EntryType::Symlink, 0),
            Kind::Link(_) => (tar::EntryType::Link, 0),
            Kind::Fifo => (tar::EntryType::Fifo, 0),
        };
        header.set_entry_type(entry_type);
        header.set_size(size);
        header.set_mode(owner.mode & 0o7777);
        header.set_uid(owner.uid as u64);
        header.set_gid(owner.gid as u64);
        header.set_mtime(mtime);
        if let Some(user) = self.users.get(&owner.uid) {
            let _ = header.set_username(user);
        }
        if let Some(group) = self.groups.get(&owner.gid) {
            let _ = header.set_groupname(group);
        }
        header.set_cksum();

        if !pax.is_empty() {
            let mut extensions = tar::Header::new_ustar();
            let label = b"PaxHeader";
            extensions.as_ustar_mut().unwrap().name[..label.len()].copy_from_slice(label);
            extensions.set_entry_type(tar::EntryType::XHeader);
            extensions.set_size(pax.len() as u64);
            extensions.set_mode(0o644);
            extensions.set_mtime(mtime);
            extensions.set_cksum();
            self.builder.append(&extensions, pax.as_slice())?;
        }
        self.builder.append(
            &header,
            Exactly {
                inner: data,
                left: size,
            },
        )
    }
}

fn mtime(metadata: &fs::Metadata) -> u64 {
    metadata.mtime().max(0) as u64
}

/// A file's contents, also if its real mode doesn't let the app read it: proot gives Linux's
/// root every file, whatever its mode.
fn open_for_reading(path: &Path, metadata: &fs::Metadata) -> io::Result<File> {
    match File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() | 0o400))?;
            let file = File::open(path);
            fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
            file
        }
        file => file,
    }
}

/// Write a backup of the system at `root` to `output`. `about` says where it comes from (the
/// app, its version, the desktop and the user); the rest of the manifest is filled in here.
/// `progress` gets how far it got, and the whole.
pub fn back_up<W: Write>(
    root: &Path,
    output: W,
    about: Manifest,
    package_cache: bool,
    progress: &mut dyn FnMut(Progress, Progress),
) -> io::Result<Manifest> {
    let total = measure(root, package_cache)?;
    progress(Progress::default(), total);
    let manifest = Manifest {
        format: FORMAT,
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |it| it.as_secs()),
        files: total.files,
        bytes: total.bytes,
        package_cache,
        ..about
    };

    let mut encoder = zstd::stream::write::Encoder::new(output, LEVEL)?;
    encoder.include_checksum(true)?;
    let workers = std::thread::available_parallelism().map_or(1, |it| it.get());
    encoder.multithread(workers.saturating_sub(1).max(1) as u32)?;
    let mut writer = Writer {
        builder: tar::Builder::new(encoder),
        users: names(&root.join("etc/passwd")),
        groups: names(&root.join("etc/group")),
    };
    let description = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    let public = Owner {
        mode: 0o644,
        uid: 0,
        gid: 0,
        recorded: true,
    };
    writer.append(
        MANIFEST.as_bytes(),
        Kind::File(description.len() as u64),
        &public,
        manifest.created,
        &mut description.as_slice(),
    )?;

    let mut done = Progress::default();
    let mut reported = Progress::default();
    // The first name of each hard-linked file, by its data.
    let mut first_names: HashMap<PathBuf, PathBuf> = HashMap::new();
    walk(root, package_cache, |path, relative, metadata, folder| {
        let mut name = relative.as_os_str().as_bytes().to_vec();
        let file_type = metadata.file_type();
        let mut owner = owner_of(path, metadata, folder);
        let kind = if file_type.is_dir() {
            name.push(b'/');
            Kind::Folder
        } else if file_type.is_file() {
            Kind::File(metadata.len())
        } else if file_type.is_symlink() {
            match hard_link_data(root, path) {
                Some(data) => match first_names.get(&data) {
                    Some(first) => {
                        // A hard link has its file's owner and mode.
                        let data_metadata = fs::symlink_metadata(&data)?;
                        owner = Owner {
                            recorded: true,
                            ..owner_of(&data, &data_metadata, folder)
                        };
                        Kind::Link(first.clone())
                    }
                    None => {
                        first_names.insert(data.clone(), relative.to_path_buf());
                        let data_metadata = fs::symlink_metadata(&data)?;
                        owner = owner_of(&data, &data_metadata, folder);
                        let mut file = open_for_reading(&data, &data_metadata)?;
                        writer.append(
                            &name,
                            Kind::File(data_metadata.len()),
                            &owner,
                            mtime(&data_metadata),
                            &mut file,
                        )?;
                        done.files += 1;
                        done.bytes += data_metadata.len();
                        return Ok(None);
                    }
                },
                None => {
                    owner.recorded = true;
                    Kind::Symlink(fs::read_link(path)?)
                }
            }
        } else if file_type.is_fifo() {
            Kind::Fifo
        } else {
            // Sockets live only as long as their program; device files come from proot.
            return Ok(None);
        };
        let is_folder = matches!(kind, Kind::Folder);
        if let Kind::File(size) = kind {
            let mut file = open_for_reading(path, metadata)?;
            writer.append(&name, kind, &owner, mtime(metadata), &mut file)?;
            done.bytes += size;
        } else {
            writer.append(&name, kind, &owner, mtime(metadata), &mut io::empty())?;
        }
        if is_folder {
            return Ok(Some(owner));
        }
        done.files += 1;
        if done.files - reported.files >= 500 || done.bytes - reported.bytes >= 32 << 20 {
            reported = done;
            progress(done, total);
        }
        Ok(None)
    })?;
    progress(done, total);
    let encoder = writer.builder.into_inner()?;
    let mut output = encoder.finish()?;
    output.flush()?;
    Ok(manifest)
}

/// The archive's contents, whichever of the usual compressions it has.
fn decompressed<'a, R: Read + 'a>(input: R) -> io::Result<Box<dyn Read + 'a>> {
    let mut input = BufReader::with_capacity(1 << 20, input);
    let start: Vec<u8> = input.fill_buf()?.iter().take(6).copied().collect();
    Ok(if start.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(zstd::stream::read::Decoder::with_buffer(input)?)
    } else if start.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Box::new(xz2::read::XzDecoder::new_multi_decoder(input))
    } else if start.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::MultiGzDecoder::new(input))
    } else {
        Box::new(input)
    })
}

/// The description at the start of a backup; `None` for an archive made elsewhere.
pub fn read_manifest<R: Read>(input: R) -> io::Result<Option<Manifest>> {
    let mut archive = tar::Archive::new(decompressed(input)?);
    let Some(entry) = archive.entries()?.next() else {
        return Err(invalid("an empty archive"));
    };
    let mut entry = entry?;
    if inside(&entry.path()?) != Some(PathBuf::from(MANIFEST)) {
        return Ok(None);
    }
    let mut text = Vec::new();
    entry.read_to_end(&mut text)?;
    serde_json::from_slice(&text)
        .map(Some)
        .map_err(|error| invalid(&format!("a damaged description: {error}")))
}

/// An archive path as a path inside the system, or `None` if it would lead out of it.
fn inside(path: &Path) -> Option<PathBuf> {
    let mut inside = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => inside.push(name),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(inside)
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("a path with a NUL byte"))
}

/// Set a path's modification time, without following a symlink.
fn set_mtime(path: &Path, seconds: u64) -> io::Result<()> {
    let time = libc::timespec {
        tv_sec: seconds as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    let path = c_path(path)?;
    let status = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Make the folders on the way to `relative`, remembering those the archive didn't list.
fn make_folders(
    target: &Path,
    relative: &Path,
    folders: &mut HashMap<PathBuf, (Owner, u64)>,
) -> io::Result<()> {
    let mut folder = PathBuf::new();
    for component in relative.components() {
        folder.push(component);
        let path = target.join(&folder);
        if fs::symlink_metadata(&path).is_err() {
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            let default = Owner {
                mode: 0o755,
                uid: 0,
                gid: 0,
                recorded: false,
            };
            folders.entry(folder.clone()).or_insert((default, 0));
        }
    }
    Ok(())
}

/// Delete whatever is at `path`, folders with all they hold.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    // The real mode of a folder can keep even its owner out.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(path)? {
        remove_tree(&entry?.path())?;
    }
    fs::remove_dir(path)
}

/// Lay out the system in `input` at `target`, an empty or missing folder, as proot will see it
/// from `final_root`, where `target` goes once it's complete (its hard links point there).
pub fn restore<R: Read>(
    input: R,
    target: &Path,
    final_root: &Path,
    progress: &mut dyn FnMut(Progress),
) -> io::Result<Restored> {
    fs::create_dir_all(target)?;
    let mut restored = Restored::default();
    let mut reported = Progress::default();
    // Folders get their real modes and times last: the files go in first.
    let mut folders: HashMap<PathBuf, (Owner, u64)> = HashMap::new();
    // Hard links by the first name of their file, in the archive's order.
    let mut links: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut link_groups: HashMap<PathBuf, usize> = HashMap::new();

    let mut archive = tar::Archive::new(decompressed(input)?);
    for (index, entry) in archive.entries()?.enumerate() {
        let mut entry = entry?;
        let Some(relative) = inside(&entry.path()?) else {
            restored.left_out += 1;
            continue;
        };
        if index == 0 && relative == Path::new(MANIFEST) {
            let mut text = Vec::new();
            entry.read_to_end(&mut text)?;
            restored.manifest = serde_json::from_slice(&text).ok();
            continue;
        }
        let ours = restored.manifest.is_some();
        let mut components = relative.components();
        let Some(first) = components.next() else {
            continue;
        };
        let nested = components.next().is_some();
        if relative.iter().any(is_internal)
            || first.as_os_str() == hard_links::STORE_DIR
            || (nested && RUNTIME.iter().any(|it| first.as_os_str() == *it))
        {
            restored.left_out += 1;
            continue;
        }

        let header = entry.header();
        let entry_type = header.entry_type();
        let unrecorded = entry
            .pax_extensions()?
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|it| it.key_bytes() == RECORD_KEY.as_bytes() && it.value_bytes() == b"none");
        let header = entry.header();
        let mode = header.mode()? & 0o7777;
        let uid = header.uid()? as u32;
        let gid = header.gid()? as u32;
        // An archive made elsewhere has what `tar` saw. `tar` run as root in a proot system saw
        // the files proot kept no record for as root's, with the real modes the app's umask
        // left (nothing for the group and others): the app's own files, and some in the users'
        // homes. Those come back without a record, as they were, so that every user still
        // reads them.
        let unrecorded = if ours {
            unrecorded
        } else {
            uid == 0 && gid == 0 && mode & 0o077 == 0
        };
        let owner = Owner {
            mode,
            uid,
            gid,
            recorded: !entry_type.is_symlink() && !unrecorded,
        };
        let mtime = header.mtime()?;
        let path = target.join(&relative);
        if let Some(parent) = relative.parent() {
            make_folders(target, parent, &mut folders)?;
        }

        if entry_type.is_dir() {
            match fs::symlink_metadata(&path) {
                Ok(it) if it.is_dir() => {}
                Ok(_) => {
                    fs::remove_file(&path)?;
                    fs::create_dir(&path)?;
                }
                Err(_) => fs::create_dir(&path)?,
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            if owner.recorded {
                write_record(&path, &owner)?;
            }
            folders.insert(relative, (owner, mtime));
            continue;
        }

        if fs::symlink_metadata(&path).is_ok() {
            remove_tree(&path)?;
        }
        if entry_type.is_hard_link() {
            let Some(first) = entry.link_name()?.and_then(|it| inside(&it)) else {
                restored.left_out += 1;
                continue;
            };
            match link_groups.get(&first) {
                Some(&group) => links[group].1.push(relative),
                None => {
                    link_groups.insert(first.clone(), links.len());
                    links.push((first, vec![relative]));
                }
            }
        } else if entry_type.is_symlink() {
            let Some(link) = entry.link_name()? else {
                restored.left_out += 1;
                continue;
            };
            symlink(&link, &path)?;
            let _ = set_mtime(&path, mtime);
            if owner.recorded {
                write_record(&path, &owner)?;
            }
        } else if entry_type.is_fifo() {
            let status = unsafe { libc::mkfifo(c_path(&path)?.as_ptr(), 0o600) };
            if status != 0 {
                return Err(io::Error::last_os_error());
            }
            finish_file(&path, &owner, mtime)?;
        } else if entry_type.is_file() || entry_type.is_contiguous() || entry_type.is_gnu_sparse() {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            restored.progress.bytes += io::copy(&mut entry, &mut file)?;
            drop(file);
            finish_file(&path, &owner, mtime)?;
        } else {
            // Device files, which proot provides.
            restored.left_out += 1;
            continue;
        }
        restored.progress.files += 1;
        let done = restored.progress;
        if done.files - reported.files >= 500 || done.bytes - reported.bytes >= 32 << 20 {
            reported = done;
            progress(done);
        }
    }

    let store = target.join(hard_links::STORE_DIR);
    fs::create_dir_all(&store)?;
    for (first, names) in links {
        // The names were counted as they came.
        if !link_file(target, final_root, &store, &first, &names)? {
            restored.progress.files -= names.len() as u64;
            restored.left_out += names.len() as u64;
        }
    }
    hard_links::mark_migrated(&store)?;
    for (name, mode) in RUNTIME_MODES {
        let path = target.join(name);
        if fs::symlink_metadata(&path).is_err() {
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(*mode))?;
        }
    }

    // The deepest first, so that a folder's mode never keeps out what goes into it.
    let mut folders: Vec<(PathBuf, (Owner, u64))> = folders.into_iter().collect();
    folders.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (relative, (owner, mtime)) in folders {
        let path = target.join(relative);
        // Programs see a recorded folder's mode in its record; the app needs to get in.
        let real = if owner.recorded { 0o700 } else { owner.mode };
        let _ = set_mtime(&path, mtime);
        fs::set_permissions(&path, fs::Permissions::from_mode(real))?;
    }
    progress(restored.progress);
    Ok(restored)
}

/// Give a restored file (or FIFO) its mode, time and record. Programs see a recorded file's mode
/// in its record; for real, the app reads and writes it, and runs it if anyone may. One without
/// a record shows its real mode.
fn finish_file(path: &Path, owner: &Owner, mtime: u64) -> io::Result<()> {
    let real = if owner.recorded {
        0o600 | if owner.mode & 0o111 != 0 { 0o100 } else { 0 }
    } else {
        owner.mode
    };
    let _ = set_mtime(path, mtime);
    if owner.recorded {
        write_record(path, owner)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(real))
}

/// Make `first` and `names` proot's hard links to one file: its data in the store, an
/// intermediate symlink to it, and every name a symlink to that. False if `first` isn't a file.
fn link_file(
    target: &Path,
    final_root: &Path,
    store: &Path,
    first: &Path,
    names: &[PathBuf],
) -> io::Result<bool> {
    let data = target.join(first);
    let is_file = fs::symlink_metadata(&data).is_ok_and(|it| it.is_file());
    let Some(name) = first.file_name().filter(|_| is_file) else {
        return Ok(false);
    };
    let mut base = OsString::from(hard_links::PREFIX);
    base.push(name);
    let base = base.to_string_lossy().into_owned();
    let intermediate = (1..10_000)
        .map(|n| format!("{base}{n:04}"))
        .find(|it| fs::symlink_metadata(store.join(it)).is_err())
        .ok_or_else(|| invalid("no free name in the hard link store"))?;
    let stored = format!("{intermediate}.{:04}", names.len() + 1);
    fs::rename(&data, store.join(&stored))?;
    if let (Some(record), Some(stored_record)) =
        (record_path(&data), record_path(&store.join(&stored)))
    {
        if fs::symlink_metadata(&record).is_ok() {
            fs::rename(record, stored_record)?;
        }
    }
    let final_store = final_root.join(hard_links::STORE_DIR);
    symlink(final_store.join(&stored), store.join(&intermediate))?;
    for name in std::iter::once(first).chain(names.iter().map(PathBuf::as_path)) {
        let path = target.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            remove_tree(&path)?;
        }
        symlink(final_store.join(&intermediate), &path)?;
    }
    Ok(true)
}

/// Whether `root` holds a Linux system that setup can work with.
pub fn looks_like_a_system(root: &Path) -> bool {
    root.join("etc/passwd").is_file() && root.join("usr/bin").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A system as proot leaves it, at `<temporary>/files/arch`.
    fn system(temporary: &Path) -> PathBuf {
        let root = temporary.join("files/arch");
        let file = |path: &str, content: &str, real: u32, record: Option<&str>| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(real)).unwrap();
            if let Some(record) = record {
                fs::write(record_path(&path).unwrap(), record).unwrap();
            }
        };
        file(
            "etc/passwd",
            "root:x:0:0::/root:/bin/bash\ntester:x:1001:1001::/home/tester:/bin/bash\n",
            0o644,
            Some("644\n0\n0\n"),
        );
        file(
            "etc/group",
            "root:x:0:\ntester:x:1001:\n",
            0o644,
            Some("644\n0\n0\n"),
        );
        file("etc/shadow", "secret", 0o600, Some("600\n0\n0\n"));
        file("usr/bin/bash", "#!bash", 0o700, Some("755\n0\n0\n"));
        file("usr/bin/sudo", "#!sudo", 0o700, Some("4755\n0\n0\n"));
        file(
            "etc/localdesktop/plasma/kdeglobals",
            "[General]",
            0o600,
            None,
        );
        file(
            "home/tester/notes.txt",
            "mine",
            0o600,
            Some("644\n1001\n1001\n"),
        );
        fs::write(
            record_path(&root.join("home/tester")).unwrap(),
            "700\n1001\n1001\n",
        )
        .unwrap();
        for folder in ["proc", "sys", "dev", "run"] {
            fs::create_dir_all(root.join(folder)).unwrap();
        }
        fs::create_dir_all(root.join("tmp/.X11-unix")).unwrap();
        fs::write(root.join("tmp/leftover"), "gone").unwrap();
        fs::write(record_path(&root.join("tmp")).unwrap(), "1777\n0\n0\n").unwrap();
        file(
            &format!("{PACKAGE_CACHE}/bash-5.3.pkg.tar.zst"),
            "package",
            0o600,
            Some("644\n0\n0\n"),
        );
        symlink("bash", root.join("usr/bin/sh")).unwrap();
        // A hard link pair, laid out as proot does, pointing into another app's system.
        let store = root.join(hard_links::STORE_DIR);
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join(".proot.l2s.e2fsck0001.0002"), "fsck").unwrap();
        fs::write(
            record_path(&store.join(".proot.l2s.e2fsck0001.0002")).unwrap(),
            "755\n0\n0\n",
        )
        .unwrap();
        let other = Path::new("/data/data/app.polarbear/files/arch/.l2s");
        symlink(
            other.join(".proot.l2s.e2fsck0001.0002"),
            store.join(".proot.l2s.e2fsck0001"),
        )
        .unwrap();
        symlink(
            other.join(".proot.l2s.e2fsck0001"),
            root.join("usr/bin/e2fsck"),
        )
        .unwrap();
        symlink(
            other.join(".proot.l2s.e2fsck0001"),
            root.join("usr/bin/fsck.ext4"),
        )
        .unwrap();
        root
    }

    fn about() -> Manifest {
        Manifest {
            app: "app.polarbear".into(),
            version: "2.1.1".into(),
            desktop: "plasma".into(),
            user: "tester".into(),
            ..Manifest::default()
        }
    }

    fn back_up_to_vec(root: &Path, package_cache: bool) -> (Vec<u8>, Manifest) {
        let mut archive = Vec::new();
        let manifest = back_up(root, &mut archive, about(), package_cache, &mut |_, _| {}).unwrap();
        (archive, manifest)
    }

    /// Each entry as programs would see it through proot, from its records and links.
    fn seen(root: &Path) -> Vec<String> {
        let mut seen = Vec::new();
        walk(root, true, |path, relative, metadata, folder| {
            let owner = owner_of(path, metadata, folder);
            let what = if metadata.is_dir() {
                "folder".to_string()
            } else if let Some(data) = hard_link_data(root, path) {
                let data_metadata = fs::metadata(&data).unwrap();
                let owner = owner_of(&data, &data_metadata, folder);
                let content = fs::read_to_string(&data).unwrap();
                format!(
                    "hard link {:o} {} {} {content}",
                    owner.mode, owner.uid, owner.gid
                )
            } else if metadata.is_symlink() {
                format!("symlink to {}", fs::read_link(path).unwrap().display())
            } else {
                format!("file {}", fs::read_to_string(path).unwrap())
            };
            seen.push(format!(
                "{} {:o} {} {} {} {what}",
                relative.display(),
                owner.mode,
                owner.uid,
                owner.gid,
                owner.recorded,
            ));
            Ok(metadata.is_dir().then_some(owner))
        })
        .unwrap();
        seen.sort();
        seen
    }

    #[test]
    fn should_restore_what_programs_saw() {
        let temporary = tempdir().unwrap();
        let root = system(temporary.path());
        let (archive, manifest) = back_up_to_vec(&root, true);
        assert_eq!(manifest.files, 11);
        assert_eq!(
            read_manifest(archive.as_slice()).unwrap(),
            Some(manifest.clone())
        );

        // Into another app's system.
        let target = temporary.path().join("other/files/arch");
        let final_root = Path::new("/data/data/app.polarbear.dev/files/arch");
        let restored = restore(archive.as_slice(), &target, final_root, &mut |_| {}).unwrap();
        assert_eq!(restored.progress.files, manifest.files);
        assert_eq!(restored.progress.bytes, manifest.bytes);
        assert_eq!(restored.manifest, Some(manifest));
        assert_eq!(restored.left_out, 0);
        assert!(looks_like_a_system(&target));

        let before = seen(&root);
        let after = seen(&target);
        let without_runtime = |lines: Vec<String>| -> Vec<String> {
            lines
                .into_iter()
                .filter(|it| !it.starts_with("tmp/"))
                .collect()
        };
        assert_eq!(without_runtime(before), after);
        // The hard links point into the system they were restored for, and the store is marked
        // as migrated.
        let link = fs::read_link(target.join("usr/bin/e2fsck")).unwrap();
        assert!(
            link.starts_with(final_root.join(".l2s")),
            "{}",
            link.display()
        );
        assert_eq!(
            fs::read_link(target.join("usr/bin/fsck.ext4")).unwrap(),
            link
        );
        assert!(target.join(".l2s/.proot-migrated").exists());
        // Records as proot writes them; the file without one has its real mode back.
        assert_eq!(
            fs::read_to_string(record_path(&target.join("usr/bin/sudo")).unwrap()).unwrap(),
            "4755\n0\n0\n"
        );
        assert!(
            !record_path(&target.join("etc/localdesktop/plasma/kdeglobals"))
                .unwrap()
                .exists()
        );
        let real = |path: &str| fs::symlink_metadata(target.join(path)).unwrap().mode() & 0o7777;
        assert_eq!(real("etc/localdesktop/plasma/kdeglobals"), 0o600);
        assert_eq!(real("usr/bin/bash"), 0o700);
        assert_eq!(real("etc/shadow"), 0o600);
        // /tmp stays, empty.
        assert!(target.join("tmp").is_dir());
        assert!(!target.join("tmp/leftover").exists());
    }

    #[test]
    fn should_leave_the_package_cache_out_unless_asked() {
        let temporary = tempdir().unwrap();
        let root = system(temporary.path());
        let (archive, manifest) = back_up_to_vec(&root, false);
        assert_eq!(manifest.files, 10);
        assert!(!manifest.package_cache);
        let target = temporary.path().join("restored");
        restore(archive.as_slice(), &target, &target, &mut |_| {}).unwrap();
        assert!(target.join(PACKAGE_CACHE).is_dir());
        assert_eq!(fs::read_dir(target.join(PACKAGE_CACHE)).unwrap().count(), 0);
    }

    #[test]
    fn should_make_an_archive_tar_reads() {
        let temporary = tempdir().unwrap();
        let root = system(temporary.path());
        let long = "usr/share/a-folder-with-a-name-long-enough/to-need-a-pax-header-of-its-own/for-the-path-of/file.txt";
        fs::create_dir_all(root.join(long).parent().unwrap()).unwrap();
        fs::write(root.join(long), "long").unwrap();
        let (archive, _) = back_up_to_vec(&root, false);
        let mut entries = HashMap::new();
        let mut archive = tar::Archive::new(decompressed(archive.as_slice()).unwrap());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let header = entry.header();
            entries.insert(
                entry.path().unwrap().display().to_string(),
                (
                    header.entry_type(),
                    header.mode().unwrap(),
                    header.uid().unwrap(),
                    header.username().unwrap().map(str::to_string),
                    entry
                        .link_name()
                        .unwrap()
                        .map(|it| it.display().to_string()),
                ),
            );
        }
        assert_eq!(entries["usr/bin/bash"].1, 0o755);
        assert_eq!(entries["home/tester/notes.txt"].2, 1001);
        assert_eq!(
            entries["home/tester/notes.txt"].3.as_deref(),
            Some("tester")
        );
        assert_eq!(entries["usr/bin/e2fsck"].0, tar::EntryType::Regular);
        assert_eq!(entries["usr/bin/fsck.ext4"].0, tar::EntryType::Link);
        assert_eq!(entries["usr/bin/fsck.ext4"].1, 0o755);
        assert_eq!(
            entries["usr/bin/fsck.ext4"].4.as_deref(),
            Some("usr/bin/e2fsck")
        );
        assert_eq!(entries["usr/bin/sh"].4.as_deref(), Some("bash"));
        assert!(entries.contains_key(long));
        assert!(entries.contains_key("tmp/") || entries.contains_key("tmp"));
        assert!(!entries
            .keys()
            .any(|it| it.contains(".proot") || it.starts_with(".l2s")));
    }

    #[test]
    fn should_restore_an_archive_made_elsewhere() {
        let temporary = tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &str,
                       kind: tar::EntryType,
                       mode: u32,
                       uid: u64,
                       link: Option<&str>,
                       data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_uid(uid);
            header.set_gid(uid);
            header.set_size(data.len() as u64);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            builder.append_data(&mut header, path, data).unwrap();
        };
        add("./etc/", tar::EntryType::Directory, 0o755, 0, None, b"");
        add(
            "./etc/passwd",
            tar::EntryType::Regular,
            0o644,
            0,
            None,
            b"root:x:0:0::/root:/bin/sh\n",
        );
        add(
            "./usr/bin/ls",
            tar::EntryType::Regular,
            0o755,
            0,
            None,
            b"ls",
        );
        add(
            "./usr/bin/dir",
            tar::EntryType::Link,
            0o755,
            0,
            Some("./usr/bin/ls"),
            b"",
        );
        add("./dev/null", tar::EntryType::Char, 0o666, 0, None, b"");
        add(
            "./home/alice/a",
            tar::EntryType::Regular,
            0o600,
            1000,
            None,
            b"a",
        );
        // A script the app wrote, as `tar` run as root in a proot system sees it.
        add(
            "./usr/local/bin/start",
            tar::EntryType::Regular,
            0o700,
            0,
            None,
            b"#!/bin/sh",
        );
        let archive = builder.into_inner().unwrap();
        let mut compressed = Vec::new();
        let mut encoder =
            flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::fast());
        encoder.write_all(&archive).unwrap();
        encoder.finish().unwrap();

        let target = temporary.path().join("files/arch");
        let restored = restore(compressed.as_slice(), &target, &target, &mut |_| {}).unwrap();
        assert_eq!(restored.manifest, None);
        assert_eq!(restored.left_out, 1);
        assert!(looks_like_a_system(&target));
        assert_eq!(
            fs::read_to_string(record_path(&target.join("home/alice/a")).unwrap()).unwrap(),
            "600\n1000\n1000\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("usr/bin/dir")).unwrap(),
            "ls"
        );
        // Every user still runs it, as before.
        let script = target.join("usr/local/bin/start");
        assert!(!record_path(&script).unwrap().exists());
        assert_eq!(fs::metadata(&script).unwrap().mode() & 0o7777, 0o700);
        assert!(record_path(&target.join("etc/passwd")).unwrap().exists());
        let seen = seen(&target);
        assert!(
            seen.contains(&"usr/bin/dir 777 0 0 false hard link 755 0 0 ls".to_string()),
            "{seen:#?}"
        );
        // A folder the archive didn't list.
        assert!(
            seen.contains(&"home 755 0 0 false folder".to_string()),
            "{seen:#?}"
        );
        // The archive left /tmp out altogether.
        assert!(
            seen.contains(&"tmp 1777 0 0 false folder".to_string()),
            "{seen:#?}"
        );
    }

    #[test]
    fn should_refuse_paths_out_of_the_system() {
        assert_eq!(
            inside(Path::new("./etc/passwd")),
            Some(PathBuf::from("etc/passwd"))
        );
        assert_eq!(
            inside(Path::new("/etc/passwd")),
            Some(PathBuf::from("etc/passwd"))
        );
        assert_eq!(inside(Path::new("etc/../../x")), None);
    }

    #[test]
    fn should_count_pax_records_like_tar() {
        assert_eq!(pax_record("path", b"abc"), b"12 path=abc\n");
        let long = vec![b'x'; 95];
        let record = pax_record("path", &long);
        assert_eq!(record.len(), 105);
        assert!(record.starts_with(b"105 path="));
    }
}
