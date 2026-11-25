//! Log relay: keeps apps running, with their output captured, while the panel restarts.
//!
//! Without it an app's stdout/stderr are pipes into the panel process. When the panel restarts
//! those pipes break, so the panel has to restart every app to read its output again. On Unix the
//! panel instead starts each app through `webdep --log-relay <socket> -- sh -c <command>`: a small
//! process that owns the app's pipes and passes the lines on over a Unix socket in the app's
//! `run/` folder. The panel (or a restarted panel) connects to that socket and reads.
//!
//! The relay writes nothing to disk. Lines the panel has not received yet wait in memory, at most
//! [`QUEUE_LINES`] of them (the oldest are dropped and counted). After the app exits the relay
//! waits up to [`LINGER_AFTER_EXIT`] for the panel to collect the last lines and the exit status,
//! then exits with the app's exit code.
//!
//! Wire format, relay to panel, one JSON object per line (see [`Msg`]):
//! `hello` once per connection, then `out`/`err` lines, `dropped` counts and finally `exit`.
//!
//! `WEBDEP_LOG_RELAY=0` turns it off (apps are then piped straight into the panel, as before).
//! Windows always pipes straight into the panel.

#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;

/// The hidden command-line flag that runs this binary as a relay.
pub const FLAG: &str = "--log-relay";
pub const ENV_ENABLE: &str = "WEBDEP_LOG_RELAY";
/// Lines kept for the panel while it is away.
pub const QUEUE_LINES: usize = 1000;
/// Longer lines are cut (the rest arrives as the next line).
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// How long a relay waits after its app exited for the panel to collect the end of the output.
pub const LINGER_AFTER_EXIT: Duration = Duration::from_secs(120);
const PROTOCOL_VERSION: u32 = 1;
/// Unix socket paths are limited to about 108 bytes; stay well under it.
const MAX_SOCKET_PATH: usize = 100;

/// One message from the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Msg {
    Hello { v: u32, relay_pid: u32, app_pid: u32 },
    Out { l: String },
    Err { l: String },
    /// This many lines were dropped because the panel was away and the queue was full.
    Dropped { n: u64 },
    Exit { code: Option<i32>, signal: Option<i32> },
}

/// Whether apps should be started through the relay.
pub fn enabled() -> bool {
    cfg!(unix)
        && !std::env::var(ENV_ENABLE).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
}

/// Where an app's relay listens, or `None` when the relay is off or the path would be too long.
pub fn socket_path(run_dir: &Path) -> Option<PathBuf> {
    if !enabled() {
        return None;
    }
    let path = run_dir.join("relay.sock");
    (path.as_os_str().len() <= MAX_SOCKET_PATH).then_some(path)
}

/// This program, for starting relays. After an update the running binary may show as renamed or
/// deleted; the path the service starts is the one to use.
pub fn self_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("webdep"));
    crate::updater::normalize_exe_path(exe)
}

/// What the relay keeps for the panel: lines not delivered yet (oldest dropped past the limit),
/// how many were dropped, and the app's exit once it happened.
#[derive(Debug, Default)]
pub struct Queue {
    lines: VecDeque<Msg>,
    dropped: u64,
    exit: Option<Msg>,
}

impl Queue {
    pub fn push(&mut self, msg: Msg) {
        if self.lines.len() >= QUEUE_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(msg);
    }

    pub fn set_exit(&mut self, exit: Msg) {
        self.exit = Some(exit);
    }

    /// Everything waiting, in order; the exit goes last, and only once the lines before it are out.
    fn take(&mut self) -> Vec<Msg> {
        let mut out = Vec::with_capacity(self.lines.len() + 2);
        if self.dropped > 0 {
            out.push(Msg::Dropped { n: self.dropped });
            self.dropped = 0;
        }
        out.extend(self.lines.drain(..));
        if let Some(exit) = self.exit.clone() {
            out.push(exit);
        }
        out
    }

    /// Puts back messages that could not be sent, ahead of anything that arrived since.
    fn put_back(&mut self, msgs: Vec<Msg>) {
        for msg in msgs.into_iter().rev() {
            match msg {
                Msg::Dropped { n } => self.dropped += n,
                Msg::Exit { .. } => {}
                other => self.lines.push_front(other),
            }
        }
        while self.lines.len() > QUEUE_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
    }
}

