//! The pool's log: Go's `log.New(os.Stdout, "codex-pool ", LstdFlags|LUTC)`
//! line format, written to stdout and to a rotating file under /data.
//!
//! Each deploy recreates the container, and Docker's log goes with it, so the
//! log is also kept under /data, which outlives the container. A session that
//! moved accounts a minute after a deploy could not be traced once the next
//! deploy had discarded that container's log. The file is rotated past a size
//! (Go rotatingLogFile), so it stays bounded however long the pool runs.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

pub struct RotatingLogFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: File,
    size: u64,
}

impl RotatingLogFile {
    pub fn open(path: &Path, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (file, size) = open_append(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes,
            keep,
            file,
            size,
        })
    }

    pub fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.size + line.len() as u64 > self.max_bytes && self.size > 0 {
            for index in (1..=self.keep).rev() {
                let from = if index > 1 {
                    PathBuf::from(format!("{}.{}", self.path.display(), index - 1))
                } else {
                    self.path.clone()
                };
                let _ = std::fs::rename(from, format!("{}.{index}", self.path.display()));
            }
            let (file, size) = open_append(&self.path)?;
            self.file = file;
            self.size = size;
        }
        self.file.write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }
}

fn open_append(path: &Path) -> std::io::Result<(File, u64)> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let size = file.metadata()?.len();
    Ok((file, size))
}

enum Sink {
    Stdout,
    File(RotatingLogFile),
    Capture(Arc<Mutex<Vec<String>>>),
}

pub struct Logger {
    sinks: Mutex<Vec<Sink>>,
}

impl Logger {
    /// Stdout and, when it can be opened, the persistent file.
    pub fn production(data_dir: &Path) -> Self {
        let logger = Self {
            sinks: Mutex::new(vec![Sink::Stdout]),
        };
        match RotatingLogFile::open(&data_dir.join("logs").join("service.log"), 20 << 20, 3) {
            Ok(file) => logger.lock().push(Sink::File(file)),
            Err(error) => logger.printf(&format!("persistent log unavailable: {error}")),
        }
        logger
    }

    /// Production always supplies a logger, so tests do too, or a test never
    /// reaches the code paths that log.
    pub fn discard() -> Self {
        Self {
            sinks: Mutex::new(Vec::new()),
        }
    }

    /// A logger whose lines (without the prefix and time) a test can read.
    pub fn capture() -> (Self, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                sinks: Mutex::new(vec![Sink::Capture(lines.clone())]),
            },
            lines,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Sink>> {
        self.sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Go a.logger.Printf: one line, "codex-pool 2006/01/02 15:04:05 msg".
    pub fn printf(&self, message: &str) {
        let message = message.strip_suffix('\n').unwrap_or(message);
        let line = format!(
            "codex-pool {} {message}\n",
            chrono::Utc::now().format("%Y/%m/%d %H:%M:%S")
        );
        for sink in self.lock().iter_mut() {
            match sink {
                Sink::Stdout => {
                    let _ = std::io::stdout().write_all(line.as_bytes());
                }
                Sink::File(file) => {
                    let _ = file.write(line.as_bytes());
                }
                Sink::Capture(lines) => {
                    lines
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(message.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go TestRotatingLogFileStaysBounded.
    #[test]
    fn rotating_log_file_stays_bounded() {
        let dir = crate::pool::testing::TempDir::new();
        let path = dir.path().join("logs").join("pool.log");
        let mut log = RotatingLogFile::open(&path, 64, 2).unwrap_or_else(|error| panic!("{error}"));
        for index in 0..20 {
            let _ = log.write(format!("line {index:02} of the pool log\n").as_bytes());
        }
        let size = |path: &Path| std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        assert!(size(&path) <= 64);
        assert!(path.with_extension("log.1").exists());
        assert!(path.with_extension("log.2").exists());
        assert!(!path.with_extension("log.3").exists());
        let (logger, lines) = Logger::capture();
        logger.printf("hello\n");
        assert_eq!(
            lines.lock().map(|l| l.clone()).unwrap_or_default(),
            vec!["hello".to_string()]
        );
    }
}
