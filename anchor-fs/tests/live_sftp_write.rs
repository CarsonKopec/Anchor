//! Live SFTP write/read/stat/remove round-trip against a real server.
//!
//! Ignored by default (it needs a configured connection + a reachable server). Run with:
//!
//! ```text
//! cargo test -p anchor-fs --test live_sftp_write -- --ignored --nocapture
//! ```
//!
//! Uses connection `ANCHOR_TEST_CONN` (default `box1`) — which must be configured and have a
//! stored password — and writes a uniquely-named temp file under `ANCHOR_TEST_REMOTE_PATH`
//! (default `/C:/Users/kopec`), then reads it back, stats it, and removes it. The remote dir
//! must be writable; the colon in `/C:/...` is fine — it only travels over SFTP, never as a
//! Windows path.

use std::path::Path;

use anchor_core::config::AnchorConfig;
use anchor_core::credentials::{CredentialStore, Secrets};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a live SFTP server + configured connection"]
async fn sftp_write_read_remove_roundtrip() {
    let name = std::env::var("ANCHOR_TEST_CONN").unwrap_or_else(|_| "box1".into());
    let remote_dir =
        std::env::var("ANCHOR_TEST_REMOTE_PATH").unwrap_or_else(|_| "/C:/Users/kopec".into());

    let cfg = AnchorConfig::load().expect("load config");
    let mut conn = cfg
        .get(&name)
        .cloned()
        .unwrap_or_else(|| panic!("connection '{name}' is not configured"));
    conn.remote_path = remote_dir.clone();

    let secret = CredentialStore::new()
        .retrieve(&conn.credential_key)
        .expect("retrieve credential (run `anchor set-password`)");
    let backend = anchor_fs::build_backend(&conn, &secret).expect("build backend");

    // A non-trivial byte pattern so a read-back proves byte accuracy, not just length.
    let payload: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
    let fname = format!("anchor-write-test-{}.bin", std::process::id());
    let path = Path::new(&fname);

    let written = backend.write(path, 0, &payload).await.expect("write");
    assert_eq!(written as usize, payload.len(), "short write");

    let got = backend
        .read(path, 0, payload.len() as u32)
        .await
        .expect("read");
    assert_eq!(got, payload, "read-back content mismatch");

    let meta = backend.stat(path).await.expect("stat");
    assert!(!meta.is_dir, "written path should be a file");
    assert_eq!(meta.len, payload.len() as u64, "stat size mismatch");

    backend.remove(path, false).await.expect("remove");
    assert!(
        backend.stat(path).await.is_err(),
        "file should be gone after remove"
    );

    println!(
        "OK: wrote/read/stat/removed {remote_dir}/{fname} ({} bytes), content verified",
        payload.len()
    );
}
