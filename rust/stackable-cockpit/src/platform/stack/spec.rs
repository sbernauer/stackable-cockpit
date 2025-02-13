use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use tracing::{debug, info, instrument, log::warn};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

use crate::{
    common::ManifestSpec,
    helm::{self, HelmChart, HelmError},
    platform::{
        cluster::{ResourceRequests, ResourceRequestsError},
        demo::DemoParameter,
        release::{ReleaseInstallError, ReleaseList},
    },
    utils::{
        k8s::{KubeClient, KubeClientError},
        params::{
            IntoParameters, IntoParametersError, Parameter, RawParameter, RawParameterParseError,
        },
        path::{IntoPathOrUrl, PathOrUrlParseError},
    },
    xfer::{
        processor::{Processor, Template, Yaml},
        FileTransferClient, FileTransferError,
    },
};

pub type RawStackParameterParseError = RawParameterParseError;
pub type RawStackParameter = RawParameter;
pub type StackParameter = Parameter;

/// This error indicates that the stack, the stack manifests or the demo
/// manifests failed to install.
#[derive(Debug, Snafu)]
pub enum StackError {
    /// This error indicates that parsing a string into stack / demo parameters
    /// failed.
    #[snafu(display("parameter parse error: {source}"))]
    ParameterError { source: IntoParametersError },

    /// This error indicates that the requested stack doesn't exist in the
    /// loaded list of stacks.
    #[snafu(display("no such stack"))]
    NoSuchStack,

    /// This error indicates that the release failed to install.
    #[snafu(display("release install error: {source}"))]
    ReleaseInstallError { source: ReleaseInstallError },

    /// This error indicates the Helm wrapper encountered an error.
    #[snafu(display("Helm error: {source}"))]
    HelmError { source: HelmError },

    #[snafu(display("kube error: {source}"))]
    KubeError { source: KubeClientError },

    /// This error indicates a YAML error occurred.
    #[snafu(display("yaml error: {source}"))]
    YamlError { source: serde_yaml::Error },

    #[snafu(display("stack resource requests error"), context(false))]
    StackResourceRequestsError { source: ResourceRequestsError },

    #[snafu(display("path or url parse error"))]
    PathOrUrlParseError { source: PathOrUrlParseError },

    #[snafu(display("transfer error"))]
    TransferError { source: FileTransferError },

    #[snafu(display("cannot install stack in namespace '{requested}', only '{}' supported", supported.join(", ")))]
    UnsupportedNamespace {
        requested: String,
        supported: Vec<String>,
    },
}

/// This struct describes a stack with the v2 spec
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StackSpecV2 {
    /// A short description of the demo
    pub description: String,

    /// The release used by the stack, e.g. 23.4
    #[serde(rename = "stackableRelease")]
    pub release: String,

    /// Supported namespaces this stack can run in. An empty list indicates that
    /// the stack can run in any namespace.
    #[serde(default)]
    pub supported_namespaces: Vec<String>,

    /// A variable number of operators
    #[serde(rename = "stackableOperators")]
    pub operators: Vec<String>,

    /// A variable number of labels (tags)
    #[serde(default)]
    pub labels: Vec<String>,

    /// A variable number of Helm or YAML manifests
    #[serde(default)]
    pub manifests: Vec<ManifestSpec>,

    /// The resource requests the stack imposes on a Kubernetes cluster
    pub resource_requests: Option<ResourceRequests>,

    /// A variable number of supported parameters
    #[serde(default)]
    pub parameters: Vec<StackParameter>,
}

