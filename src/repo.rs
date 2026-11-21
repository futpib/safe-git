use crate::{
    object::{
        Kind, MAX_OBJECT, MAX_OBJECTS, MAX_PACK, MAX_TOTAL, Object, Objects, Oid, ReadObjects,
    },
    pack,
    packed::{Entry as PackedEntry, Packed},
};
use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, bufread::ZlibDecoder, write::ZlibEncoder};
use rustix::{
    fs::{self, AtFlags, Mode, OFlags},
    io::Errno,
};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const READ: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);
const META_LIMIT: usize = 8 * 1024 * 1024;
static SERIAL: AtomicU64 = AtomicU64::new(0);

pub fn refname(name: &str) -> Result<()> {
    ensure!(
        name.len() <= 1024 && name.starts_with("refs/") && name.is_ascii(),
        "expected an ASCII refs/... name"
    );
    ensure!(
        !name
            .bytes()
            .any(|b| b <= 32 || b == 127 || b"~^:?*[\\".contains(&b)),
        "invalid ref name"
    );
    ensure!(
        !name.contains("..") && !name.contains("@{") && !name.ends_with('.'),
        "invalid ref name"
    );
    for part in name.split('/') {
        ensure!(
            !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"),
            "invalid ref component"
        );
    }
    Ok(())
}
pub fn branch(name: &str) -> Result<String> {
    let short = name.strip_prefix("refs/heads/").unwrap_or(name);
    ensure!(
        !short.starts_with("refs/") && !short.starts_with('-'),
        "expected a branch name"
    );
    refname(&format!("refs/heads/{short}"))?;
    Ok(short.to_owned())
}
pub fn tracking(remote: &str, name: &str) -> Result<String> {
    ensure!(
        !remote.contains('/') && !remote.starts_with('-'),
        "invalid remote label"
    );
    let name = format!("refs/remotes/{remote}/{}", branch(name)?);
    refname(&name)?;
    Ok(name)
}

