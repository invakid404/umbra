//! Owned host stdio for one platform launch, never persisted or path-reopened.
use crate::{ErrorKind, Result, UmbraError};
use rustix::net::{
    recvmsg, sendmsg, AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketType,
};
use std::fs::{File, OpenOptions};
use std::io::{IoSlice, IoSliceMut, IsTerminal};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;

/// Negotiated capability for the platform's one-shot stdio adoption request.
pub const CAPABILITY: &str = "launch-stdio-v1";

fn io(error: impl std::fmt::Display) -> UmbraError {
    UmbraError::new(ErrorKind::Io, "launch.stdio", error.to_string())
}

/// Three explicitly owned descriptions. Dropping this value closes all copies.
#[derive(Debug)]
pub struct LaunchStdio {
    fds: [OwnedFd; 3],
}

impl LaunchStdio {
    /// Capture current stdio before providers allocate handles. Rust startup
    /// sanitizes closed standard slots on supported macOS hosts.
    pub fn capture() -> Result<Self> {
        Self::new([
            std::io::stdin().as_fd().try_clone_to_owned().map_err(io)?,
            std::io::stdout().as_fd().try_clone_to_owned().map_err(io)?,
            std::io::stderr().as_fd().try_clone_to_owned().map_err(io)?,
        ])
    }

    /// Admit owned tty, pipe/FIFO, Unix stream, regular-file and null endpoints.
    pub fn new(fds: [OwnedFd; 3]) -> Result<Self> {
        let null = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open("/dev/null")
            .and_then(|file| file.metadata())
            .map_err(|error| {
                UmbraError::new(
                    ErrorKind::UnsupportedCapability,
                    "null-device-identity-unavailable",
                    error.to_string(),
                )
            })?;
        if !null.file_type().is_char_device() {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "null-device-identity-unavailable",
                "null reference is not a character device",
            ));
        }
        for fd in &fds {
            let metadata = File::from(fd.try_clone().map_err(io)?)
                .metadata()
                .map_err(io)?;
            let kind = metadata.file_type();
            let allowed = kind.is_file()
                || kind.is_fifo()
                || (kind.is_char_device() && (fd.is_terminal() || metadata.rdev() == null.rdev()))
                || (kind.is_socket()
                    && rustix::net::sockopt::socket_type(fd).map_err(io)? == SocketType::STREAM
                    && rustix::net::getsockname(fd).map_err(io)?.address_family()
                        == AddressFamily::UNIX);
            if !allowed {
                return Err(UmbraError::new(
                    ErrorKind::UnsupportedCapability,
                    "stdio-endpoint-kind",
                    "inherited stdio endpoint kind is not supported",
                ));
            }
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).map_err(io)?;
        }
        Ok(Self { fds })
    }

    /// Borrow descriptions for root spawn actions; no ambient fd lookup.
    pub fn descriptors(&self) -> &[OwnedFd; 3] {
        &self.fds
    }

    /// Transfer references on the already authenticated platform IPC stream.
    /// A fixed one-byte marker keeps this ancillary message separate from JSON.
    pub(crate) fn send(&self, stream: &UnixStream) -> Result<()> {
        let fds: Vec<_> = self.fds.iter().map(AsFd::as_fd).collect();
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
            return Err(io("stdio ancillary buffer too small"));
        }
        let sent = sendmsg(
            stream,
            &[IoSlice::new(b"S")],
            &mut ancillary,
            SendFlags::empty(),
        )
        .map_err(io)?;
        if sent != 1 {
            return Err(io("incomplete stdio transfer"));
        }
        Ok(())
    }

    /// Receive and validate one ownership transfer. All received handles are
    /// closed on an invalid message or endpoint; acknowledgement comes later.
    pub(crate) fn receive(stream: &UnixStream) -> Result<Self> {
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let mut marker = [0];
        let message = recvmsg(
            stream,
            &mut [IoSliceMut::new(&mut marker)],
            &mut ancillary,
            RecvFlags::empty(),
        )
        .map_err(io)?;
        let mut fds = Vec::new();
        for item in ancillary.drain() {
            match item {
                RecvAncillaryMessage::ScmRights(rights) => fds.extend(rights),
                _ => return Err(io("unexpected stdio ancillary data")),
            }
        }
        if message.bytes != 1 || marker != *b"S" || !message.flags.is_empty() || fds.len() != 3 {
            return Err(io("invalid stdio transfer"));
        }
        Self::new(
            fds.try_into()
                .map_err(|_| io("invalid stdio descriptor count"))?,
        )
    }

    /// Release a standalone CLI's original stdin/stdout references after the
    /// platform ACK. Null replacements avoid leaving Rust's standard handles
    /// dangling or allowing later sockets to reuse the numbers. stderr stays.
    pub fn release_cli_streams() -> Result<()> {
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .map_err(io)?;
        rustix::stdio::dup2_stdin(&null).map_err(io)?;
        rustix::stdio::dup2_stdout(&null).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn transfer_preserves_open_description_and_fences_received_handles() {
        let mut original = tempfile::tempfile().unwrap();
        original.write_all(b"abcdef").unwrap();
        original.seek(SeekFrom::Start(2)).unwrap();
        let stdio = LaunchStdio::new(std::array::from_fn(|_| {
            original.as_fd().try_clone_to_owned().unwrap()
        }))
        .unwrap();
        let (sender, receiver) = UnixStream::pair().unwrap();
        stdio.send(&sender).unwrap();
        let adopted = LaunchStdio::receive(&receiver).unwrap();
        drop(stdio);
        for fd in adopted.descriptors() {
            assert!(rustix::io::fcntl_getfd(fd)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC));
        }
        let mut received = File::from(adopted.fds.into_iter().next().unwrap());
        let mut byte = [0];
        received.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"c");
        original.read_exact(&mut byte).unwrap();
        assert_eq!(
            byte, *b"d",
            "transfer must share the offset, not reopen the path"
        );
    }

    #[test]
    fn malformed_transfer_drops_all_received_writer_copies() {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let (sender, receiver) = UnixStream::pair().unwrap();
        let fds = [writer.as_fd(), writer.as_fd(), writer.as_fd()];
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(&fds)));
        sendmsg(
            &sender,
            &[IoSlice::new(b"!")],
            &mut ancillary,
            SendFlags::empty(),
        )
        .unwrap();
        assert!(LaunchStdio::receive(&receiver).is_err());
        drop(writer);
        let mut reader = reader;
        assert_eq!(
            reader.read(&mut [0]).unwrap(),
            0,
            "invalid transfer retained a writer"
        );
    }
}
