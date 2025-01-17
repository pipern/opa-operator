mod error;

use crate::error::Error;
use async_trait::async_trait;
use futures::Future;
use k8s_openapi::api::core::v1::{ConfigMap, EnvVar, Pod};
use kube::api::ListParams;
use kube::Api;
use kube::ResourceExt;
use product_config::types::PropertyNameKind;
use product_config::ProductConfigManager;
use stackable_opa_crd::{
    OpaRole, OpenPolicyAgent, APP_NAME, CONFIG_FILE, PORT, REPO_RULE_REFERENCE,
};
use stackable_operator::builder::{
    ContainerBuilder, ContainerPortBuilder, ObjectMetaBuilder, PodBuilder,
};
use stackable_operator::client::Client;
use stackable_operator::controller::{Controller, ControllerStrategy, ReconciliationState};
use stackable_operator::error::OperatorResult;
use stackable_operator::labels::{
    build_common_labels_for_all_managed_resources, get_recommended_labels, APP_COMPONENT_LABEL,
    APP_INSTANCE_LABEL, APP_VERSION_LABEL,
};
use stackable_operator::product_config_utils::{
    config_for_role_and_group, transform_all_roles_to_config, validate_all_roles_and_groups_config,
    ValidatedRoleConfigByPropertyKind,
};
use stackable_operator::reconcile::{
    ContinuationStrategy, ReconcileFunctionAction, ReconcileResult, ReconciliationContext,
};
use stackable_operator::role_utils::{
    get_role_and_group_labels, list_eligible_nodes_for_role_and_group, EligibleNodesForRoleAndGroup,
};
use stackable_operator::{configmap, k8s_utils, name_utils, role_utils};
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use strum::IntoEnumIterator;
use tracing::{debug, info, trace, warn};

const FINALIZER_NAME: &str = "opa.stackable.tech/cleanup";
const SHOULD_BE_SCRAPED: &str = "monitoring.stackable.tech/should_be_scraped";
const CONFIG_MAP_TYPE_CONFIG: &str = "config";

type OpaReconcileResult = ReconcileResult<error::Error>;

struct OpaState {
    context: ReconciliationContext<OpenPolicyAgent>,
    existing_pods: Vec<Pod>,
    eligible_nodes: EligibleNodesForRoleAndGroup,
    validated_role_config: ValidatedRoleConfigByPropertyKind,
}

impl OpaState {
    pub fn required_pod_labels(&self) -> BTreeMap<String, Option<Vec<String>>> {
        let roles = OpaRole::iter()
            .map(|role| role.to_string())
            .collect::<Vec<_>>();
        let mut mandatory_labels = BTreeMap::new();

        mandatory_labels.insert(String::from(APP_COMPONENT_LABEL), Some(roles));
        mandatory_labels.insert(
            String::from(APP_INSTANCE_LABEL),
            Some(vec![self.context.name()]),
        );
        mandatory_labels.insert(
            APP_VERSION_LABEL.to_string(),
            Some(vec![self.context.resource.spec.version.to_string()]),
        );
        mandatory_labels
    }

