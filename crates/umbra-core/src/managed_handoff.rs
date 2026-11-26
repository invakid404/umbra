//! One-shot provider connection handoff on an authenticated managed route.
use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

/// Try one nonblocking send. The role loop retains both endpoints until success
/// and enforces the original startup or cleanup deadline between attempts.
pub fn send(route: &UnixStream, provider: &UnixStream) -> io::Result<bool> {
    let fds = [provider.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff ancillary buffer too small",
        ));
    }
    match sendmsg(
        route,
        &[IoSlice::new(b"P")],
        &mut ancillary,
        SendFlags::DONTWAIT,
    ) {
        Ok(1) => Ok(true),
        Ok(_) => Err(io::ErrorKind::WriteZero.into()),
        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Try one nonblocking receive; malformed messages close every received right.
pub fn receive(route: &UnixStream) -> io::Result<Option<UnixStream>> {
    let mut marker = [0u8];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let message = match recvmsg(
        route,
        &mut [IoSliceMut::new(&mut marker)],
        &mut ancillary,
        RecvFlags::DONTWAIT,
    ) {
        Ok(message) => message,
        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut received = Vec::new();
    for item in ancillary.drain() {
        match item {
            RecvAncillaryMessage::ScmRights(fds) => received.extend(fds),
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
    }
    // Darwin returns MSG_DONTWAIT in msg_flags. It is our requested mode,
    // not truncation; every other returned flag remains a refusal.
    let unexpected_flags = message.flags.bits() & !RecvFlags::DONTWAIT.bits();
    if message.bytes != 1 || marker != *b"P" || unexpected_flags != 0 || received.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff bytes={} marker={marker:?} flags={:?} descriptors={}",
                message.bytes,
                message.flags,
                received.len()
            ),
        ));
    }
    let fd = received.pop().unwrap();
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
    if rustix::net::sockopt::socket_type(&fd)? != rustix::net::SocketType::STREAM
        || rustix::net::getsockname(&fd)?.address_family() != rustix::net::AddressFamily::UNIX
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // A listening Unix socket also reports STREAM. Require an established
    // connection; unnamed socketpair peers need not have an address.
    let _ = rustix::net::getpeername(&fd)?;
    crate::cost_snapshot!("provider-handoff-received", None, &[]);
    Ok(Some(UnixStream::from(fd)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::BorrowedFd;

    fn send_unchecked(route: &UnixStream, fds: &[BorrowedFd<'_>], marker: u8) {
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(fds)));
        assert_eq!(
            sendmsg(
                route,
                &[IoSlice::new(&[marker])],
                &mut ancillary,
                SendFlags::empty()
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn handoff_rejects_unconnected_listener() {
        let path = std::path::Path::new("/tmp").join(format!("handoff-{}", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (route, receiver) = UnixStream::pair().unwrap();
        send_unchecked(&route, &[listener.as_fd()], b'P');
        let result = receive(&receiver);
        std::fs::remove_file(path).unwrap();
        assert!(
            result.is_err(),
            "listening socket adopted as provider connection"
        );
    }

    #[test]
    fn handoff_rejects_wrong_marker_and_closes_received_descriptor() {
        let (route, receiver) = UnixStream::pair().unwrap();
        let (provider, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        send_unchecked(&route, &[provider.as_fd()], b'Q');
        drop(provider);
        assert!(receive(&receiver).is_err());
        assert_eq!(
            peer.read(&mut [0]).unwrap(),
            0,
            "rejected descriptor leaked"
        );
    }

    #[test]
    fn handoff_rejects_multiple_descriptors_and_closes_every_copy() {
        let (route, receiver) = UnixStream::pair().unwrap();
        let (first, mut first_peer) = UnixStream::pair().unwrap();
        let (second, mut second_peer) = UnixStream::pair().unwrap();
        first_peer.set_nonblocking(true).unwrap();
        second_peer.set_nonblocking(true).unwrap();
        send_unchecked(&route, &[first.as_fd(), second.as_fd()], b'P');
        drop((first, second));
        assert!(receive(&receiver).is_err());
        for peer in [&mut first_peer, &mut second_peer] {
            assert_eq!(
                peer.read(&mut [0]).unwrap(),
                0,
                "rejected descriptor leaked"
            );
        }
    }

    #[test]
    fn handoff_rejects_missing_and_non_socket_descriptors() {
        let (mut route, receiver) = UnixStream::pair().unwrap();
        route.write_all(b"P").unwrap();
        assert!(receive(&receiver).is_err());
        let file = std::fs::File::open("/dev/null").unwrap();
        send_unchecked(&route, &[file.as_fd()], b'P');
        assert!(receive(&receiver).is_err());
    }

    #[test]
    fn handoff_preserves_connection_and_closes_sender_reference() {
        let (route, receiver) = UnixStream::pair().unwrap();
        let (provider, mut peer) = UnixStream::pair().unwrap();
        assert!(send(&route, &provider).unwrap());
        let mut adopted = receive(&receiver).unwrap().unwrap();
        drop(provider);
        assert!(rustix::io::fcntl_getfd(&adopted)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC));
        peer.write_all(b"ok").unwrap();
        let mut bytes = [0; 2];
        adopted.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
        drop(adopted);
        assert_eq!(peer.read(&mut bytes).unwrap(), 0);
    }
}
