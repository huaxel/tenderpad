//! Offline rebuild staging. No caller may remove a live output before validation.
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Rebuild {
    directory: PathBuf,
    output: PathBuf,
    force: bool,
}

impl Rebuild {
    pub(crate) fn new(
        output: &Path,
        inputs: &[&Path],
        force: bool,
    ) -> Result<Self, Box<dyn Error>> {
        if output.exists() && !force {
            return Err(
                format!("{} exists; pass force=True to rebuild it", output.display()).into(),
            );
        }
        let canonical_output = fs::canonicalize(output).unwrap_or_else(|_| output.to_path_buf());
        for input in inputs {
            if *input == output
                || fs::canonicalize(input).is_ok_and(|path| path == canonical_output)
            {
                return Err(format!(
                    "output {} must not alias the input {}",
                    output.display(),
                    input.display()
                )
                .into());
            }
        }
        Self::check_wal(output)?;
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        // Exclusive directory creation gives ownership even under concurrent builds.
        // Never delete a guessed staging path: it could belong to another process.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!(".db-rebuild-{}-{nonce}-{id}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory,
                        output: output.to_path_buf(),
                        force,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not allocate an exclusive staging directory".into())
    }

    fn check_wal(output: &Path) -> Result<(), Box<dyn Error>> {
        let mut name = output
            .file_name()
            .ok_or("output needs a file name")?
            .to_os_string();
        name.push(".wal");
        if output.with_file_name(name).exists() {
            return Err("output has a WAL; close/recover the database before rebuilding".into());
        }
        Ok(())
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.directory.join("database.duckdb")
    }

    /// Call only after validation, CHECKPOINT, and closing the staging connection.
    /// Rebuilds are offline operations: this is not a live DuckDB lock protocol.
    pub(crate) fn publish(self) -> Result<(), Box<dyn Error>> {
        Self::check_wal(&self.output)?;
        if self.force {
            fs::rename(self.path(), &self.output)?;
        } else {
            // No-clobber publication, even if another builder created output.
            fs::hard_link(self.path(), &self.output)?;
        }
        Ok(())
    }
}

impl Drop for Rebuild {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
