//! Qualification-only observations at the real debugger fork/exec boundary.
use std::{
    cell::Cell,
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    thread::JoinHandle,
};

const MAGIC: u64 = 0x554d425241464453;
const MAX_ENTRIES: usize = 4096;
thread_local! {
    static NEXT: Cell<Option<(RawFd, u128)>> = const { Cell::new(None) };
}

/// One inherited parent socket, identified independently of its descriptor number.
#[derive(Clone, Debug)]
pub struct Socket {
    pub fd: RawFd,
    pub device: u32,
    pub inode: u64,
    pub mode: u16,
    pub retained_before_exec: bool,
    pub opaque_identity: u64,
}

/// Observations written before the real debugger executable starts.
#[derive(Debug)]
pub struct Report {
    pub pid: u32,
    pub sockets: Vec<Socket>,
}

/// Inspect actual process sockets through libproc and match parent identities.
/// An unreadable or truncated inventory is an error, never an empty result.
pub fn inherited_sockets(pid: u32, expected: &[Socket]) -> io::Result<Vec<RawFd>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::other("invalid process identity"));
    }
    let mut descriptors = [0u64; 1024];
    let size = std::mem::size_of_val(&descriptors);
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDLISTFDS,
            0,
            descriptors.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if count <= 0 || count as usize >= size || !(count as usize).is_multiple_of(8) {
        return Err(io::Error::other(
            "process descriptor inventory unavailable or truncated",
        ));
    }
    let mut inherited = Vec::new();
    for index in 0..count as usize / 8 {
        // SDK sys/proc_info.h: proc_fdinfo is two 32-bit fields.
        let entry = unsafe {
            std::ptr::read_unaligned(descriptors.as_ptr().cast::<libc::proc_fdinfo>().add(index))
        };
        if entry.proc_fdtype != libc::PROX_FDTYPE_SOCKET as u32 {
            continue;
        }
        let identity = socket_identity(pid, entry.proc_fd)?;
        if expected
            .iter()
            .any(|socket| socket.opaque_identity == identity)
        {
            inherited.push(entry.proc_fd);
        }
    }
    Ok(inherited)
}

/// Arms one debugger spawn on the current native-owner thread.
pub struct Probe {
    nonce: u128,
    writer: Option<File>,
    reader: Option<JoinHandle<io::Result<Report>>>,
    _affine: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Probe {
    pub fn new() -> io::Result<Self> {
        if NEXT.with(Cell::get).is_some() {
            return Err(io::Error::other("endpoint probe already armed"));
        }
        let mut fds = [0; 2];
        // SAFETY: two writable descriptors, immediately adopted below.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        for fd in fds {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let nonce = umbra_core::shutdown::Nonce::new_v4().as_u128();
        let task = std::thread::Builder::new()
            .name("endpoint-inspection".into())
            .spawn(move || read_report(reader, nonce))?;
        NEXT.with(|next| next.set(Some((writer.as_raw_fd(), nonce))));
        Ok(Self {
            nonce,
            writer: Some(writer),
            reader: Some(task),
            _affine: std::marker::PhantomData,
        })
    }

    /// Call after native setup/teardown returns, so all child write ends are gone.
    pub fn finish(mut self) -> io::Result<Report> {
        self.disarm();
        self.reader
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("endpoint inspection thread panicked"))?
    }

    fn disarm(&mut self) {
        NEXT.with(|next| {
            if next.get().is_some_and(|(_, nonce)| nonce == self.nonce) {
                next.set(None);
            }
        });
        self.writer.take();
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.disarm();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

unsafe extern "C" {
    fn umbra_probe_socket_identity(pid: i32, fd: i32, identity: *mut u64) -> i32;
}

fn socket_identity(pid: u32, fd: RawFd) -> io::Result<u64> {
    let mut identity = 0;
    if unsafe { umbra_probe_socket_identity(pid as i32, fd, &mut identity) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(identity)
}

fn read_words<const N: usize>(reader: &mut File) -> io::Result<[u64; N]> {
    let mut words = [0u64; N];
    for word in &mut words {
        let mut bytes = [0u8; 8];
        reader.read_exact(&mut bytes)?;
        *word = u64::from_ne_bytes(bytes);
    }
    Ok(words)
}

fn read_report(mut reader: File, nonce: u128) -> io::Result<Report> {
    let header = read_words::<5>(&mut reader)?;
    if header[0] != MAGIC
        || header[1] != (nonce >> 64) as u64
        || header[2] != nonce as u64
        || header[3] == 0
        || header[3] > u32::MAX as u64
        || header[4] > MAX_ENTRIES as u64
    {
        return Err(io::Error::other("invalid endpoint inspection header"));
    }
    let mut sockets = Vec::new();
    for _ in 0..header[4] {
        let words = read_words::<6>(&mut reader)?;
        if words[0] > i32::MAX as u64
            || words[1] > u32::MAX as u64
            || words[3] > u16::MAX as u64
            || words[4] > 1
            || words[5] == 0
        {
            return Err(io::Error::other("invalid endpoint inspection record"));
        }
        sockets.push(Socket {
            fd: words[0] as RawFd,
            device: words[1] as u32,
            inode: words[2],
            mode: words[3] as u16,
            retained_before_exec: words[4] == 1,
            opaque_identity: words[5],
        });
    }
    Ok(Report {
        pid: header[3] as u32,
        sockets,
    })
}

pub(crate) struct Prepared {
    fd: RawFd,
    header: [u64; 5],
    sockets: Vec<[u64; 6]>,
}

pub(crate) fn prepare() -> io::Result<Option<Prepared>> {
    let Some((fd, nonce)) = NEXT.with(Cell::take) else {
        return Ok(None);
    };
    let mut sockets = Vec::new();
    for candidate in 3..unsafe { libc::getdtablesize() } {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(candidate, stat.as_mut_ptr()) } != 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT == libc::S_IFSOCK {
            sockets.push([
                candidate as u64,
                stat.st_dev as u32 as u64,
                stat.st_ino,
                stat.st_mode as u64,
                0,
                socket_identity(std::process::id(), candidate)?,
            ]);
        }
    }
    if sockets.len() > MAX_ENTRIES {
        return Err(io::Error::other("endpoint inspection inventory too large"));
    }
    Ok(Some(Prepared {
        fd,
        header: [
            MAGIC,
            (nonce >> 64) as u64,
            nonce as u64,
            0,
            sockets.len() as u64,
        ],
        sockets,
    }))
}

impl Prepared {
    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }

    /// Only preallocated buffers and async-signal-safe calls in the fork child.
    pub(crate) unsafe fn inspect(&mut self) -> io::Result<()> {
        self.header[3] = unsafe { libc::getpid() } as u64;
        unsafe { self.write(&self.header)? };
        for record in &mut self.sockets {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let result = unsafe { libc::fstat(record[0] as RawFd, stat.as_mut_ptr()) };
            if result == 0 {
                let stat = unsafe { stat.assume_init() };
                record[4] = u64::from(
                    stat.st_dev as u32 as u64 == record[1]
                        && stat.st_ino == record[2]
                        && stat.st_mode as u64 == record[3],
                );
            } else if io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
                return Err(io::Error::last_os_error());
            }
            // One fixed-size record is smaller than PIPE_BUF; its reader runs
            // independently in the parent while Command::spawn waits for exec.
            if unsafe { libc::write(self.fd, record.as_ptr().cast(), 48) } != 48 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    unsafe fn write(&self, words: &[u64; 5]) -> io::Result<()> {
        if unsafe { libc::write(self.fd, words.as_ptr().cast(), 40) } != 40 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
