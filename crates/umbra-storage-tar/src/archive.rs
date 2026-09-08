use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) enum Content {
    Empty,
    Stored {
        file: Arc<File>,
        offset: u64,
        len: u64,
    },
    Patch {
        base: Box<Content>,
        offset: u64,
        bytes: Vec<u8>,
        len: u64,
    },
}
impl Content {
    pub(crate) fn read_at(&self, position: u64, out: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Empty => Ok(0),
            Self::Stored { file, offset, len } => {
                let count = (*len).saturating_sub(position).min(out.len() as u64) as usize;
                if count == 0 {
                    return Ok(0);
                }
                let mut done = 0;
                while done < count {
                    let n = file.read_at(&mut out[done..count], offset + position + done as u64)?;
                    if n == 0 {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    done += n;
                }
                Ok(count)
            }
            Self::Patch {
                base,
                offset,
                bytes,
                len,
            } => {
                let count = (*len).saturating_sub(position).min(out.len() as u64) as usize;
                let out = &mut out[..count];
                out.fill(0);
                base.read_at(position, out)?;
                let start = position.max(*offset);
                let end = (position + count as u64).min(offset + bytes.len() as u64);
                if start < end {
                    out[(start - position) as usize..(end - position) as usize].copy_from_slice(
                        &bytes[(start - offset) as usize..(end - offset) as usize],
                    );
                }
                Ok(count)
            }
        }
    }
}
struct ContentReader<'a> {
    content: &'a Content,
    position: u64,
}
impl Read for ContentReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.content.read_at(self.position, out)?;
        self.position += n as u64;
        Ok(n)
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub(crate) stat: BlobStat,
    pub(crate) target: Option<BytePath>,
    pub(crate) content: Content,
}
#[derive(Clone, Debug)]
pub(crate) struct State {
    request: OpenRunRequest,
    layout: (StoragePath, BytePath, BytePath),
    pub(crate) nodes: HashMap<StoragePath, Node>,
    pub(crate) retries: Vec<(StorageRequest, Result<StorageResponse>)>,
}
type Manifest = (
    OpenRunRequest,
    (StoragePath, BytePath, BytePath),
    Vec<(StoragePath, BlobStat, Option<BytePath>)>,
    Vec<(StorageRequest, Result<StorageResponse>)>,
);
impl State {
    pub(crate) fn new(request: &OpenRunRequest, config: &TarStorageConfig) -> Self {
        let mut nodes = HashMap::new();
        for anchor in [StorageAnchor::Root, StorageAnchor::Control] {
            nodes.insert(
                StoragePath::new(anchor, Vec::new()).unwrap(),
                new_node(&CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                }),
            );
        }
        Self {
            request: request.clone(),
            layout: (
                config.run_parent.clone(),
                config.root_anchor.clone(),
                config.control_anchor.clone(),
            ),
            nodes,
            retries: Vec::new(),
        }
    }
    pub(crate) fn validate_identity(
        &self,
        request: &OpenRunRequest,
        config: &TarStorageConfig,
    ) -> Result<()> {
        if self.request.run_id != request.run_id
            || self.request.immutable_base != request.immutable_base
            || self.request.policy.format_version != request.policy.format_version
            || self.layout
                != (
                    config.run_parent.clone(),
                    config.root_anchor.clone(),
                    config.control_anchor.clone(),
                )
        {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "archive run/base/format/layout mismatch",
            ));
        }
        Ok(())
    }
    fn entry_path(&self, path: &StoragePath) -> Vec<u8> {
        let mut bytes = self.layout.0.as_bytes().to_vec();
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(self.request.run_id.0.to_string().as_bytes());
        bytes.push(b'/');
        bytes.extend_from_slice(match path.anchor() {
            StorageAnchor::Root => self.layout.1.as_bytes(),
            StorageAnchor::Control => self.layout.2.as_bytes(),
        });
        if !path.as_bytes().is_empty() {
            bytes.push(b'/');
            bytes.extend_from_slice(path.as_bytes());
        }
        bytes
    }
}
pub(crate) fn new_node(options: &CreateOptions) -> Node {
    let (kind, target) = match &options.kind {
        CreateKind::File => (ObjectKind::File, None),
        CreateKind::Directory => (ObjectKind::Directory, None),
        CreateKind::LogicalSymlink { target } => (ObjectKind::LogicalSymlink, Some(target.clone())),
    };
    Node {
        stat: BlobStat {
            object_id: ObjectId(Uuid::new_v4()),
            kind,
            len: target.as_ref().map_or(0, |t| t.as_bytes().len() as u64),
            link_count: 1,
            mode: options.mode & 0o7777,
            uid: 0,
            gid: 0,
            modified_nanos: now(),
        },
        target,
        content: Content::Empty,
    }
}
pub(crate) fn now() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i128
}
fn header(kind: ObjectKind, stat: &BlobStat) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(match kind {
        ObjectKind::File => tar::EntryType::Regular,
        ObjectKind::Directory => tar::EntryType::Directory,
        ObjectKind::LogicalSymlink => tar::EntryType::Symlink,
    });
    h.set_size(if kind == ObjectKind::File {
        stat.len
    } else {
        0
    });
    h.set_mode(stat.mode);
    h.set_uid(stat.uid as u64);
    h.set_gid(stat.gid as u64);
    h.set_mtime((stat.modified_nanos.max(0) / 1_000_000_000) as u64);
    h
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// A complete tar is synced before publication. CreateNew uses an exclusive hard
// link of the complete temporary inode, avoiding any partially written archive.
pub(crate) fn encode_manifest(state: &State) -> Result<Vec<u8>> {
    let mut nodes: Vec<_> = state.nodes.iter().collect();
    nodes.sort_by_key(|(p, _)| state.entry_path(p));
    let records: Vec<_> = nodes
        .iter()
        .map(|(p, n)| ((*p).clone(), n.stat.clone(), n.target.clone()))
        .collect();
    let manifest = serde_json::to_vec(&(&state.request, &state.layout, records, &state.retries))
        .map_err(json_error)?;
    if manifest.len() as u64 > MANIFEST_LIMIT {
        return Err(error(
            ErrorKind::StorageUnavailable,
            "archive metadata/retry budget exhausted (16 MiB)",
        ));
    }
    Ok(manifest)
}

