//! End-to-end checks of `webdep --log-relay` (Linux/Unix only): the relay keeps an app running
//! while nobody reads it, holds its output in memory, hands it to the next reader, and reports
//! the exit status.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wd-relay-{}-{name}.sock", std::process::id()))
}

fn start(sock: &Path, script: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_webdep"))
        .args(["--log-relay", sock.to_str().unwrap(), "--", "sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start relay")
}

fn connect(sock: &Path) -> BufReader<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(sock) {
            Ok(s) => {
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                return BufReader::new(s);
            }
            Err(e) if Instant::now() > deadline => panic!("relay did not listen: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn next(r: &mut BufReader<UnixStream>) -> serde_json::Value {
    let mut line = String::new();
    r.read_line(&mut line).expect("read from relay");
    serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("bad message {line:?}: {e}"))
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(Instant::now() < deadline, "relay did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn output_written_while_nobody_reads_reaches_the_next_reader() {
    let sock = socket("away");
    let mut relay = start(&sock, "echo one; echo two >&2; sleep 1; echo three; sleep 1; exit 3");

    let mut r = connect(&sock);
    assert_eq!(next(&mut r)["t"], "hello");
    let a = next(&mut r);
    let b = next(&mut r);
    let mut got = vec![(a["t"].clone(), a["l"].clone()), (b["t"].clone(), b["l"].clone())];
    got.sort_by_key(|(t, _)| t.to_string());
    assert_eq!(got, vec![("err".into(), "two".into()), ("out".into(), "one".into())]);

    // The panel goes away; the app keeps going and "three" waits in the relay.
    drop(r);
    std::thread::sleep(Duration::from_millis(1500));

    let mut r = connect(&sock);
    assert_eq!(next(&mut r)["t"], "hello");
    let line = next(&mut r);
    assert_eq!((line["t"].as_str(), line["l"].as_str()), (Some("out"), Some("three")));
    let exit = next(&mut r);
    assert_eq!(exit["t"], "exit");
    assert_eq!(exit["code"], 3);

    assert_eq!(wait_exit(&mut relay).code(), Some(3), "the relay exits with the app's code");
    assert!(!sock.exists(), "the socket is removed when the relay exits");
}

#[test]
fn sigterm_does_not_stop_the_relay_or_the_app() {
    let sock = socket("term");
    let mut relay = start(&sock, "sleep 1; echo still-here");
    let mut r = connect(&sock);
    assert_eq!(next(&mut r)["t"], "hello");

    // What `start.sh --stop` or a panel restart might send.
    unsafe {
        libc::kill(relay.id() as libc::pid_t, libc::SIGTERM);
    }
    let line = next(&mut r);
    assert_eq!(line["l"], "still-here");
    assert_eq!(next(&mut r)["t"], "exit");
    assert_eq!(wait_exit(&mut relay).code(), Some(0));
}

#[test]
fn a_new_reader_replaces_the_old_one() {
    let sock = socket("swap");
    let mut relay = start(&sock, "sleep 1; echo hi");
    let mut first = connect(&sock);
    assert_eq!(next(&mut first)["t"], "hello");
    let mut second = connect(&sock);
    assert_eq!(next(&mut second)["t"], "hello");
    assert_eq!(next(&mut second)["l"], "hi");
    assert_eq!(next(&mut second)["t"], "exit");
    assert_eq!(wait_exit(&mut relay).code(), Some(0));
}
