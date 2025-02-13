use std::path::PathBuf;

use snafu::{ResultExt, Snafu};
use tokio::fs;
use url::Url;

pub mod cache;
pub mod processor;

use crate::{
    utils::path::PathOrUrl,
    xfer::{
        cache::{Cache, CacheSettings, CacheStatus, Error},
        processor::Processor,
    },
};

type Result<T> = core::result::Result<T, FileTransferError>;

#[derive(Debug, Snafu)]
pub enum FileTransferError {
    #[snafu(display("io error"))]
    IoError { source: std::io::Error },

    #[snafu(display("failed to extract file name from URL"))]
    FileNameError,

    #[snafu(display("cache error"))]
    CacheError { source: Error },

    #[snafu(display("request error"))]
    RequestError { source: reqwest::Error },

    #[snafu(display("failed to deserialize content into YAML"))]
    YamlError { source: serde_yaml::Error },

    #[snafu(display("templating error"))]
    TemplatingError { source: tera::Error },
}

#[derive(Debug)]
pub struct FileTransferClient {
    pub(crate) client: reqwest::Client,
    pub(crate) cache: Cache,
}

impl FileTransferClient {
    /// Creates a new [`FileTransferClient`] with caching capabilities.
    pub async fn new(cache_settings: CacheSettings) -> Result<Self> {
        let cache = cache_settings.try_into_cache().await.context(CacheSnafu)?;
        let client = reqwest::Client::new();

        Ok(Self { client, cache })
    }

    pub fn new_with(cache: Cache) -> Self {
        let client = reqwest::Client::new();
        Self { client, cache }
    }

    /// Retrieves data from `path_or_url` which can either be a [`PathBuf`]
    /// or a [`Url`]. The `processor` defines how the data is processed, for
    /// example as plain text data, YAML content or even templated.
    pub async fn get<P>(&self, path_or_url: &PathOrUrl, processor: &P) -> Result<P::Output>
    where
        P: Processor<Input = String>,
    {
        match path_or_url {
            PathOrUrl::Path(path) => processor.process(self.get_from_local_file(path).await?),
            PathOrUrl::Url(url) => processor.process(self.get_from_cache_or_remote(url).await?),
        }
    }

    async fn get_from_local_file(&self, path: &PathBuf) -> Result<String> {
        fs::read_to_string(path).await.context(IoSnafu)
    }

    /// Internal method which either looks up the requested file in the cache
    /// or retrieves it from the remote located at `url` when the cache missed
    /// or is expired.
    async fn get_from_cache_or_remote(&self, url: &Url) -> Result<String> {
        match self.cache.retrieve(url).await.context(CacheSnafu {})? {
            CacheStatus::Hit(content) => Ok(content),
            CacheStatus::Expired | CacheStatus::Miss => {
                let content = self.get_from_remote(url).await?;
                self.cache
                    .store(url, &content)
                    .await
                    .context(CacheSnafu {})?;

                Ok(content)
            }
        }
    }

    /// Internal call which executes a HTTP GET request to `url`.
    async fn get_from_remote(&self, url: &Url) -> Result<String> {
        let req = self
            .client
            .get(url.clone())
            .build()
            .context(RequestSnafu {})?;
        let result = self.client.execute(req).await.context(RequestSnafu {})?;

        result.text().await.context(RequestSnafu {})
    }
}
