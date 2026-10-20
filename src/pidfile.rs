//! Pidfile for a running `tokenbar serve` daemon.
//!
//! Written at daemon startup (`<data_dir>/serve.pid`), removed on clean
//! shutdown. `tokenbar restart` reads it to find the daemon, sends SIGHUP,
//! and waits for the pidfile to be rewritten with a new PID (launchd
//! KeepAlive respawns the process).

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct PidInfo {
    pub pid: i32,
    pub addr: String,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("serve.pid")
}

/// Atomically write `<data_dir>/serve.pid` (`<pid> <addr> started_at=<unix>`).
pub async fn write(data_dir: &Path, pid: i32, addr: &str) -> io::Result<()> {
    let target = path(data_dir);
    let tmp = target.with_extension("pid.tmp");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tokio::fs::write(&tmp, format!("{pid} {addr} started_at={stamp}\n")).await?;
    // Rename is atomic on a same filesystem; a kill mid-write can never leave
    // a truncated final file for `read()` to misparse.
    tokio::fs::rename(&tmp, &target).await
}

/// Parse the pidfile; `Ok(None)` when missing or malformed.
pub async fn read(data_dir: &Path) -> io::Result<Option<PidInfo>> {
    match tokio::fs::read(path(data_dir)).await {
        Ok(bytes) => {
            let line = String::from_utf8_lossy(&bytes);
            let mut parts = line.split_whitespace();
            let pid = parts.next().and_then(|s| s.parse::<i32>().ok());
            let addr = parts.next().map(|s| s.to_string());
            match (pid, addr) {
                (Some(pid), Some(addr)) => Ok(Some(PidInfo { pid, addr })),
                _ => Ok(None),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove the pidfile; missing-file is fine (idempotent).
pub async fn remove(data_dir: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path(data_dir)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tokenbar-pidfile-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn write_read_remove_roundtrip() {
        let dir = temp_dir();
        write(&dir, 4242, "127.0.0.1:8790")
            .await
            .expect("write pidfile");
        let info = read(&dir).await.expect("read pidfile");
        assert_eq!(
            info,
            Some(PidInfo {
                pid: 4242,
                addr: "127.0.0.1:8790".into()
            })
        );
        remove(&dir).await.expect("remove pidfile");
        assert!(read(&dir).await.expect("read after remove").is_none());
    }

    #[tokio::test]
    async fn read_missing_is_none() {
        let dir = temp_dir();
        assert!(read(&dir).await.expect("read missing pidfile").is_none());
    }

    #[tokio::test]
    async fn read_malformed_is_none() {
        let dir = temp_dir();
        tokio::fs::write(path(&dir), "not-a-pidfile\n").await.unwrap();
        assert!(read(&dir).await.expect("read malformed pidfile").is_none());
    }

    #[tokio::test]
    async fn remove_missing_is_ok() {
        let dir = temp_dir();
        remove(&dir).await.expect("remove missing pidfile");
    }

    #[tokio::test]
    async fn write_overwrites_previous_content() {
        let dir = temp_dir();
        write(&dir, 1, "127.0.0.1:8000").await.unwrap();
        write(&dir, 2, "127.0.0.1:8790").await.unwrap();
        let info = read(&dir).await.unwrap().unwrap();
        assert_eq!(info.pid, 2);
        assert_eq!(info.addr, "127.0.0.1:8790");
    }
}