/// Splits a byte stream into lines, cutting any line longer than [`MAX_LINE_BYTES`].
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                out.push(self.flush());
            } else {
                self.buf.push(b);
                if self.buf.len() >= MAX_LINE_BYTES {
                    out.push(self.flush());
                }
            }
        }
        out
    }

    /// The last line when the stream ends without a newline.
    pub fn finish(&mut self) -> Option<String> {
        (!self.buf.is_empty()).then(|| self.flush())
    }

    fn flush(&mut self) -> String {
        let line = String::from_utf8_lossy(&self.buf).trim_end_matches('\r').to_string();
        self.buf.clear();
        line
    }
}

/// Sends what the queue holds. `Ok(true)` once the exit went out. On a write error the messages
/// go back into the queue for the next connection.
async fn pump<W: AsyncWrite + Unpin>(queue: &Mutex<Queue>, writer: &mut W) -> std::io::Result<bool> {
    let batch = queue.lock().unwrap_or_else(|e| e.into_inner()).take();
    if batch.is_empty() {
        return Ok(false);
    }
    let done = matches!(batch.last(), Some(Msg::Exit { .. }));
    let mut text = String::new();
    for msg in &batch {
        text.push_str(&serde_json::to_string(msg).unwrap_or_default());
        text.push('\n');
    }
    if let Err(e) = async {
        writer.write_all(text.as_bytes()).await?;
        writer.flush().await
    }
    .await
    {
        queue.lock().unwrap_or_else(|e| e.into_inner()).put_back(batch);
        return Err(e);
    }
    Ok(done)
}

/// The exit status a relay reports, as an `ExitStatus` for the code that waits on apps.
pub fn exit_status(code: Option<i32>, signal: Option<i32>) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match (code, signal) {
            (_, Some(sig)) => ExitStatus::from_raw(sig & 0x7f),
            (Some(code), None) => ExitStatus::from_raw((code & 0xff) << 8),
            (None, None) => ExitStatus::from_raw(0),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        let _ = signal;
        ExitStatus::from_raw(code.unwrap_or(1) as u32)
    }
}

/// Where an app started through a relay reports how it ended.
pub type ExitSlot = Arc<Mutex<Option<ExitStatus>>>;

/// A running app as the panel tracks it: either a child of this panel (started directly or
/// through a relay), or an app found again after the panel restarted (only its relay's pid).
pub struct AppProcess {
    child: Option<Child>,
    pid: u32,
    exit: ExitSlot,
}

impl AppProcess {
    /// An app this panel started (the child is the app itself or its relay).
    pub fn started(child: Child, exit: ExitSlot) -> Self {
        let pid = child.id().unwrap_or(0);
        AppProcess { child: Some(child), pid, exit }
    }

    /// An app whose relay this panel reconnected to after a restart.
    pub fn reattached(relay_pid: u32, exit: ExitSlot) -> Self {
        AppProcess { child: None, pid: relay_pid, exit }
    }

    pub fn id(&self) -> Option<u32> {
        match &self.child {
            Some(c) => c.id(),
            None => (self.pid > 0).then_some(self.pid),
        }
    }

    /// Like `Child::try_wait`. A reattached app counts as ended once its relay reported the exit,
    /// or (an error) when the relay disappeared without saying.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let reported = *self.exit.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = self.child.as_mut() {
            return Ok(child.try_wait()?.map(|own| reported.unwrap_or(own)));
        }
        if reported.is_some() {
            return Ok(reported);
        }
        if self.pid == 0 || !crate::runtime::is_pid_alive(self.pid) {
            return Err(std::io::Error::other("the app's log relay is gone"));
        }
        Ok(None)
    }

    pub fn start_kill(&mut self) -> std::io::Result<()> {
        if let Some(child) = self.child.as_mut() {
            return child.start_kill();
        }
        #[cfg(unix)]
        if self.pid > 1 && self.pid <= i32::MAX as u32 {
            // SAFETY: plain kill(2) on a known positive pid.
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
        Ok(())
    }
}