    async fn create_missing_pods(&mut self) -> OpaReconcileResult {
        // The iteration happens in two stages here, to accommodate the way our operators think
        // about nodes and roles.
        // The hierarchy is:
        // - Roles (for example Datanode, Namenode, Opa Server)
        //   - Role groups for this role (user defined)
        //      - Individual nodes
        for role in OpaRole::iter() {
            if let Some(nodes_for_role) = self.eligible_nodes.get(&role.to_string()) {
                let role_str = &role.to_string();
                for (role_group, (nodes, replicas)) in nodes_for_role {
                    debug!(
                        "Identify missing pods for [{}] role and group [{}]",
                        role_str, role_group
                    );
                    trace!(
                        "candidate_nodes[{}]: [{:?}]",
                        nodes.len(),
                        nodes
                            .iter()
                            .map(|node| node.metadata.name.as_ref().unwrap())
                            .collect::<Vec<_>>()
                    );
                    trace!(
                        "existing_pods[{}]: [{:?}]",
                        &self.existing_pods.len(),
                        &self
                            .existing_pods
                            .iter()
                            .map(|pod| pod.metadata.name.as_ref().unwrap())
                            .collect::<Vec<_>>()
                    );
                    trace!(
                        "labels: [{:?}]",
                        get_role_and_group_labels(role_str, role_group)
                    );
                    let nodes_that_need_pods = k8s_utils::find_nodes_that_need_pods(
                        nodes,
                        &self.existing_pods,
                        &get_role_and_group_labels(role_str, role_group),
                        *replicas,
                    );

                    for node in nodes_that_need_pods {
                        let node_name = if let Some(node_name) = &node.metadata.name {
                            node_name
                        } else {
                            warn!("No name found in metadata, this should not happen! Skipping node: [{:?}]", node);
                            continue;
                        };
                        debug!(
                            "Creating pod on node [{}] for [{}] role and group [{}]",
                            node.metadata
                                .name
                                .as_deref()
                                .unwrap_or("<no node name found>"),
                            role_str,
                            role_group
                        );

                        // now we have a node that needs pods -> get validated config
                        let validated_config = config_for_role_and_group(
                            role_str,
                            role_group,
                            &self.validated_role_config,
                        )?;

                        let config_maps = self
                            .create_config_maps(role_str, role_group, validated_config)
                            .await?;

                        self.create_pod(
                            role_str,
                            role_group,
                            node_name,
                            &config_maps,
                            validated_config,
                        )
                        .await?;

                        return Ok(ReconcileFunctionAction::Requeue(Duration::from_secs(10)));
                    }
                }
            }
        }
        Ok(ReconcileFunctionAction::Continue)
    }

    /// Creates the config maps required for an opa instance (or role, role_group combination):
    /// * The 'config.yaml'
    ///
    /// The 'config.yaml' properties are read from the product_config.
    ///
    /// Labels are automatically adapted from the `recommended_labels` with a type (config for
    /// 'config.yaml'). Names are generated via `name_utils::build_resource_name`.
    ///
    /// Returns a map with a 'type' identifier (e.g. data, id) as key and the corresponding
    /// ConfigMap as value. This is required to set the volume mounts in the pod later on.
    ///
    /// # Arguments
    ///
    /// - `role` - The OPA role.
    /// - `group` - The role group.
    /// - `validated_config` - The validated product config.
    ///
    async fn create_config_maps(
        &self,
        role: &str,
        group: &str,
        validated_config: &HashMap<PropertyNameKind, BTreeMap<String, String>>,
    ) -> Result<HashMap<&'static str, ConfigMap>, Error> {
        let mut config_maps = HashMap::new();

        let recommended_labels = get_recommended_labels(
            &self.context.resource,
            APP_NAME,
            &self.context.resource.spec.version.to_string(),
            role,
            group,
        );

        if let Some(config) = validated_config.get(&PropertyNameKind::File(CONFIG_FILE.to_string()))
        {
            // enhance with config map type label
            let mut cm_config_labels = recommended_labels.clone();
            cm_config_labels.insert(
                configmap::CONFIGMAP_TYPE_LABEL.to_string(),
                CONFIG_MAP_TYPE_CONFIG.to_string(),
            );

            let cm_config_name = name_utils::build_resource_name(
                APP_NAME,
                &self.context.name(),
                role,
                Some(group),
                None,
                Some(CONFIG_MAP_TYPE_CONFIG),
            )?;

            let mut cm_config_data = BTreeMap::new();
            if let Some(repo_reference) = config.get(REPO_RULE_REFERENCE) {
                cm_config_data.insert(CONFIG_FILE.to_string(), build_config_file(repo_reference));
            }

            let cm_config = configmap::build_config_map(
                &self.context.resource,
                &cm_config_name,
                &self.context.namespace(),
                cm_config_labels,
                cm_config_data,
            )?;

            config_maps.insert(
                CONFIG_MAP_TYPE_CONFIG,
                configmap::create_config_map(&self.context.client, cm_config).await?,
            );
        }

