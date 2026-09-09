//! Smoke test: does the raw transport actually talk NFSv4.0 to a real server?
//!
//! Skipped unless `UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names a live fixture,
//! so an ordinary `cargo test` needs no server. This suite never mounts
//! anything: it speaks the protocol from userspace through `LibnfsRawTransport`.
#![cfg(feature = "transport-raw")]

use umbra_storage_nfs_userspace::transport::raw::{LibnfsRawTransport, RawTransportConfig};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, Deadline, RawTransport, WireProfile,
};

fn fixture() -> Option<RawTransportConfig> {
    let target = std::env::var("UMBRA_NFS_RAW_FIXTURE").ok()?;
    let (host, port) = target.rsplit_once(':')?;
    let mut config = RawTransportConfig::loopback(port.parse().ok()?);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 5_000 };
    Some(config)
}

#[test]
fn the_transport_reads_the_export_root_and_resolves_a_component() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };

    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    assert_eq!(transport.wire_profile(), WireProfile::V40_TCP_SYS);

    let deadline = Deadline { millis: 5_000 };
    let root = transport
        .root_filehandle(deadline)
        .expect("PUTROOTFH; GETFH");
    assert!(!root.as_bytes().is_empty());

    let export = ComponentName::new(b"export".to_vec()).unwrap();
    let (export_fh, attrs) = transport
        .lookup(&root, &export, AttrMask::STAT, deadline)
        .expect("LOOKUP export");
    assert_ne!(export_fh.as_bytes(), root.as_bytes());
    assert_eq!(
        attrs.file_type,
        Some(umbra_storage_nfs_userspace::transport::Nfs4Type::Directory)
    );
    assert!(attrs.fileid.is_some(), "identity needs a fileid");
    assert!(attrs.fsid.is_some(), "identity needs an fsid");

    let probe = ComponentName::new(b"probe".to_vec()).unwrap();
    let (probe_fh, _) = transport
        .lookup(&export_fh, &probe, AttrMask::STAT, deadline)
        .expect("LOOKUP probe");

    let read_name = ComponentName::new(b"read.txt".to_vec()).unwrap();
    let (file_fh, file_attrs) = transport
        .lookup(&probe_fh, &read_name, AttrMask::STAT, deadline)
        .expect("LOOKUP read.txt");
    assert_eq!(file_attrs.size, Some(20));

    let read = transport
        .read(
            &file_fh,
            umbra_storage_nfs_userspace::handle::Stateid::ANONYMOUS,
            0,
            64,
            deadline,
        )
        .expect("READ with the anonymous stateid");
    assert_eq!(read.data, b"hello-umbra-raw-rpc\n");
    assert!(read.eof);
}

#[test]
fn a_bounded_readdir_page_returns_entries_and_a_cookie_verifier() {
    let Some(config) = fixture() else {
        eprintln!("skipped: UMBRA_NFS_RAW_FIXTURE is unset");
        return;
    };
    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    let deadline = Deadline { millis: 5_000 };

    let root = transport.root_filehandle(deadline).unwrap();
    let export = ComponentName::new(b"export".to_vec()).unwrap();
    let (export_fh, _) = transport
        .lookup(&root, &export, AttrMask::IDENTITY, deadline)
        .unwrap();

    let page = transport
        .readdir(
            &export_fh,
            umbra_storage_nfs_userspace::transport::ReadDirRequest {
                cookie: umbra_storage_nfs_userspace::transport::DirCookie(0),
                verifier: umbra_storage_nfs_userspace::transport::DirVerifier([0; 8]),
                dir_count: 4096,
                max_count: 8192,
                attrs: AttrMask::IDENTITY,
            },
            deadline,
        )
        .expect("READDIR one page");

    let names: Vec<&[u8]> = page.entries.iter().map(|e| e.name.as_bytes()).collect();
    assert!(
        names.contains(&b"probe".as_slice()),
        "expected the seeded directory, saw {names:?}"
    );
}
