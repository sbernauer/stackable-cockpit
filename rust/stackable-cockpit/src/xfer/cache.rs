use std::{
    ffi::OsString,
    num::ParseIntError,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use snafu::{ResultExt, Snafu};
use tokio::{fs, io};
use tracing::debug;
use url::Url;

use crate::constants::{
    CACHE_LAST_AUTO_PURGE_FILEPATH, CACHE_PROTECTED_FILES, DEFAULT_AUTO_PURGE_INTERVAL,
    DEFAULT_CACHE_MAX_AGE,
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("io error"))]
    CacheIoError { source: io::Error },

    #[snafu(display("system time error"))]
    SystemTimeError { source: SystemTimeError },

    #[snafu(display("failed to parse string as integer"))]
    ParseIntError { source: ParseIntError },

    #[snafu(display("tried to write file with disabled cache"))]
    WriteDisabled,
}

#[derive(Debug)]
pub struct Cache {
    pub(crate) auto_purge_interval: Duration,
    pub(crate) backend: CacheBackend,
    pub(crate) max_age: Duration,
}

impl Cache {
    /// Returns wether the cache is enabled.
    pub fn is_enabled(&self) -> bool {
        match self.backend {
            CacheBackend::Disk { .. } => true,
            CacheBackend::Disabled => false,
        }
    }

    /// Retrieves cached content located at `file_name`. It should be noted that
    /// the `file_name` should only contain the file name and extension without
    /// any path segments prefixed. The cache internally makes sure the file is
    /// read from within the cache base path. The status is indicated by
    /// [`CacheStatus`]. An error is returned when the cache was unable to read
    /// data from disk.
    pub async fn retrieve(&self, file_url: &Url) -> Result<CacheStatus<String>> {
        match &self.backend {
            CacheBackend::Disk { base_path } => {
                let file_path = Self::file_path(base_path, file_url);

                if !file_path.is_file() {
                    return Ok(CacheStatus::Miss);
                }

                let modified = file_path
                    .metadata()
                    .context(CacheIoSnafu {})?
                    .modified()
                    .context(CacheIoSnafu {})?;

                let elapsed = modified.elapsed().context(SystemTimeSnafu {})?;

                if elapsed > self.max_age {
                    return Ok(CacheStatus::Expired);
                }

                let content = Self::read(file_path).await?;
                Ok(CacheStatus::Hit(content))
            }
            CacheBackend::Disabled => Ok(CacheStatus::Miss),
        }
    }

    /// Stores `file_content` at the cache base path in a file named `file_name`.
    /// The method returns an error if the cache fails to write the data to disk
    /// or the cache is disabled.
    pub async fn store(&self, file_url: &Url, file_content: &str) -> Result<()> {
        match &self.backend {
            CacheBackend::Disk { base_path } => {
                let file_path = Self::file_path(base_path, file_url);
                Self::write(file_path, file_content).await
            }
            CacheBackend::Disabled => WriteDisabledSnafu {}.fail(),
        }
    }

    /// Returns a list of currently cached files. This method makes no assumptions
    /// if the cached files are expired. It simply returns a list of files known
    /// by the cache.
    pub async fn list(&self) -> Result<Vec<(PathBuf, SystemTime)>> {
        match &self.backend {
            CacheBackend::Disk { base_path } => {
                let mut files = Vec::new();

                let mut entries = fs::read_dir(base_path).await.context(CacheIoSnafu)?;

                while let Some(entry) = entries.next_entry().await.context(CacheIoSnafu)? {
                    let metadata = entry.metadata().await.context(CacheIoSnafu)?;

                    // Skip protected files
                    if is_protected_file(entry.file_name()) {
                        continue;
                    }

                    files.push((entry.path(), metadata.modified().context(CacheIoSnafu)?))
                }

                Ok(files)
            }
            CacheBackend::Disabled => Ok(vec![]),
        }
    }

    /// Removes all cached files by deleting the base cache folder and then
    /// recreating it.
    pub async fn purge(&self, delete_filter: DeleteFilter) -> Result<()> {
        match &self.backend {
            CacheBackend::Disk { base_path } => {
                let mut entries = fs::read_dir(base_path).await.context(CacheIoSnafu)?;

                while let Some(entry) = entries.next_entry().await.context(CacheIoSnafu)? {
                    let metadata = entry.metadata().await.context(CacheIoSnafu)?;

                    let should_delete_file = match delete_filter {
                        // Skip protected files
                        _ if is_protected_file(entry.file_name()) => false,

                        // Without --old / --outdated
                        DeleteFilter::All => true,
                        // with --old/--outdated
                        DeleteFilter::OnlyExpired => {
                            metadata
                                .modified()
                                .context(CacheIoSnafu)?
                                .elapsed()
                                .context(SystemTimeSnafu)?
                                > self.max_age
                        }
                    };

                    if should_delete_file {
                        fs::remove_file(entry.path()).await.context(CacheIoSnafu)?;
                    }
                }

                Ok(())
            }
            CacheBackend::Disabled => Ok(()),
        }
    }