        Ok(config_maps)
    }

    /// Creates the pod required for the opa instance.
    ///
    /// # Arguments
    ///
    /// - `role` - The OPA role.
    /// - `group` - The role group.
    /// - `node_name` - The node name for this pod.
    /// - `config_maps` - The config maps and respective types required for this pod.
    /// - `validated_config` - The validated product config.
    ///
    async fn create_pod(
        &self,
        role: &str,
        group: &str,
        node_name: &str,
        config_maps: &HashMap<&'static str, ConfigMap>,
        validated_config: &HashMap<PropertyNameKind, BTreeMap<String, String>>,
    ) -> Result<Pod, Error> {
        let mut env_vars = vec![];
        let mut start_command = vec![];
        let mut port = None;

        for (property_name_kind, config) in validated_config {
            match property_name_kind {
                PropertyNameKind::Env => {
                    for (property_name, property_value) in config {
                        if property_name.is_empty() {
                            warn!("Received empty property_name for ENV... skipping");
                            continue;
                        }

                        env_vars.push(EnvVar {
                            name: property_name.clone(),
                            value: Some(property_value.to_string()),
                            value_from: None,
                        });
                    }
                }
                PropertyNameKind::Cli => {
                    port = config.get(PORT);
                    start_command = build_opa_start_command(port);
                }
                _ => {}
            }
        }

        let pod_name = name_utils::build_resource_name(
            APP_NAME,
            &self.context.name(),
            role,
            Some(group),
            Some(node_name),
            None,
        )?;

        let mut container_builder = ContainerBuilder::new(APP_NAME);
        container_builder.image(format!(
            "{}:{}",
            APP_NAME,
            &self.context.resource.spec.version.to_string()
        ));
        container_builder.command(start_command);
        container_builder.add_env_vars(env_vars);

        // Add one mount for the config directory
        if let Some(config_map_data) = config_maps.get(CONFIG_MAP_TYPE_CONFIG) {
            if let Some(name) = config_map_data.metadata.name.as_ref() {
                container_builder.add_configmapvolume(name, "conf".to_string());
            } else {
                return Err(error::Error::MissingConfigMapNameError {
                    cm_type: CONFIG_MAP_TYPE_CONFIG,
                });
            }
        } else {
            return Err(error::Error::MissingConfigMapError {
                cm_type: CONFIG_MAP_TYPE_CONFIG,
                pod_name,
            });
        }

        let mut annotations = BTreeMap::new();
        // only add metrics container port and annotation if available
        if let Some(metrics_port) = port {
            annotations.insert(SHOULD_BE_SCRAPED.to_string(), "true".to_string());
            let parsed_port = metrics_port.parse()?;
            // with OPA, there is only one port available
            // we expose that port twice: once for metrics and once for the clients
            container_builder.add_container_port(
                ContainerPortBuilder::new(parsed_port)
                    .name("metrics")
                    .build(),
            );
            container_builder.add_container_port(
                ContainerPortBuilder::new(parsed_port)
                    .name("client")
                    .build(),
            );
        }

        let pod_labels = get_recommended_labels(
            &self.context.resource,
            APP_NAME,
            &self.context.resource.spec.version.to_string(),
            role,
            group,
        );

        let pod = PodBuilder::new()
            .metadata(
                ObjectMetaBuilder::new()
                    .generate_name(pod_name)
                    .namespace(&self.context.client.default_namespace)
                    .with_labels(pod_labels)
                    .with_annotations(annotations)
                    .ownerreference_from_resource(&self.context.resource, Some(true), Some(true))?
                    .build()?,
            )
            .add_stackable_agent_tolerations()
            .add_container(container_builder.build())
            .node_name(node_name)
            .build()?;

        Ok(self.context.client.create(&pod).await?)
    }

    async fn delete_all_pods(&self) -> OperatorResult<ReconcileFunctionAction> {
        for pod in &self.existing_pods {
            self.context.client.delete(pod).await?;
        }
        Ok(ReconcileFunctionAction::Done)
    }
}

impl ReconciliationState for OpaState {
    type Error = error::Error;