/// Called for each line: (channel, level, message), as the panel's log callback.
pub type LogCb = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// Reads a relay connection: waits for `hello` (returning the relay's pid), then passes every
/// line to `log_cb` in a background task and records the exit in `exit`. `on_out` sees each
/// stdout line too (the panel uses it to notice child pids the app prints).
pub async fn attach<R>(reader: R, log_cb: LogCb, on_out: Arc<dyn Fn(&str) + Send + Sync>, exit: ExitSlot) -> std::io::Result<u32>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    let first = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .map_err(|_| std::io::Error::other("the log relay did not answer"))??
        .ok_or_else(|| std::io::Error::other("the log relay closed the connection"))?;
    let relay_pid = match serde_json::from_str::<Msg>(&first) {
        Ok(Msg::Hello { relay_pid, .. }) => relay_pid,
        _ => return Err(std::io::Error::other("unexpected first message from the log relay")),
    };
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<Msg>(&line) {
                Ok(Msg::Out { l }) => {
                    log_cb("app", "info", &l);
                    on_out(&l);
                }
                Ok(Msg::Err { l }) => log_cb("app", "warn", &l),
                Ok(Msg::Dropped { n }) => log_cb(
                    "system",
                    "warn",
                    &format!("{n} log line(s) were left out because the panel was away and more than {QUEUE_LINES} lines piled up"),
                ),
                Ok(Msg::Exit { code, signal }) => {
                    *exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(exit_status(code, signal));
                    break;
                }
                Ok(Msg::Hello { .. }) | Err(_) => {}
            }
        }
    });
    Ok(relay_pid)
}

#[cfg(unix)]
pub use unix::{connect, run};

#[cfg(unix)]
mod unix {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Notify;