/// All repository-internal access is relative to opened directory descriptors.
/// No repository symlink is followed, even during component traversal.
pub struct Repo {
    pub(crate) root: File,
    pub(crate) worktree: Option<File>,
    _lock: File,
}
impl Repo {
    pub fn init(path: &Path, initial: &str) -> Result<()> {
        let initial = branch(initial)?;
        std::fs::create_dir(path).context("init requires a new directory")?;
        let root = File::from(fs::open(path, DIR, Mode::empty())?);
        let repo = Self::lock(root)?;
        repo.parent("objects/info/.placeholder", true)?;
        repo.parent("objects/pack/.placeholder", true)?;
        repo.parent("refs/heads/.placeholder", true)?;
        repo.parent("refs/remotes/.placeholder", true)?;
        repo.atomic(
            "config",
            b"[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
        )?;
        repo.atomic("HEAD", format!("ref: refs/heads/{initial}\n").as_bytes())?;
        Ok(())
    }
    fn lock(root: File) -> Result<Self> {
        let fd = fs::openat(
            &root,
            ".safe-git.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )?;
        let lock = File::from(fd);
        let meta = lock.metadata()?;
        ensure!(
            meta.is_file() && meta.nlink() == 1,
            "unsafe repository lock file"
        );
        fs::flock(&lock, fs::FlockOperation::NonBlockingLockExclusive)
            .context("repository is locked by another safe-git operation")?;
        Ok(Self {
            root,
            worktree: None,
            _lock: lock,
        })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let directory =
            File::from(fs::open(path, DIR, Mode::empty()).context("open repository directory")?);
        let (metadata, worktree) = match fs::openat(&directory, ".git", DIR, Mode::empty()) {
            Ok(fd) => (File::from(fd), Some(directory)),
            Err(Errno::NOENT) => (directory, None),
            Err(e) => return Err(e).context(
                ".git must be a real directory; linked worktrees and gitdir files are unsupported",
            ),
        };
        let mut repo = Self::lock(metadata)?;
        repo.worktree = worktree;
        for unsupported in [
            "commondir",
            "gitdir",
            "shallow",
            "objects/info/alternates",
            "objects/info/http-alternates",
            "info/grafts",
            "refs/replace",
        ] {
            ensure!(
                !repo.exists(unsupported)?,
                "unsupported repository metadata: {unsupported}"
            );
        }
        let config = repo
            .read("config", META_LIMIT)?
            .context("repository has no config")?;
        let bare = validate_config(&config)?;
        ensure!(
            bare == repo.worktree.is_none(),
            "use the working-tree root for a non-bare repository"
        );
        repo.head()?;
        // Verify these directory components even before any object/ref reads.
        repo.parent("objects/.check", false)?;
        repo.parent("refs/.check", false)?;
        Ok(repo)
    }
    pub(crate) fn parent(&self, path: &str, create: bool) -> Result<(File, String)> {
        let mut parts: Vec<_> = path.split('/').collect();
        ensure!(
            parts
                .iter()
                .all(|p| !p.is_empty() && *p != "." && *p != ".."),
            "invalid internal path"
        );
        let name = parts.pop().context("empty path")?.to_owned();
        let mut dir = self.root.try_clone()?;
        for part in parts {
            if create {
                match fs::mkdirat(&dir, part, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) => dir.sync_all()?,
                    Err(Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            dir = File::from(
                fs::openat(&dir, part, DIR, Mode::empty())
                    .with_context(|| format!("open repository directory {part:?}"))?,
            );
        }
        Ok((dir, name))
    }
    pub(crate) fn exists(&self, path: &str) -> Result<bool> {
        let (dir, name) = match self.parent(path, false) {
            Ok(v) => v,
            Err(e) if missing(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        match fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) fn read(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        let (dir, name) = match self.parent(path, false) {
            Ok(v) => v,
            Err(e) if missing(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let fd = match fs::openat(&dir, name.as_str(), READ, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open repository file {path:?}")),
        };
        let file = File::from(fd);
        ensure!(
            file.metadata()?.is_file(),
            "repository file is not regular: {path:?}"
        );
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit,
            "repository file exceeds limit: {path:?}"
        );
        Ok(Some(bytes))
    }
    pub(crate) fn atomic(&self, path: &str, data: &[u8]) -> Result<()> {
        let (dir, name) = self.parent(path, true)?;
        let temp = format!(
            ".safe-git-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let mut lock = Pending::new(dir, temp)?;
        lock.file.write_all(data)?;
        lock.commit(&name)
    }
    pub fn head(&self) -> Result<String> {
        let bytes = self.read("HEAD", 2048)?.context("missing HEAD")?;
        let value = std::str::from_utf8(&bytes)?
            .strip_suffix('\n')
            .unwrap_or(std::str::from_utf8(&bytes)?);
        let name = value
            .strip_prefix("ref: ")
            .context("detached HEAD is unsupported")?;
        ensure!(
            name.starts_with("refs/heads/"),
            "HEAD must name a local branch"
        );
        refname(name)?;
        Ok(name.to_owned())
    }
    pub fn read_ref(&self, name: &str) -> Result<Option<Oid>> {
        refname(name)?;
        if let Some(value) = self.read(name, 128)? {
            return Ok(Some(parse_ref(&value)?));
        }
        Ok(self.packed_refs()?.get(name).copied())
    }
    fn packed_refs(&self) -> Result<std::collections::HashMap<String, Oid>> {
        let mut refs = std::collections::HashMap::new();
        if let Some(packed) = self.read("packed-refs", META_LIMIT)? {
            for line in packed
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty() && l[0] != b'#' && l[0] != b'^')
            {
                ensure!(line.len() > 41 && line[40] == b' ', "malformed packed-refs");
                let id = Oid::parse(&line[..40])?;
                let reference = std::str::from_utf8(&line[41..])?;
                refname(reference)?;
                ensure!(id != Oid::ZERO, "zero packed ref");
                ensure!(
                    refs.insert(reference.to_owned(), id).is_none(),
                    "duplicate packed ref"
                );
            }
        }
        Ok(refs)
    }
    pub fn update_ref(&self, name: &str, expected: Option<Oid>, new: Oid) -> Result<()> {
        self.prepare_ref(name, expected)?.commit(new)
    }
    pub(crate) fn prepare_ref(&self, name: &str, expected: Option<Oid>) -> Result<RefUpdate> {
        refname(name)?;
        // A packed ref must not become a directory (or vice versa).
        // Check before creating parents so a failed update leaves no shadowing directory.
        for existing in self.packed_refs()?.keys() {
            ensure!(
                !existing.starts_with(&format!("{name}/"))
                    && !name.starts_with(&format!("{existing}/")),
                "ref directory/file conflict"
            );
        }
        let (dir, file) = self.parent(name, true)?;
        let lock = Pending::new(dir, format!("{file}.lock")).context("acquire Git ref lock")?;
        ensure!(
            self.read_ref(name)? == expected,
            "ref changed concurrently; retry"
        );
        Ok(RefUpdate { lock, name: file })
    }
    pub fn write_object(&self, obj: &Object) -> Result<()> {
        let id = obj.oid()?.to_string();
        let path = format!("objects/{}/{}", &id[..2], &id[2..]);
        // Verify existing objects rather than silently trusting their filenames.
        if let Some(bytes) = self.read(&path, MAX_OBJECT + 1024 * 1024)? {
            ensure!(
                loose(&bytes)?.oid()? == obj.oid()?,
                "existing object is corrupt"
            );
            return Ok(());
        }
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        write!(z, "{} {}\0", obj.kind.name(), obj.data.len())?;
        z.write_all(&obj.data)?;
        self.atomic(&path, &z.finish()?)
    }
}
fn missing(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Errno>() == Some(&Errno::NOENT)
}
fn parse_ref(bytes: &[u8]) -> Result<Oid> {
    let id = Oid::parse(bytes.strip_suffix(b"\n").unwrap_or(bytes))?;
    ensure!(id != Oid::ZERO, "zero object ID in ref");
    Ok(id)
}
pub(crate) struct RefUpdate {
    pub(crate) lock: Pending,
    name: String,
}
impl RefUpdate {
    pub(crate) fn commit(mut self, new: Oid) -> Result<()> {
        ensure!(new != Oid::ZERO, "zero ref update");
        self.lock.file.write_all(format!("{new}\n").as_bytes())?;
        self.lock.commit(&self.name)
    }
}
pub(crate) struct Pending {
    dir: File,
    name: String,
    pub(crate) file: File,
    committed: bool,
}
impl Pending {
    pub(crate) fn new(dir: File, name: String) -> Result<Self> {
        let file = File::from(fs::openat(
            &dir,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        Ok(Self {
            dir,
            name,
            file,
            committed: false,
        })
    }
    pub(crate) fn commit(&mut self, name: &str) -> Result<()> {
        self.file.sync_all()?;
        fs::renameat(&self.dir, self.name.as_str(), &self.dir, name)?;
        self.committed = true;
        self.dir.sync_all()?;
        Ok(())
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::unlinkat(&self.dir, self.name.as_str(), AtFlags::empty());
        }
    }
}

fn validate_config(bytes: &[u8]) -> Result<bool> {
    let config = std::str::from_utf8(bytes).context("config is not UTF-8")?;
    let mut core = false;
    let mut bare = None;
    let mut version = None;
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            ensure!(line.ends_with(']'), "unsupported config section syntax");
            let section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            ensure!(
                section != "extensions" && !section.starts_with("extensions "),
                "repository extensions are unsupported"
            );
            core = section == "core";
            continue;
        }
        if !core {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value
                .split(['#', ';'])
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            match key.trim().to_ascii_lowercase().as_str() {
                "bare" => {
                    ensure!(bare.is_none(), "duplicate core.bare");
                    bare = Some(value.eq_ignore_ascii_case("true"));
                }
                "repositoryformatversion" => {
                    ensure!(version.is_none(), "duplicate format version");
                    version = Some(value == "0" || value == "1");
                }
                "worktree" => bail!("core.worktree redirection is unsupported"),
                _ => {}
            }
        }
    }
    ensure!(
        bare.is_some() && version == Some(true),
        "requires a SHA-1 repository with format version 0 or 1"
    );
    Ok(bare == Some(true))
}

fn loose(bytes: &[u8]) -> Result<Object> {
    let mut decoder = ZlibDecoder::new(bytes);
    let mut decoded = Vec::new();
    (&mut decoder)
        .take((MAX_OBJECT + 129) as u64)
        .read_to_end(&mut decoded)?;
    ensure!(
        decoded.len() <= MAX_OBJECT + 128 && decoder.total_in() as usize == bytes.len(),
        "oversized or trailing loose object data"
    );
    let nul = decoded
        .iter()
        .position(|b| *b == 0)
        .context("missing object header")?;
    ensure!(nul < 128, "oversized object header");
    let header = std::str::from_utf8(&decoded[..nul])?;
    let (kind, size) = header.split_once(' ').context("invalid object header")?;
    let kind = Kind::parse(kind.as_bytes())?;
    let size: usize = size.parse()?;
    ensure!(
        size <= MAX_OBJECT && decoded.len() - nul - 1 == size,
        "loose object size mismatch"
    );
    // Re-inflation verifies a complete zlib stream (read adapters tolerate truncation).
    pack::inflate(bytes, decoded.len())?;
    Ok(Object {
        kind,
        data: decoded[nul + 1..].to_vec(),
    })
}

pub struct Store<'a> {
    pub repo: &'a Repo,
    objects: Objects,
    bytes: usize,
    incoming: Objects,
    incoming_bytes: usize,
    packs: Vec<Packed>,
    packs_loaded: bool,
}
#[derive(Debug)]
pub struct MissingObject(pub Oid);
impl std::fmt::Display for MissingObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing object {}; automatic promisor fetching is not implemented",
            self.0
        )
    }
}
impl std::error::Error for MissingObject {}
impl<'a> Store<'a> {
    pub fn new(repo: &'a Repo) -> Self {
        Self {
            repo,
            objects: Objects::new(),
            bytes: 0,
            incoming: Objects::new(),
            incoming_bytes: 0,
            packs: Vec::new(),
            packs_loaded: false,
        }
    }
    pub fn add(&mut self, objects: Objects) -> Result<()> {
        for (id, obj) in objects {
            if !self.incoming.contains_key(&id) {
                self.incoming_bytes += obj.data.len();
                ensure!(
                    self.incoming_bytes <= MAX_TOTAL && self.incoming.len() < MAX_OBJECTS,
                    "incoming object budget exceeded"
                );
                self.incoming.insert(id, obj);
            }
        }
        Ok(())
    }
    fn insert(&mut self, id: Oid, obj: Arc<Object>) -> Result<()> {
        if !self.objects.contains_key(&id) {
            // This cache may be discarded: local objects remain in verified packs
            // or loose storage. Incoming objects are retained separately.
            if self.bytes + obj.data.len() > 128 * 1024 * 1024 || self.objects.len() >= MAX_OBJECTS
            {
                self.objects.clear();
                self.bytes = 0;
            }
            self.bytes = self
                .bytes
                .checked_add(obj.data.len())
                .context("size overflow")?;
            ensure!(
                self.bytes <= MAX_TOTAL && self.objects.len() < MAX_OBJECTS,
                "object cache limit exceeded"
            );
            self.objects.insert(id, obj);
        }
        Ok(())
    }
    fn packs(&mut self) -> Result<()> {
        self.packs_loaded = true;
        let (dir, _) = match self.repo.parent("objects/pack/.check", false) {
            Ok(v) => v,
            Err(e) if missing(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut total = 0;
        let mut count = 0;
        for entry in fs::Dir::read_from(&dir)? {
            let entry = entry?;
            count += 1;
            ensure!(count <= 4096, "too many pack directory entries");
            let name = entry.file_name().to_str()?;
            if !name.ends_with(".pack") {
                continue;
            }
            ensure!(
                name.len() == 50
                    && name.starts_with("pack-")
                    && name.as_bytes()[5..45].iter().all(u8::is_ascii_hexdigit),
                "invalid pack filename"
            );
            let bytes = self
                .repo
                .read(&format!("objects/pack/{name}"), MAX_PACK)?
                .context("pack disappeared")?;
            total += bytes.len();
            ensure!(total <= MAX_PACK, "local packs exceed byte limit");
            let index = self
                .repo
                .read(&format!("objects/pack/{}.idx", &name[..45]), META_LIMIT)?
                .context("pack has no index")?;
            self.packs.push(Packed::new(bytes, &index)?);
        }
        Ok(())
    }
}
impl Store<'_> {
    fn get_inner(&mut self, oid: Oid, depth: usize, decoded: &mut usize) -> Result<Arc<Object>> {
        ensure!(depth <= 64, "local delta chain exceeds limit");
        if let Some(obj) = self.incoming.get(&oid) {
            return Ok(Arc::clone(obj));
        }
        if let Some(obj) = self.objects.get(&oid) {
            return Ok(Arc::clone(obj));
        }
        let id = oid.to_string();
        if let Some(bytes) = self.repo.read(
            &format!("objects/{}/{}", &id[..2], &id[2..]),
            MAX_OBJECT + 1024 * 1024,
        )? {
            let obj = Arc::new(loose(&bytes)?);
            ensure!(obj.oid()? == oid, "object hash mismatch: {oid}");
            self.insert(oid, Arc::clone(&obj))?;
            return Ok(obj);
        }
        if !self.packs_loaded {
            self.packs()?;
        }
        let (pack, offset) = self
            .packs
            .iter()
            .enumerate()
            .find_map(|(i, p)| p.ids.get(&oid).map(|o| (i, *o)))
            .ok_or(MissingObject(oid))?;
        let entry = self.packs[pack].entry(offset)?;
        let object = match entry {
            PackedEntry::Object(object) => object,
            PackedEntry::Delta(base, delta) => {
                *decoded += delta.len();
                ensure!(*decoded <= MAX_TOTAL, "local delta byte budget exceeded");
                let base = self.get_inner(base, depth + 1, decoded)?;
                Object {
                    kind: base.kind,
                    data: pack::apply_delta(&base.data, &delta)?,
                }
            }
        };
        *decoded += object.data.len();
        ensure!(*decoded <= MAX_TOTAL, "local delta byte budget exceeded");
        ensure!(object.oid()? == oid, "packed object hash mismatch: {oid}");
        let object = Arc::new(object);
        self.insert(oid, Arc::clone(&object))?;
        Ok(object)
    }
}
impl ReadObjects for Store<'_> {
    fn get(&mut self, oid: Oid) -> Result<Arc<Object>> {
        self.get_inner(oid, 0, &mut 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_ref_directory_conflicts_leave_no_shadow() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repo");
        Repo::init(&path, "main").unwrap();
        let id = Oid([1; 20]);
        std::fs::write(path.join("packed-refs"), format!("{id} refs/heads/main\n")).unwrap();
        let repo = Repo::open(&path).unwrap();
        assert!(repo.update_ref("refs/heads/main/nested", None, id).is_err());
        assert!(!path.join("refs/heads/main").exists());
        assert_eq!(repo.read_ref("refs/heads/main").unwrap(), Some(id));
    }
    #[test]
    fn loose_objects_require_complete_exact_streams() {
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        z.write_all(b"blob 3\0abc").unwrap();
        let bytes = z.finish().unwrap();
        assert_eq!(loose(&bytes).unwrap().data, b"abc");
        for end in 0..bytes.len() {
            assert!(loose(&bytes[..end]).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(loose(&trailing).is_err());
    }
    #[test]
    fn ref_validation() {
        for name in ["refs/heads/main", "refs/remotes/origin/a/b"] {
            refname(name).unwrap();
        }
        for name in [
            "main",
            "refs/heads/../x",
            "refs/heads/.x",
            "refs/heads/x.lock",
            "refs/heads/a\\b",
            "refs/heads/a\nb",
            "refs/heads/a@{1}",
            "refs//x",
            "refs/heads/a.",
        ] {
            assert!(refname(name).is_err(), "{name}");
        }
    }
    #[test]
    fn refs_objects_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repo");
        Repo::init(&path, "main").unwrap();
        let repo = Repo::open(&path).unwrap();
        assert!(Repo::open(&path).is_err());
        let obj = Object {
            kind: Kind::Blob,
            data: b"hello".to_vec(),
        };
        let id = obj.oid().unwrap();
        repo.write_object(&obj).unwrap();
        assert_eq!(Store::new(&repo).get(id).unwrap().data, b"hello");
        repo.update_ref("refs/heads/main", None, id).unwrap();
        assert!(repo.update_ref("refs/heads/main", None, id).is_err());
        assert_eq!(repo.read_ref("refs/heads/main").unwrap(), Some(id));
        std::os::unix::fs::symlink(temp.path(), path.join("refs/remotes/evil")).unwrap();
        assert!(repo.update_ref("refs/remotes/evil/main", None, id).is_err());
        drop(repo);
        std::fs::remove_file(path.join("config")).unwrap();
        std::os::unix::fs::symlink("HEAD", path.join("config")).unwrap();
        assert!(Repo::open(&path).is_err());
    }
}