impl StackSpecV2 {
    /// Checks if the prerequisites to run this stack are met. These checks
    /// include:
    ///
    /// - Does the stack support to be installed in the requested namespace?
    /// - Does the cluster have enough resources available to run this stack?
    #[instrument(skip_all)]
    pub async fn check_prerequisites(&self, product_namespace: &str) -> Result<(), StackError> {
        debug!("Checking prerequisites before installing stack");

        // Returns an error if the stack doesn't support to be installed in the
        // requested product namespace. When installing a demo, this check is
        // already done on the demo spec level, however we still need to check
        // here, as stacks can be installed on their own.
        if !self.supports_namespace(product_namespace) {
            return Err(StackError::UnsupportedNamespace {
                supported: self.supported_namespaces.clone(),
                requested: product_namespace.to_string(),
            });
        }

        // Checks if the available cluster resources are sufficient to deploy
        // the demo.
        if let Some(resource_requests) = &self.resource_requests {
            if let Err(err) = resource_requests.validate_cluster_size("stack").await {
                match err {
                    ResourceRequestsError::ValidationErrors { errors } => {
                        for error in errors {
                            warn!("{error}");
                        }
                    }
                    err => return Err(err.into()),
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self, release_list))]
    pub async fn install_release(
        &self,
        release_list: ReleaseList,
        operator_namespace: &str,
        product_namespace: &str,
        skip_release_install: bool,
    ) -> Result<(), StackError> {
        info!("Installing release for stack");

        if skip_release_install {
            info!("Skipping release installation during stack installation process");
            return Ok(());
        }

        // Get the release by name
        let release = release_list
            .get(&self.release)
            .ok_or(StackError::NoSuchStack)?;

        // Install the release
        release
            .install(&self.operators, &[], operator_namespace)
            .context(ReleaseInstallSnafu)?;

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn install_stack_manifests(
        &self,
        parameters: &[String],
        product_namespace: &str,
        transfer_client: &FileTransferClient,
    ) -> Result<(), StackError> {
        info!("Installing stack manifests");

        let parameters = parameters
            .to_owned()
            .into_params(&self.parameters)
            .context(ParameterSnafu)?;

        Self::install_manifests(
            &self.manifests,
            &parameters,
            product_namespace,
            transfer_client,
        )
        .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn install_demo_manifests(
        &self,
        manifests: &Vec<ManifestSpec>,
        valid_demo_parameters: &[DemoParameter],
        demo_parameters: &[String],
        product_namespace: &str,
        transfer_client: &FileTransferClient,
    ) -> Result<(), StackError> {
        info!("Installing demo manifests");

        let parameters = demo_parameters
            .to_owned()
            .into_params(valid_demo_parameters)
            .context(ParameterSnafu)?;

        Self::install_manifests(manifests, &parameters, product_namespace, transfer_client).await?;
        Ok(())
    }

    /// Installs a list of templated manifests inside a namespace.
    #[instrument(skip_all)]
    async fn install_manifests(
        manifests: &Vec<ManifestSpec>,
        parameters: &HashMap<String, String>,
        product_namespace: &str,
        transfer_client: &FileTransferClient,
    ) -> Result<(), StackError> {
        debug!("Installing demo / stack manifests");

        for manifest in manifests {
            match manifest {
                ManifestSpec::HelmChart(helm_file) => {
                    debug!("Installing manifest from Helm chart {}", helm_file);

                    // Read Helm chart YAML and apply templating
                    let helm_file = helm_file.into_path_or_url().context(PathOrUrlParseSnafu)?;
                    let helm_chart: HelmChart = transfer_client
                        .get(&helm_file, &Template::new(parameters).then(Yaml::new()))
                        .await
                        .context(TransferSnafu)?;

                    info!(
                        "Installing Helm chart {} ({})",
                        helm_chart.name, helm_chart.version
                    );

                    helm::add_repo(&helm_chart.repo.name, &helm_chart.repo.url)
                        .context(HelmSnafu)?;

                    // Serialize chart options to string
                    let values_yaml =
                        serde_yaml::to_string(&helm_chart.options).context(YamlSnafu)?;

                    // Install the Helm chart using the Helm wrapper
                    helm::install_release_from_repo(
                        &helm_chart.release_name,
                        &helm_chart.release_name,
                        helm::ChartVersion {
                            repo_name: &helm_chart.repo.name,
                            chart_name: &helm_chart.name,
                            chart_version: Some(&helm_chart.version),
                        },
                        Some(&values_yaml),
                        product_namespace,
                        false,
                    )
                    .context(HelmSnafu)?;
                }
                ManifestSpec::PlainYaml(path_or_url) => {
                    debug!("Installing YAML manifest from {}", path_or_url);

                    // Read YAML manifest and apply templating
                    let path_or_url = path_or_url
                        .into_path_or_url()
                        .context(PathOrUrlParseSnafu)?;

                    let manifests = transfer_client
                        .get(&path_or_url, &Template::new(parameters))
                        .await
                        .context(TransferSnafu)?;

                    let kube_client = KubeClient::new().await.context(KubeSnafu)?;
                    kube_client
                        .deploy_manifests(&manifests, product_namespace)
                        .await
                        .context(KubeSnafu)?
                }
            }
        }

        Ok(())
    }

    fn supports_namespace(&self, namespace: impl Into<String>) -> bool {
        self.supported_namespaces.is_empty()
            || self.supported_namespaces.contains(&namespace.into())
    }
}