    /// Connects to a relay, retrying for up to `wait` while it starts.
    pub async fn connect(socket: &Path, wait: Duration) -> std::io::Result<UnixStream> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match UnixStream::connect(socket).await {
                Ok(s) => return Ok(s),
                Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn set_signal(sig: libc::c_int, action: libc::sighandler_t) {
        // SAFETY: installs SIG_IGN or SIG_DFL, no handler code runs.
        unsafe {
            libc::signal(sig, action);
        }
    }

    /// `webdep --log-relay <socket> -- <program> [args...]`. Returns the exit code to use.
    pub async fn run(args: Vec<String>) -> i32 {
        let pos = args.iter().position(|a| a == FLAG);
        let (socket, program) = match pos.and_then(|p| args.get(p + 1).map(|s| (p, s))) {
            Some((p, socket)) if args.get(p + 2).map(String::as_str) == Some("--") && args.len() > p + 3 => {
                (PathBuf::from(socket), args[p + 3..].to_vec())
            }
            _ => {
                eprintln!("usage: webdep {FLAG} <socket> -- <program> [args...]");
                return 2;
            }
        };

        // Stopping or restarting the panel must not take the app down: only SIGKILL (which the
        // panel uses to stop an app's whole process group) ends the relay.
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            set_signal(sig, libc::SIG_IGN);
        }

        let _ = std::fs::remove_file(&socket);
        let listener = match UnixListener::bind(&socket) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("log relay: cannot listen on {}: {e}", socket.display());
                return 1;
            }
        };
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        }

        let mut cmd = tokio::process::Command::new(&program[0]);
        cmd.args(&program[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false);
        // SAFETY: only async-signal-safe calls between fork and exec. Ignored signals would stay
        // ignored in the app, so give it the normal ones back.
        unsafe {
            cmd.pre_exec(|| {
                for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("log relay: cannot start {}: {e}", program[0]);
                let _ = std::fs::remove_file(&socket);
                return 127;
            }
        };
        let app_pid = child.id().unwrap_or(0);
        let relay_pid = std::process::id();

        let queue = Arc::new(Mutex::new(Queue::default()));
        let wake = Arc::new(Notify::new());
        let delivered = Arc::new(Notify::new());

        let mut readers = Vec::new();
        for (stream, is_err) in [
            (child.stdout.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>), false),
            (child.stderr.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>), true),
        ] {
            let Some(mut stream) = stream else { continue };
            let (queue, wake) = (queue.clone(), wake.clone());
            readers.push(tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut split = LineSplitter::default();
                let mut buf = [0u8; 8192];
                let wrap = |l: String| if is_err { Msg::Err { l } } else { Msg::Out { l } };
                loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let lines = split.feed(&buf[..n]);
                    if !lines.is_empty() {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        for l in lines {
                            q.push(wrap(l));
                        }
                        drop(q);
                        wake.notify_one();
                    }
                }
                if let Some(l) = split.finish() {
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(wrap(l));
                    wake.notify_one();
                }
            }));
        }

        // One panel connection at a time; a new one replaces the old.
        let accept = {
            let (queue, wake, delivered) = (queue.clone(), wake.clone(), delivered.clone());
            tokio::spawn(async move {
                let mut current: Option<tokio::task::JoinHandle<()>> = None;
                while let Ok((stream, _)) = listener.accept().await {
                    if let Some(old) = current.take() {
                        old.abort();
                    }
                    let (queue, conn_wake, delivered) = (queue.clone(), wake.clone(), delivered.clone());
                    current = Some(tokio::spawn(async move {
                        let wake = conn_wake;
                        let mut stream = stream;
                        let hello = Msg::Hello { v: PROTOCOL_VERSION, relay_pid, app_pid };
                        let line = format!("{}\n", serde_json::to_string(&hello).unwrap_or_default());
                        if stream.write_all(line.as_bytes()).await.is_err() {
                            return;
                        }
                        loop {
                            match pump(&queue, &mut stream).await {
                                Ok(true) => {
                                    delivered.notify_one();
                                    return;
                                }
                                Ok(false) => wake.notified().await,
                                Err(_) => return,
                            }
                        }
                    }));
                    // Anything already queued goes out right away.
                    wake.notify_one();
                }
            })
        };

        let status = child.wait().await;
        for r in readers {
            let _ = r.await;
        }
        let (code, signal) = match &status {
            Ok(s) => {
                use std::os::unix::process::ExitStatusExt;
                (s.code(), s.signal())
            }
            Err(_) => (None, Some(libc::SIGKILL)),
        };
        queue.lock().unwrap_or_else(|e| e.into_inner()).set_exit(Msg::Exit { code, signal });
        wake.notify_one();
        let _ = tokio::time::timeout(LINGER_AFTER_EXIT, delivered.notified()).await;
        accept.abort();
        let _ = std::fs::remove_file(&socket);
        match (code, signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(l: &str) -> Msg {
        Msg::Out { l: l.to_string() }
    }

    #[test]
    fn messages_round_trip_as_tagged_json() {
        let m = Msg::Exit { code: Some(3), signal: None };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"t":"exit","code":3,"signal":null}"#);
        assert_eq!(serde_json::from_str::<Msg>(&s).unwrap(), m);
        assert_eq!(serde_json::to_string(&out("hi")).unwrap(), r#"{"t":"out","l":"hi"}"#);
    }

    #[test]
    fn queue_keeps_the_newest_lines_and_counts_the_rest() {
        let mut q = Queue::default();
        for i in 0..QUEUE_LINES + 5 {
            q.push(out(&i.to_string()));
        }
        let batch = q.take();
        assert_eq!(batch[0], Msg::Dropped { n: 5 });
        assert_eq!(batch[1], out("5"));
        assert_eq!(batch.len(), QUEUE_LINES + 1);
        assert!(q.take().is_empty(), "taken lines are gone");
    }

    #[test]
    fn exit_comes_after_the_lines_and_survives_a_failed_send() {
        let mut q = Queue::default();
        q.push(out("a"));
        q.set_exit(Msg::Exit { code: Some(0), signal: None });
        let batch = q.take();
        assert_eq!(batch, vec![out("a"), Msg::Exit { code: Some(0), signal: None }]);
        q.put_back(batch);
        q.push(out("b"));
        assert_eq!(q.take(), vec![out("a"), out("b"), Msg::Exit { code: Some(0), signal: None }]);
    }

    #[test]
    fn splitter_handles_partial_long_and_unfinished_lines() {
        let mut s = LineSplitter::default();
        assert_eq!(s.feed(b"hel"), Vec::<String>::new());
        assert_eq!(s.feed(b"lo\r\nworld\n"), vec!["hello", "world"]);
        let long = vec![b'x'; MAX_LINE_BYTES + 10];
        let lines = s.feed(&long);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES);
        assert_eq!(s.finish().unwrap().len(), 10);
        assert_eq!(s.finish(), None);
    }

    #[tokio::test]
    async fn pump_sends_everything_and_reports_the_exit() {
        let queue = Mutex::new(Queue::default());
        queue.lock().unwrap().push(out("x"));
        let (mut a, b) = tokio::io::duplex(4096);
        assert!(!pump(&queue, &mut a).await.unwrap());
        queue.lock().unwrap().set_exit(Msg::Exit { code: None, signal: Some(9) });
        assert!(pump(&queue, &mut a).await.unwrap());
        drop(a);
        let mut lines = BufReader::new(b).lines();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), r#"{"t":"out","l":"x"}"#);
        assert_eq!(lines.next_line().await.unwrap().unwrap(), r#"{"t":"exit","code":null,"signal":9}"#);
    }

    #[tokio::test]
    async fn pump_keeps_messages_when_the_panel_is_gone() {
        let queue = Mutex::new(Queue::default());
        queue.lock().unwrap().push(out("kept"));
        let (mut a, b) = tokio::io::duplex(64);
        drop(b);
        assert!(pump(&queue, &mut a).await.is_err());
        assert_eq!(queue.lock().unwrap().take(), vec![out("kept")]);
    }

    #[tokio::test]
    async fn attach_passes_lines_on_and_records_the_exit() {
        let (mut a, b) = tokio::io::duplex(4096);
        let script = [
            Msg::Hello { v: 1, relay_pid: 4242, app_pid: 4243 },
            out("hello"),
            Msg::Err { l: "oops".into() },
            Msg::Dropped { n: 2 },
            Msg::Exit { code: Some(7), signal: None },
        ];
        for m in &script {
            a.write_all(format!("{}\n", serde_json::to_string(m).unwrap()).as_bytes()).await.unwrap();
        }
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let outs: Arc<Mutex<Vec<String>>> = Arc::default();
        let (s2, o2) = (seen.clone(), outs.clone());
        let log_cb: LogCb = Arc::new(move |ch, lvl, msg| s2.lock().unwrap().push(format!("{ch}/{lvl}/{msg}")));
        let on_out = Arc::new(move |l: &str| o2.lock().unwrap().push(l.to_string()));
        let exit: ExitSlot = Arc::default();
        assert_eq!(attach(b, log_cb, on_out, exit.clone()).await.unwrap(), 4242);
        for _ in 0..50 {
            if exit.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(exit.lock().unwrap().unwrap().code(), Some(7));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], "app/info/hello");
        assert_eq!(seen[1], "app/warn/oops");
        assert!(seen[2].starts_with("system/warn/2 log line(s)"));
        assert_eq!(*outs.lock().unwrap(), vec!["hello"]);
    }

    #[tokio::test]
    async fn attach_refuses_something_that_is_not_a_relay() {
        let (mut a, b) = tokio::io::duplex(256);
        a.write_all(b"HTTP/1.1 200 OK\n").await.unwrap();
        let cb: LogCb = Arc::new(|_, _, _| {});
        assert!(attach(b, cb, Arc::new(|_: &str| {}), Arc::default()).await.is_err());
    }

    #[test]
    fn exit_status_maps_codes() {
        assert_eq!(exit_status(Some(2), None).code(), Some(2));
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(exit_status(None, Some(9)).signal(), Some(9));
        }
    }

    #[test]
    fn socket_path_is_inside_the_run_folder_and_short() {
        let short = socket_path(Path::new("/opt/webdep/data/containers/shop/run"));
        let long = socket_path(Path::new(&format!("/opt/webdep/data/containers/{}/run", "x".repeat(120))));
        if enabled() {
            assert_eq!(short, Some(PathBuf::from("/opt/webdep/data/containers/shop/run/relay.sock")));
        } else {
            assert_eq!(short, None);
        }
        assert_eq!(long, None);
    }
}
