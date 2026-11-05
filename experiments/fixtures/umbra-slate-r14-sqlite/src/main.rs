use serde_json::{Value, json};
use sqlx::{
    ConnectOptions,
    sqlite::{
        SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions,
        SqliteSynchronous,
    },
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

struct Observer {
    socket: TcpStream,
    nonce: String,
    seq: u64,
    end: Instant,
}
impl Observer {
    fn send(&mut self, kind: &str, phase: &str, data: Value) {
        let end = self.end.min(Instant::now() + Duration::from_secs(3));
        self.seq += 1;
        let frame =
            json!({"nonce":self.nonce,"seq":self.seq,"kind":kind,"phase":phase,"data":data});
        let bytes = serde_json::to_vec(&frame).unwrap();
        assert!(bytes.len() <= 65536);
        let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
        frame.extend(bytes);
        let mut left = frame.as_slice();
        while !left.is_empty() {
            let budget = end.saturating_duration_since(Instant::now());
            assert!(!budget.is_zero());
            self.socket.set_write_timeout(Some(budget)).unwrap();
            match self.socket.write(left) {
                Ok(0) => panic!("write zero"),
                Ok(n) => left = &left[n..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => panic!("{e}"),
            }
        }
        let mut size = [0; 4];
        self.read(&mut size, end);
        let n = u32::from_be_bytes(size) as usize;
        assert!(n <= 65536);
        let mut bytes = vec![0; n];
        self.read(&mut bytes, end);
        let ack: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack, json!({"nonce":self.nonce,"seq":self.seq,"kind":"ACK"}));
        assert!(Instant::now() < self.end, "apparatus deadline");
    }
    fn read(&mut self, mut bytes: &mut [u8], end: Instant) {
        while !bytes.is_empty() {
            let budget = end.saturating_duration_since(Instant::now());
            assert!(!budget.is_zero());
            self.socket.set_read_timeout(Some(budget)).unwrap();
            match self.socket.read(bytes) {
                Ok(0) => panic!("EOF awaiting ACK"),
                Ok(n) => bytes = &mut bytes[n..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => panic!("{e}"),
            }
        }
    }
    fn before(&mut self, phase: &str) {
        self.send("BEFORE", phase, Value::Null);
    }
    fn result<T: std::fmt::Debug>(
        &mut self,
        phase: &str,
        value: Result<T, sqlx::Error>,
    ) -> Result<T, ()> {
        match value {
            Ok(v) => {
                self.send("AFTER", phase, json!({"result":format!("{v:?}")}));
                Ok(v)
            }
            Err(e) => {
                let code = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .map(|v| v.into_owned());
                self.send(
                    "ERROR",
                    phase,
                    json!({"variant":format!("{e:?}"),"message":e.to_string(),"code":code}),
                );
                Err(())
            }
        }
    }
}
async fn run(o: &mut Observer, path: &str, mode: &str) -> Result<(), ()> {
    let mut options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .auto_vacuum(SqliteAutoVacuum::Incremental)
        .busy_timeout(Duration::from_secs(5))
        .log_statements(log::LevelFilter::Off);
    if mode == "verify" {
        options = SqliteConnectOptions::new().filename(path).read_only(true);
    }
    o.before("connect");
    let pool = o.result(
        "connect",
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await,
    )?;
    let result = async {
        let mut connection = o.result("acquire", pool.acquire().await)?;
        let mut handle = match connection.lock_handle().await {
            Ok(v) => v,
            Err(e) => {
                o.result::<()>("lock-handle", Err(e))?;
                unreachable!()
            }
        };
        let mut name: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = unsafe {
            libsqlite3_sys::sqlite3_file_control(
                handle.as_raw_handle().as_ptr(),
                c"main".as_ptr(),
                libsqlite3_sys::SQLITE_FCNTL_VFSNAME,
                (&mut name as *mut *mut std::ffi::c_char).cast(),
            )
        };
        let vfs = if name.is_null() {
            None
        } else {
            Some(
                unsafe { std::ffi::CStr::from_ptr(name) }
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        unsafe { libsqlite3_sys::sqlite3_free(name.cast()) };
        o.send("AFTER", "vfs", json!({"rc":rc,"name":vfs}));
        drop(handle);
        drop(connection);
        for (pragma, statement) in [
            ("journal_mode", "PRAGMA journal_mode"),
            ("synchronous", "PRAGMA synchronous"),
            ("auto_vacuum", "PRAGMA auto_vacuum"),
            ("busy_timeout", "PRAGMA busy_timeout"),
            ("compile_options", "PRAGMA compile_options"),
        ] {
            o.before(pragma);
            let rows = sqlx::query(statement).fetch_all(&pool).await;
            // Preserve typed values rather than relying on SqliteRow's Debug output.
            use sqlx::Row;
            let rows = rows
                .map(|rows| {
                    rows.iter()
                        .map(|r| {
                            r.try_get::<String, _>(0)
                                .or_else(|_| r.try_get::<i64, _>(0).map(|v| v.to_string()))
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .and_then(|x| x);
            o.result(pragma, rows)?;
        }
        o.before("version");
        o.result(
            "version",
            sqlx::query_scalar::<_, String>("SELECT sqlite_version()")
                .fetch_one(&pool)
                .await,
        )?;
        if mode != "verify" {
            o.before("schema");
            o.result(
                "schema",
                sqlx::query(
                    "CREATE TABLE IF NOT EXISTS slate_rows (value TEXT PRIMARY KEY NOT NULL)",
                )
                .execute(&pool)
                .await,
            )?;
            o.before("initial-rows");
            o.result(
                "initial-rows",
                sqlx::query_scalar::<_, String>(
                    "SELECT value FROM slate_rows ORDER BY value COLLATE BINARY",
                )
                .fetch_all(&pool)
                .await,
            )?;
            for (phase, statement) in [
                ("begin", "BEGIN"),
                ("insert", "INSERT INTO slate_rows(value) VALUES (?)"),
                ("commit", "COMMIT"),
            ] {
                o.before(phase);
                let q = sqlx::query(statement);
                let q = if phase == "insert" {
                    q.bind(if mode == "seed" {
                        "r14-seed"
                    } else {
                        "r14-new"
                    })
                } else {
                    q
                };
                o.result(phase, q.execute(&pool).await)?;
            }
        }
        o.before("rows");
        o.result(
            "rows",
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM slate_rows ORDER BY value COLLATE BINARY",
            )
            .fetch_all(&pool)
            .await,
        )?;
        o.before("integrity");
        o.result(
            "integrity",
            sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
                .fetch_all(&pool)
                .await,
        )?;
        Ok(())
    }
    .await;
    o.before("close");
    pool.close().await;
    o.send("AFTER", "close", Value::Null);
    result
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 5);
    let address: SocketAddr = args[3].parse().unwrap();
    assert!(address.ip().is_loopback());
    let socket = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    let mut o = Observer {
        socket,
        nonce: args[4].clone(),
        seq: 0,
        end: Instant::now() + Duration::from_secs(45),
    };
    o.send(
        "HELLO",
        "ready",
        json!({"version":1,"pid":std::process::id()}),
    );
    o.send("PING", "ready", json!("r14-ping"));
    match args[1].as_str() {
        "qualify-disconnect" => {
            o.before("synthetic");
            return;
        }
        "qualify-error" => {
            o.send("ERROR", "synthetic", json!({"synthetic":true}));
        }
        "qualify-ok" => {}
        "fresh" | "existing" | "seed" | "verify" => {
            let project = std::path::Path::new(&args[2])
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("project");
            std::env::set_current_dir(project).unwrap();
            let version = unsafe { std::ffi::CStr::from_ptr(libsqlite3_sys::sqlite3_libversion()) }
                .to_string_lossy()
                .into_owned();
            let mut compile = Vec::new();
            let mut i = 0;
            loop {
                let p = unsafe { libsqlite3_sys::sqlite3_compileoption_get(i) };
                if p.is_null() {
                    break;
                }
                compile.push(
                    unsafe { std::ffi::CStr::from_ptr(p) }
                        .to_string_lossy()
                        .into_owned(),
                );
                i += 1;
            }
            o.send(
                "AFTER",
                "engine",
                json!({"version":version,"compile_options":compile,"fixture":"slate-r14-v1"}),
            );
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = rt.block_on(run(&mut o, &args[2], &args[1]));
            o.send("TERMINAL", "done", json!({"ok":result.is_ok()}));
            drop(rt);
            if result.is_err() {
                std::process::exit(20);
            }
            return;
        }
        _ => panic!("unknown mode"),
    }
    o.send("TERMINAL", "done", json!({"ok":true}));
}