pub(crate) fn publish(state: &State, destination: &Path, exclusive: bool) -> Result<()> {
    let manifest = encode_manifest(state)?;
    publish_prepared(state, &manifest, destination, exclusive)
}

pub(crate) fn publish_prepared(
    state: &State,
    manifest: &[u8],
    destination: &Path,
    exclusive: bool,
) -> Result<()> {
    let mut nodes: Vec<_> = state.nodes.iter().collect();
    nodes.sort_by_key(|(p, _)| state.entry_path(p));
    let temp = Temp(destination.with_file_name(format!(".umbra-tar-{}", Uuid::new_v4())));
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp.0)
        .map_err(io)?;
    let mut builder = tar::Builder::new(f);
    let mut h = tar::Header::new_gnu();
    h.set_size(manifest.len() as u64);
    h.set_mode(0o600);
    h.set_mtime(0);
    h.set_cksum();
    builder
        .append_data(&mut h, ".provider/manifest.json", manifest)
        .map_err(io)?;
    for (path, node) in nodes {
        let name = state.entry_path(path);
        let name = Path::new(OsStr::from_bytes(&name));
        let mut h = header(node.stat.kind, &node.stat);
        if let Some(target) = &node.target {
            builder
                .append_link(
                    &mut h,
                    name,
                    Path::new(OsStr::from_bytes(target.as_bytes())),
                )
                .map_err(io)?;
        } else {
            builder
                .append_data(
                    &mut h,
                    name,
                    ContentReader {
                        content: &node.content,
                        position: 0,
                    },
                )
                .map_err(io)?;
        }
    }
    builder.finish().map_err(io)?;
    builder.into_inner().map_err(io)?.sync_all().map_err(io)?;
    if exclusive {
        fs::hard_link(&temp.0, destination).map_err(io)?;
        fs::remove_file(&temp.0).map_err(io)?;
    } else {
        match regular(destination) {
            Ok(_) => (),
            Err(e) if e.kind == ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        fs::rename(&temp.0, destination).map_err(io)?;
    }
    sync_dir(destination.parent().unwrap())
}

