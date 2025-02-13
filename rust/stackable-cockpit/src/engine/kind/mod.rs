use std::process::Stdio;

use snafu::{ensure, ResultExt, Snafu};
use tokio::{io::AsyncWriteExt, process::Command};
use tracing::{debug, info, instrument};

use crate::{
    constants::DEFAULT_LOCAL_CLUSTER_NAME,
    engine::{check_if_docker_is_running, kind::config::KindClusterConfig, DockerError},
    utils::check::binaries_present_with_name,
};

mod config;

#[derive(Debug, Snafu)]
pub enum KindClusterError {
    #[snafu(display("io error"))]
    IoError { source: std::io::Error },

    #[snafu(display("no stdin error"))]
    StdinError,

    #[snafu(display("kind command error: {error}"))]
    KindCommandError { error: String },

    #[snafu(display("missing required binary: {binary}"))]
    MissingBinaryError { binary: String },

    #[snafu(display("docker error"))]
    DockerError { source: DockerError },

    #[snafu(display("failed to covert kind config to yaml"))]
    YamlError { source: serde_yaml::Error },
}

#[derive(Debug)]
pub struct KindCluster {
    cp_node_count: usize,
    node_count: usize,
    name: String,
}

impl KindCluster {
    /// Create a new kind cluster. This will NOT yet create the cluster on the
    /// system, but instead will return a data structure representing the
    /// cluster. To actually create the cluster, the `create` method must be
    /// called.
    pub fn new(node_count: usize, cp_node_count: usize, name: Option<String>) -> Self {
        Self {
            name: name.unwrap_or(DEFAULT_LOCAL_CLUSTER_NAME.into()),
            cp_node_count,
            node_count,
        }
    }

    /// Create a new local cluster by calling the kind binary.
    #[instrument]
    pub async fn create(&self) -> Result<(), KindClusterError> {
        info!("Creating local cluster using kind");

        // Check if required binaries are present
        if let Some(binary) = binaries_present_with_name(&["docker", "kind"]) {
            return Err(KindClusterError::MissingBinaryError { binary });
        }

        // Check if Docker is running
        check_if_docker_is_running().await.context(DockerSnafu)?;

        debug!("Creating kind cluster config");
        let config = KindClusterConfig::new(self.node_count, self.cp_node_count);
        let config_string = serde_yaml::to_string(&config).context(YamlSnafu)?;

        debug!("Creating kind cluster");
        let mut kind_cmd = Command::new("kind")
            .args(["create", "cluster"])
            .args(["--name", self.name.as_str()])
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .context(IoSnafu)?;

        // Pipe in config
        let mut stdin = kind_cmd.stdin.take().ok_or(KindClusterError::StdinError)?;
        stdin
            .write_all(config_string.as_bytes())
            .await
            .context(IoSnafu)?;

        // Write the piped in data
        stdin.flush().await.context(IoSnafu)?;
        drop(stdin);

        if let Err(err) = kind_cmd.wait().await {
            return Err(KindClusterError::KindCommandError {
                error: err.to_string(),
            });
        }

        Ok(())
    }

    /// Creates a kind cluster if it doesn't exist already.
    #[instrument]
    pub async fn create_if_not_exists(&self) -> Result<(), KindClusterError> {
        info!("Creating cluster if it doesn't exist using kind");

        if Self::check_if_cluster_exists(&self.name).await? {
            return Ok(());
        }

        self.create().await
    }

    /// Retrieve the cluster node count
    pub fn get_node_count(&self) -> usize {
        self.node_count
    }

    /// Retrieve the cluster name
    pub fn get_name(&self) -> &String {
        &self.name
    }

    /// Check if a kind cluster with the provided name already exists.
    #[instrument]
    async fn check_if_cluster_exists(cluster_name: &str) -> Result<bool, KindClusterError> {
        debug!("Checking if kind cluster exists");

        let output = Command::new("kind")
            .args(["get", "clusters"])
            .output()
            .await
            .context(IoSnafu)?;

        ensure!(
            output.status.success(),
            KindCommandSnafu {
                error: String::from_utf8_lossy(&output.stderr)
            }
        );

        let output = String::from_utf8_lossy(&output.stdout);
        Ok(output.lines().any(|name| name == cluster_name))
    }
}
