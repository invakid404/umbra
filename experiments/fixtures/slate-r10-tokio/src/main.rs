use std::{
    collections::BTreeSet,
    ffi::CString,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};
unsafe extern "C" {
    fn pthread_threadid_np(thread: usize, tid: *mut u64) -> i32;
    fn __error() -> *mut i32;
    fn open(path: *const i8, flags: i32, ...) -> i32;
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn close(fd: i32) -> i32;
}
fn tid() -> u64 {
    let mut id = 0;
    assert_eq!(unsafe { pthread_threadid_np(0, &mut id) }, 0);
    id
}
fn call(op: &str, fd: i32, report: &mut String, f: impl FnOnce() -> isize) -> isize {
    unsafe { *__error() = 0 };
    let result = f();
    let errno = unsafe { *__error() };
    writeln!(report, "{op} {result} tid={} fd={fd} errno={errno}", tid()).unwrap();
    assert!(result >= 0, "{op}: {errno}");
    result
}
fn sync_io(input: &Path, output: &Path, report: &mut String) {
    use std::os::unix::ffi::OsStrExt;
    let input = CString::new(input.as_os_str().as_bytes()).unwrap();
    let output = CString::new(output.as_os_str().as_bytes()).unwrap();
    let fd = call("open-input", -1, report, || unsafe {
        open(input.as_ptr(), 0) as isize
    }) as i32;
    let mut bytes = [0u8; 6];
    let mut offset = 0;
    while offset < 6 {
        let n = call("read", fd, report, || unsafe {
            read(fd, bytes[offset..].as_mut_ptr(), 6 - offset)
        }) as usize;
        assert!(n > 0);
        offset += n;
    }
    assert_eq!(&bytes, b"ABCDEF");
    assert_eq!(
        call("close", fd, report, || unsafe { close(fd) as isize }),
        0
    );
    let fd = call("open-output", -1, report, || unsafe {
        open(output.as_ptr(), 0xa01, 0o600) as isize
    }) as i32;
    let bytes = b"RUNTIME";
    let mut offset = 0;
    while offset < bytes.len() {
        let n = call("write", fd, report, || unsafe {
            write(fd, bytes[offset..].as_ptr(), bytes.len() - offset)
        }) as usize;
        assert!(n > 0);
        offset += n;
    }
    assert_eq!(
        call("close", fd, report, || unsafe { close(fd) as isize }),
        0
    );
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert_eq!(args.len(), 5);
    let mode = args[1].to_str().unwrap().to_owned();
    assert!(mode == "c10-14" || mode == "c10-15");
    let input = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    let record = PathBuf::from(&args[4]);
    let main_tid = tid();
    let result=std::thread::spawn(move || {
        let owner=tid();assert_ne!(owner,main_tid);
        let live=Arc::new((Mutex::new(BTreeSet::new()),Condvar::new()));
        let starts=live.clone();let stops=live.clone();
        let rt=tokio::runtime::Builder::new_multi_thread().worker_threads(2)
            .on_thread_start(move || {let mut s=starts.0.lock().unwrap();s.insert(tid());starts.1.notify_all();})
            .on_thread_stop(move || {let mut s=stops.0.lock().unwrap();s.remove(&tid());stops.1.notify_all();})
            .build().unwrap();
        let mut active=live.0.lock().unwrap();
        while active.len()<2 {active=live.1.wait(active).unwrap();}
        let workers:Vec<_>=active.iter().copied().collect();assert_eq!(workers.len(),2);drop(active);
        let mut calls=String::new();
        if mode=="c10-14" {sync_io(&input,&output,&mut calls);} else {
            // Task-level Result only. No blocking-worker syscall errno is exposed.
            rt.block_on(async {
                writeln!(calls,"dispatch task=roundtrip operation=write path={:?} poll_tid={}",output,tid()).unwrap();
                let written=tokio::fs::write(&output,b"ASYNC").await;
                writeln!(calls,"result task=roundtrip operation=write value={written:?} poll_tid={}",tid()).unwrap();written.unwrap();
                let read=tokio::fs::read(&output).await;
                writeln!(calls,"result task=roundtrip operation=read value={read:?} poll_tid={}",tid()).unwrap();
                assert_eq!(read.unwrap(),b"ASYNC");
            });
        }
        // Runtime still exists. The acknowledged workers must not have exited.
        let active=live.0.lock().unwrap();for worker in &workers {assert!(active.contains(worker));}drop(active);
        let payload=if mode=="c10-14" {"RUNTIME"}else{"ASYNC"};
        let report=format!("phase=complete tid={owner} main_tid={main_tid} worker0={} worker1={} workers_ready=2 workers_still_alive=2 output={payload} length={}\n{calls}",workers[0],workers[1],payload.len());
        drop(rt);
        (record,report)
    }).join().expect("runtime owner joined");
    // Original thread lives throughout runtime work; records written after join.
    std::fs::write(result.0, format!("{}owner_joined=1\n", result.1)).unwrap();
}