    fn reconcile(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<ReconcileFunctionAction, Self::Error>> + Send + '_>>
    {
        info!("========================= Starting reconciliation =========================");

        Box::pin(async move {
            self.context
                .handle_deletion(Box::pin(self.delete_all_pods()), FINALIZER_NAME, true)
                .await?
                .then(self.context.delete_illegal_pods(
                    self.existing_pods.as_slice(),
                    &self.required_pod_labels(),
                    ContinuationStrategy::OneRequeue,
                ))
                .await?
                .then(
                    self.context
                        .wait_for_terminating_pods(self.existing_pods.as_slice()),
                )
                .await?
                .then(
                    self.context
                        .wait_for_running_and_ready_pods(self.existing_pods.as_slice()),
                )
                .await?
                .then(self.context.delete_excess_pods(
                    list_eligible_nodes_for_role_and_group(&self.eligible_nodes).as_slice(),
                    &self.existing_pods,
                    ContinuationStrategy::OneRequeue,
                ))
                .await?
                .then(self.create_missing_pods())
                .await
        })
    }
}

struct OpaStrategy {
    config: Arc<ProductConfigManager>,
}

impl OpaStrategy {
    pub fn new(config: ProductConfigManager) -> OpaStrategy {
        OpaStrategy {
            config: Arc::new(config),
        }
    }
}

#[async_trait]
impl ControllerStrategy for OpaStrategy {
    type Item = OpenPolicyAgent;
    type State = OpaState;
    type Error = error::Error;

    async fn init_reconcile_state(
        &self,
        context: ReconciliationContext<Self::Item>,
    ) -> Result<Self::State, Self::Error> {
        let existing_pods = context
            .list_owned(build_common_labels_for_all_managed_resources(
                APP_NAME,
                &context.resource.name(),
            ))
            .await?;
        trace!("Found [{}] pods", existing_pods.len());

        let mut eligible_nodes = HashMap::new();

        eligible_nodes.insert(
            OpaRole::Server.to_string(),
            role_utils::find_nodes_that_fit_selectors(
                &context.client,
                None,
                &context.resource.spec.servers,
            )
            .await?,
        );

        Ok(OpaState {
            validated_role_config: validated_product_config(&context.resource, &self.config)?,
            context,
            existing_pods,
            eligible_nodes,
        })
    }
}

/// Validates the provided custom resource configuration fpr the provided roles with the
/// product-config.
pub fn validated_product_config(
    resource: &OpenPolicyAgent,
    product_config: &ProductConfigManager,
) -> OperatorResult<ValidatedRoleConfigByPropertyKind> {
    let mut roles = HashMap::new();
    roles.insert(
        OpaRole::Server.to_string(),
        (
            vec![
                PropertyNameKind::File(CONFIG_FILE.to_string()),
                PropertyNameKind::Cli,
            ],
            resource.spec.servers.clone().into(),
        ),
    );

    let role_config = transform_all_roles_to_config(resource, roles);

    validate_all_roles_and_groups_config(
        &resource.spec.version.to_string(),
        &role_config,
        product_config,
        false,
        false,
    )
}

/// This creates an instance of a [`Controller`] which waits for incoming events and reconciles them.
///
/// This is an async method and the returned future needs to be consumed to make progress.
pub async fn create_controller(client: Client, product_config_path: &str) -> OperatorResult<()> {
    let opa_api: Api<OpenPolicyAgent> = client.get_all_api();
    let pods_api: Api<Pod> = client.get_all_api();
    let configmaps_api: Api<ConfigMap> = client.get_all_api();

    let controller = Controller::new(opa_api)
        .owns(pods_api, ListParams::default())
        .owns(configmaps_api, ListParams::default());

    let product_config = ProductConfigManager::from_yaml_file(product_config_path).unwrap();

    let strategy = OpaStrategy::new(product_config);

    controller
        .run(client, strategy, Duration::from_secs(10))
        .await;

    Ok(())
}

fn build_config_file(repo_rule_reference: &str) -> String {
    format!(
        "
services:
  - name: stackable
    url: {}

bundles:
  stackable:
    service: stackable
    resource: opa/bundle.tar.gz
    persist: true
    polling:
      min_delay_seconds: 10
      max_delay_seconds: 20",
        repo_rule_reference
    )
}

fn build_opa_start_command(port: Option<&String>) -> Vec<String> {
    let mut command = vec![String::from("./opa run")];

    // --server
    command.push("-s".to_string());

    if let Some(port) = port {
        // --addr
        command.push(format!("-a 0.0.0.0:{}", port))
    }

    // --config-file
    command.push("-c {{configroot}}/conf/config.yaml".to_string());

    command
}
