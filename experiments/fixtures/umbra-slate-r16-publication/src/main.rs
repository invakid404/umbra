//! Codex 44fe510 rollout/src/compression.rs:153-157,183-189 publication only.
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
const SOURCE: &[u8] = b"C16 complete temporary payload\n";
fn seed(path: &Path, bytes: &[u8]) -> io::Result<fs::File> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(file)
}
fn persist(src: &Path, dst: &Path) -> io::Result<()> {
    eprintln!("C16 fallback-enter");
    let temporary = tempfile::TempPath::try_from_path(src)?;
    match temporary.persist_noclobber(dst) {
        Ok(()) => {
            eprintln!("C16 persist-return Ok");
            Ok(())
        }
        Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => {
            eprintln!("C16 persist-return AlreadyExists");
            Ok(())
        }
        Err(err) => {
            eprintln!("C16 persist-return {:?}", err.error);
            Err(err.error)
        }
    }
}
fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    assert_eq!(args.len(), 3);
    let mode: u8 = args[1].to_str().unwrap().parse().unwrap();
    assert!((1..=5).contains(&mode));
    let root = Path::new(&args[2]);
    let src = root.join("src");
    let dst = root.join("dst");
    let source = seed(&src, SOURCE)?;
    if mode == 5 {
        eprintln!("C16 drop-enter");
        io::stderr().flush()?;
        drop(source);
        eprintln!("C16 drop-return Ok");
        eprintln!("C16 decision-return Ok");
        io::stderr().flush()?;
        std::process::exit(0);
    }
    // Keep the source open across publication, matching Codex compression.rs.
    // Incumbents are supplied by the harness; no routed marker write/drop.

    eprintln!("C16 ready mode={mode} pid={}", std::process::id());
    if mode <= 2 {
        eprintln!("C16 link-enter");
        match fs::hard_link(&src, &dst) {
            Ok(()) => eprintln!("C16 link-return Ok"),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                eprintln!("C16 link-return AlreadyExists")
            }
            Err(err) => {
                eprintln!("C16 link-return {err:?}");
                persist(&src, &dst)?;
            }
        }
    } else {
        persist(&src, &dst)?;
    }
    eprintln!("C16 decision-return Ok");
    io::stderr().flush()?;
    // Deliberately avoid File teardown (#227) after observing publication.
    std::process::exit(0);
}
