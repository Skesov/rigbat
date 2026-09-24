//! Runs a test in a child process whose session and system bus is a private `dbus-daemon`.

use std::io::{BufRead as _, BufReader, Read as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "RIGBAT_TEST_PRIVATE_BUS";
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// No service directories, so a call to an absent name fails instead of activating a real service.
const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=@DIR@</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

/// `true` in the child, which runs the body; `false` in the parent once the child passed, or with no `dbus-daemon`.
// A skipped test must say so, and tests set up no tracing.
#[expect(clippy::print_stderr)]
pub fn isolated(module: &str, test: &str) -> bool {
    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping {test}: dbus-daemon is not installed");
        return false;
    };
    let name = match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    };
    let exe = std::env::current_exe().expect("test binary path");
    let child = Command::new(exe)
        .args(["--exact", &name, "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env("DBUS_SESSION_BUS_ADDRESS", &daemon.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", &daemon.address)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the test child");
    let (status, output) = wait_with_timeout(child, CHILD_TIMEOUT);
    // `--exact` with a misspelt name runs nothing and still exits 0.
    let ran = output.contains("test result: ok. 1 passed");
    assert!(
        status == Some(true) && ran,
        "{name} on the private bus {}:\n{output}",
        match status {
            None => "timed out",
            Some(_) => "failed",
        }
    );
    false
}

/// Whether `child` succeeded (`None`: killed at `timeout`), and its output.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> (Option<bool>, String) {
    let drain = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut out = String::new();
            let _ = pipe.read_to_string(&mut out);
            out
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("piped stdout")));
    let stderr = drain(Box::new(child.stderr.take().expect("piped stderr")));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("waiting for the test child") {
            break Some(status.success());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    (status, format!("{out}{err}"))
}

/// Killed, and its socket directory removed, on drop.
struct Daemon {
    process: Child,
    dir: PathBuf,
    address: String,
}

impl Daemon {
    /// `None` only when `dbus-daemon` is not installed; a broken one fails the test.
    fn start() -> Option<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rigbat-bus-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("creating the bus directory");
        let config = dir.join("bus.conf");
        let dir_str = dir.to_str().expect("UTF-8 temp dir");
        std::fs::write(&config, CONFIG.replace("@DIR@", dir_str)).expect("writing bus.conf");

        let spawned = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut process = match spawned {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
            spawned => spawned.expect("starting dbus-daemon"),
        };
        let mut address = String::new();
        let stdout = process.stdout.take().expect("piped stdout");
        BufReader::new(stdout)
            .read_line(&mut address)
            .expect("reading the bus address");
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Some(Self {
            process,
            dir,
            address,
        })
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Polls `probe` until it yields `Some`, failing the test after `timeout`.
pub async fn eventually<T, F>(timeout: Duration, mut probe: impl FnMut() -> F) -> T
where
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
