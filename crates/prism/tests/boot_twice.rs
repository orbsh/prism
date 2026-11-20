//! A boot that looks healthy can still be broken: a panic inside a worker thread does not stop the
//! server, does not change the exit code, and does not appear in any unit test. That is exactly how a
//! real defect hid — `Collection::scan_range(begin, None)` ran to the end of the keyspace, a foreign
//! 20-byte key was decoded as a timer entry, and the panic landed in a tokio worker while the process
//! kept serving and logging that everything was live.
//!
//! The only shape that catches it is a REAL boot twice against the SAME data directory: the first boot
//! writes the rows, the second reads them, and the panic needs those rows to exist. So this test runs
//! the binary as a child process (nothing is caught in-process, because the failure is not an error
//! value), twice, and asserts no panic line appears in either run while registration still succeeds.
#![cfg(feature = "fjall")]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Run the binary once against `dir`, wait for the registration line, then stop it. Output goes to a
/// FILE rather than a pipe: a child that writes more than a pipe buffer would otherwise block, and a
/// test that deadlocks on its own fixture is worse than the defect it is looking for.
fn boot_once(exe: &Path, dir: &Path, log: &Path) -> String {
    let out = std::fs::File::create(log).expect("the log file");
    let err = out.try_clone().expect("a second handle for stderr");
    let mut child = spawn(exe, dir, out, err);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen_registration = false;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(log) {
            if text.contains("turn executor live") {
                seen_registration = true;
                break;
            }
            // A panic is also a stop condition: waiting the full deadline for a line that will never
            // come makes a failure look like a timeout.
            if text.contains("panicked") {
                break;
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut text = String::new();
    if let Ok(mut f) = std::fs::File::open(log) {
        let _ = f.read_to_string(&mut text);
    }
    assert!(
        seen_registration,
        "the boot never registered, so the run proves nothing:\n{text}"
    );
    text
}

fn spawn(exe: &Path, dir: &Path, out: std::fs::File, err: std::fs::File) -> Child {
    // A free port, taken and released: the child only needs to bind, and nothing else in the test uses
    // a fixed number (a clash would fail the boot for the wrong reason).
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        l.local_addr().expect("the port").to_string()
    };
    Command::new(exe)
        .env("PRISM_DATA", dir)
        .env("PRISM_IDENTITY", "open")
        .env("PRISM_ENGINE", "fjall")
        .env("PRISM_ADDR", addr)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .stdin(Stdio::null())
        .spawn()
        .expect("the prism binary starts")
}

#[test]
fn a_second_boot_over_stored_rows_does_not_panic() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_prism"));
    let dir = std::env::temp_dir().join(format!("prism-boot-twice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a data directory");

    let first = boot_once(&exe, &dir, &dir.join("boot-1.log"));
    assert!(
        !first.contains("panicked"),
        "the FIRST boot panicked:\n{first}"
    );
    assert!(
        first.contains("booth live"),
        "the first boot did not register anything, so it wrote no rows:\n{first}"
    );

    // Same directory, fresh process: this is the run that reads what the first one wrote.
    let second = boot_once(&exe, &dir, &dir.join("boot-2.log"));
    assert!(
        !second.contains("panicked"),
        "the SECOND boot panicked — a background task died while the process looked healthy:\n{second}"
    );
    assert!(
        second.contains("turn executor live"),
        "the second boot did not register:\n{second}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}