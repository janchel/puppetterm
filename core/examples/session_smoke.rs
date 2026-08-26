//! Headless smoke test for the SSH pty mechanism used by the Tauri backend
//! (same portable-pty code path as `start_ssh_session`).
//!
//! Run: cargo run --example session_smoke
//! Connects to localhost over SSH, types a command, reads the streamed pty
//! output, and fails unless the expected output appears.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    let mut cmd = CommandBuilder::new("ssh");
    cmd.args([
        "-tt",
        "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=accept-new",
        "localhost",
    ]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn ssh");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    // Let the session connect, then drive it like the frontend would.
    std::thread::sleep(Duration::from_millis(1500));
    writer.write_all(b"echo PING-$((1+1))\r").expect("write cmd");
    writer.write_all(b"exit\r").expect("write exit");
    writer.flush().expect("flush");

    // Read all pty output until EOF or deadline.
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Instant::now() > deadline {
            println!("timed out reading pty");
            break;
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) => {
                println!("read error: {e}");
                break;
            }
        }
    }

    let text = String::from_utf8_lossy(&out);
    println!("--- pty output ({} bytes) ---", out.len());
    println!("{text}");

    let _ = child.kill();
    let _ = child.wait();

    if text.contains("PING-2") {
        println!("RESULT: PASS");
    } else {
        println!("RESULT: FAIL — expected 'PING-2' in output");
        std::process::exit(1);
    }
}