    pub async fn auto_purge(&self) -> Result<()> {
        match &self.backend {
            CacheBackend::Disk { base_path } => {
                let cache_auto_purge_filepath = base_path.join(CACHE_LAST_AUTO_PURGE_FILEPATH);

                // Read and covert timestamp
                let last_purged_at = match fs::read_to_string(&cache_auto_purge_filepath).await {
                    Ok(timestamp) => Some(
                        UNIX_EPOCH + Duration::from_secs(timestamp.parse().context(ParseIntSnafu)?),
                    ),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    Err(err) => return Err(err).context(CacheIoSnafu),
                };

                // If the auto purge interval elapsed, run purge and write
                // back the new timestamp
                if last_purged_at
                    .and_then(|ts| ts.elapsed().ok())
                    .map_or(true, |elapsed| elapsed >= self.auto_purge_interval)
                {
                    debug!("Auto-purging outdated cache files");

                    self.purge(DeleteFilter::OnlyExpired).await?;
                    write_cache_auto_purge_file(&cache_auto_purge_filepath).await?;
                }

                Ok(())
            }
            CacheBackend::Disabled => Ok(()),
        }
    }

    fn new(backend: CacheBackend, max_age: Duration, auto_purge_interval: Duration) -> Self {
        Self {
            auto_purge_interval,
            backend,
            max_age,
        }
    }

    async fn read(file_path: PathBuf) -> Result<String> {
        fs::read_to_string(file_path).await.context(CacheIoSnafu {})
    }

    async fn write(file_path: PathBuf, file_content: &str) -> Result<()> {
        fs::write(file_path, file_content)
            .await
            .context(CacheIoSnafu {})
    }

    fn file_path(base_path: &Path, file_url: &Url) -> PathBuf {
        let mut hasher = Sha256::new();

        let sanitized_file_name = file_url
            .as_str()
            .replace(|c: char| !c.is_alphanumeric(), "-");

        hasher.update(file_url.as_str().as_bytes());
        let file_url_hash = hasher.finalize();

        base_path.join(format!("{sanitized_file_name}-{file_url_hash:x}"))
    }
}

pub enum CacheStatus<T> {
    Hit(T),
    Expired,
    Miss,
}

#[derive(Debug, Clone)]
pub struct CacheSettings {
    pub auto_purge_interval: Duration,
    pub backend: CacheBackend,
    pub max_age: Duration,
}

impl From<CacheBackend> for CacheSettings {
    fn from(backend: CacheBackend) -> Self {
        Self {
            auto_purge_interval: DEFAULT_AUTO_PURGE_INTERVAL,
            max_age: DEFAULT_CACHE_MAX_AGE,
            backend,
        }
    }
}

impl CacheSettings {
    pub fn disk(base_path: impl Into<PathBuf>) -> Self {
        CacheBackend::Disk {
            base_path: base_path.into(),
        }
        .into()
    }

    pub fn disabled() -> Self {
        CacheBackend::Disabled.into()
    }

    /// Creates a new [`Cache`] instance with the provided `settings`. It also
    /// initializes the cache backend. This ensure that local files and folders
    /// needed for operation are created.
    pub async fn try_into_cache(self) -> Result<Cache> {
        match &self.backend {
            CacheBackend::Disk { base_path } => {
                fs::create_dir_all(base_path).await.context(CacheIoSnafu)?;

                Ok(Cache::new(
                    self.backend,
                    self.max_age,
                    self.auto_purge_interval,
                ))
            }
            CacheBackend::Disabled => Ok(Cache::new(
                self.backend,
                self.max_age,
                self.auto_purge_interval,
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CacheBackend {
    Disk { base_path: PathBuf },
    Disabled,
}

pub enum DeleteFilter {
    All,
    OnlyExpired,
}

async fn write_cache_auto_purge_file(path: &Path) -> Result<()> {
    fs::write(
        path,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context(SystemTimeSnafu)?
            .as_secs()
            .to_string()
            .as_bytes(),
    )
    .await
    .context(CacheIoSnafu)
}

fn is_protected_file(filename: OsString) -> bool {
    // Non-UTF-8 filenames can't possibly be on the protected list
    let Some(filename) = filename.to_str() else { return false; };
    CACHE_PROTECTED_FILES.contains(&filename)
}