pub(crate) fn load(path: &Path) -> Result<State> {
    let file = Arc::new(regular(path)?);
    let file_len = file.metadata().map_err(io)?.len();
    if file_len < 1024 || file_len % 512 != 0 {
        return Err(error(ErrorKind::CorruptJournal, "truncated tar archive"));
    }
    let mut input = file.try_clone().map_err(io)?;
    input.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut archive = tar::Archive::new(input);
    let mut entries = archive.entries_with_seek().map_err(io)?;
    let mut manifest = entries
        .next()
        .ok_or_else(|| error(ErrorKind::CorruptJournal, "missing tar manifest"))?
        .map_err(io)?;
    if manifest.path_bytes().as_ref() != b".provider/manifest.json"
        || !manifest.header().entry_type().is_file()
        || manifest.size() > MANIFEST_LIMIT
    {
        return Err(error(ErrorKind::CorruptJournal, "invalid tar manifest"));
    }
    let mut bytes = Vec::new();
    manifest.read_to_end(&mut bytes).map_err(io)?;
    let (request, layout, records, retries): Manifest =
        serde_json::from_slice(&bytes).map_err(json_error)?;
    drop(manifest);
    let config = TarStorageConfig {
        archive_path: path.to_path_buf(),
        run_parent: layout.0.clone(),
        root_anchor: layout.1.clone(),
        control_anchor: layout.2.clone(),
    };
    config.validate()?;
    let mut state = State {
        request,
        layout,
        nodes: HashMap::new(),
        retries,
    };
    let mut expected = HashMap::new();
    let mut ids = std::collections::HashSet::new();
    for (path, stat, target) in records {
        if stat.link_count != 1
            || !ids.insert(stat.object_id)
            || stat.mode > 0o7777
            || stat.len > i64::MAX as u64
            || (stat.kind == ObjectKind::LogicalSymlink) != target.is_some()
            || (stat.kind == ObjectKind::Directory && stat.len != 0)
            || target
                .as_ref()
                .is_some_and(|t| t.as_bytes().len() as u64 != stat.len)
        {
            return Err(error(ErrorKind::CorruptJournal, "invalid indexed metadata"));
        }
        expected.insert(state.entry_path(&path), path.clone());
        if state
            .nodes
            .insert(
                path,
                Node {
                    stat,
                    target,
                    content: Content::Empty,
                },
            )
            .is_some()
        {
            return Err(error(ErrorKind::CorruptJournal, "duplicate index path"));
        }
    }
    for entry in entries {
        let entry = entry.map_err(io)?;
        let name = entry.path_bytes();
        let path = expected.remove(name.as_ref()).ok_or_else(|| {
            error(
                ErrorKind::CorruptJournal,
                "unexpected, duplicate or escaping tar entry",
            )
        })?;
        let node = state.nodes.get_mut(&path).unwrap();
        let h = entry.header();
        let kind = match h.entry_type() {
            t if t.is_file() => ObjectKind::File,
            t if t.is_dir() => ObjectKind::Directory,
            t if t.is_symlink() => ObjectKind::LogicalSymlink,
            _ => return Err(unsupported()),
        };
        let size = if kind == ObjectKind::File {
            node.stat.len
        } else {
            0
        };
        if kind != node.stat.kind
            || entry.size() != size
            || h.mode().map_err(io)? != node.stat.mode
            || h.uid().map_err(io)? != node.stat.uid as u64
            || h.gid().map_err(io)? != node.stat.gid as u64
            || h.mtime().map_err(io)? != (node.stat.modified_nanos.max(0) / 1_000_000_000) as u64
            || entry.link_name_bytes().as_deref() != node.target.as_ref().map(BytePath::as_bytes)
            || entry
                .raw_file_position()
                .checked_add(size)
                .is_none_or(|end| end > file_len.saturating_sub(1024))
        {
            return Err(error(
                ErrorKind::CorruptJournal,
                "tar entry differs from index",
            ));
        }
        if kind == ObjectKind::File {
            node.content = Content::Stored {
                file: file.clone(),
                offset: entry.raw_file_position(),
                len: size,
            };
        }
    }
    if !expected.is_empty() {
        return Err(error(ErrorKind::CorruptJournal, "missing tar entries"));
    }
    for anchor in [StorageAnchor::Root, StorageAnchor::Control] {
        let p = StoragePath::new(anchor, Vec::new())?;
        if state
            .nodes
            .get(&p)
            .is_none_or(|n| n.stat.kind != ObjectKind::Directory)
        {
            return Err(error(ErrorKind::CorruptJournal, "missing directory anchor"));
        }
    }
    for path in state.nodes.keys() {
        operations::validate_parents(&state, path)?;
    }
    let mut tail = [1; 1024];
    file.read_exact_at(&mut tail, file_len - 1024).map_err(io)?;
    if tail.iter().any(|b| *b != 0) {
        return Err(error(ErrorKind::CorruptJournal, "missing tar end blocks"));
    }
    Ok(state)
}
