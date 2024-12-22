// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use api::rpc_module::RpcModule;
use jsonrpsee::server::{RpcServiceBuilder, ServerBuilder};
use metrics::middleware::MetricsLayer;
use metrics::{MetricsService, RpcMetrics};
use serde_json::json;
use sui_open_rpc::Project;
use tokio::join;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_layer::Identity;
use tracing::info;

use crate::api::{governance::Governance, Reader};
use crate::args::Args;

mod api;
pub mod args;
mod metrics;

#[derive(clap::Args, Debug, Clone)]
pub struct RpcArgs {
    /// Address to listen to for incoming JSON-RPC connections.
    #[clap(long, default_value_t = Self::default().rpc_listen_address)]
    rpc_listen_address: SocketAddr,

    /// Address to serve Prometheus metrics from.
    #[clap(long, default_value_t = Self::default().metrics_address)]
    metrics_address: SocketAddr,

    /// The maximum number of concurrent connections to accept.
    #[clap(long, default_value_t = Self::default().max_rpc_connections)]
    max_rpc_connections: u32,
}

pub struct RpcService {
    /// The address that the server will start listening for requests on, when it is run.
    rpc_listen_address: SocketAddr,

    /// A partially built/configured JSON-RPC server.
    server: ServerBuilder<Identity, Identity>,

    /// Metrics for the RPC service.
    metrics: Arc<RpcMetrics>,

    /// Service for serving Prometheus metrics.
    metrics_service: MetricsService,

    /// All the methods added to the server so far.
    modules: jsonrpsee::RpcModule<()>,

    /// Description of the schema served by this service.
    schema: Project,

    /// Cancellation token controlling all services.
    cancel: CancellationToken,
}

impl RpcService {
    /// Create a new instance of the JSON-RPC service, configured by `rpc_args`. The service will
    /// not accept connections until [Self::run] is called.
    pub fn new(rpc_args: RpcArgs, cancel: CancellationToken) -> anyhow::Result<Self> {
        let RpcArgs {
            rpc_listen_address,
            metrics_address,
            max_rpc_connections,
        } = rpc_args;

        let (metrics, metrics_service) = MetricsService::new(metrics_address, cancel.clone())
            .context("Failed to create metrics service")?;

        let server = ServerBuilder::new()
            .http_only()
            .max_connections(max_rpc_connections);

        let schema = Project::new(
            env!("CARGO_PKG_VERSION"),
            "Sui JSON-RPC",
            "A JSON-RPC API for interacting with the Sui blockchain.",
            "Mysten Labs",
            "https://mystenlabs.com",
            "build@mystenlabs.com",
            "Apache-2.0",
            "https://raw.githubusercontent.com/MystenLabs/sui/main/LICENSE",
        );

        Ok(Self {
            rpc_listen_address,
            server,
            metrics,
            metrics_service,
            modules: jsonrpsee::RpcModule::new(()),
            schema,
            cancel,
        })
    }

    /// Return a copy of the metrics.
    pub fn metrics(&self) -> Arc<RpcMetrics> {
        self.metrics.clone()
    }

    /// Add an `RpcModule` to the service. The module's methods are combined with the existing
    /// methods registered on the service, and the operation will fail if there is any overlap.
    pub fn add_module(&mut self, module: impl RpcModule) -> anyhow::Result<()> {
        self.schema.add_module(module.schema());
        self.modules
            .merge(module.into_impl().remove_context())
            .context("Failed to add module because of a name conflict")
    }

    /// Start the service (it will accept connections) and return a handle that will resolve when
    /// the service stops.
    pub async fn run(self) -> anyhow::Result<JoinHandle<()>> {
        let Self {
            rpc_listen_address,
            server,
            metrics,
            metrics_service,
            mut modules,
            schema,
            cancel,
        } = self;

        info!("Starting JSON-RPC service on {rpc_listen_address}",);
        info!("Serving schema: {}", serde_json::to_string_pretty(&schema)?);

        // Add a method to serve the schema to clients.
        modules
            .register_method("rpc.discover", move |_, _, _| json!(schema.clone()))
            .context("Failed to add schema discovery method")?;

        let h_metrics = metrics_service
            .run()
            .await
            .context("Failed to start metrics service")?;

        let middleware = RpcServiceBuilder::new().layer(MetricsLayer::new(
            metrics,
            modules.method_names().map(|n| n.to_owned()).collect(),
        ));

        let handle = server
            .set_rpc_middleware(middleware)
            .build(rpc_listen_address)
            .await
            .context("Failed to bind JSON-RPC service")?
            .start(modules);

        // Set-up a helper task that will tear down the RPC service when the cancellation token is
        // triggered.
        let cancel_handle = handle.clone();
        let cancel_cancel = cancel.clone();
        let h_cancel = tokio::spawn(async move {
            cancel_cancel.cancelled().await;
            cancel_handle.stop()
        });

        Ok(tokio::spawn(async move {
            handle.stopped().await;
            cancel.cancel();
            let _ = join!(h_cancel, h_metrics);
        }))
    }
}

impl Default for RpcArgs {
    fn default() -> Self {
        Self {
            rpc_listen_address: "0.0.0.0:6000".parse().unwrap(),
            metrics_address: "0.0.0.0:9184".parse().unwrap(),
            max_rpc_connections: 100,
        }
    }
}

pub async fn start_rpc(args: Args) -> anyhow::Result<()> {
    let Args { db_args, rpc_args } = args;

    let cancel = CancellationToken::new();
    let mut rpc = RpcService::new(rpc_args, cancel).context("Failed to create RPC service")?;

    let reader = Reader::new(db_args, rpc.metrics()).await?;

    rpc.add_module(Governance(reader.clone()))?;

    let h_rpc = rpc.run().await.context("Failed to start RPC service")?;
    let _ = h_rpc.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        time::Duration,
    };

    use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::error::METHOD_NOT_FOUND_CODE};
    use reqwest::Client;
    use serde_json::{json, Value};
    use sui_open_rpc::Module;
    use sui_open_rpc_macros::open_rpc;
    use sui_pg_db::temp::get_available_port;

    use super::*;

    #[tokio::test]
    async fn test_add_module() {
        let mut rpc = test_service().await;

        rpc.add_module(Foo).unwrap();

        assert_eq!(
            BTreeSet::from_iter(rpc.modules.method_names()),
            BTreeSet::from_iter(["test_bar"]),
        )
    }

    #[tokio::test]
    async fn test_add_module_multiple_methods() {
        let mut rpc = test_service().await;

        rpc.add_module(Bar).unwrap();

        assert_eq!(
            BTreeSet::from_iter(rpc.modules.method_names()),
            BTreeSet::from_iter(["test_bar", "test_baz"]),
        )
    }

    #[tokio::test]
    async fn test_add_multiple_modules() {
        let mut rpc = test_service().await;

        rpc.add_module(Foo).unwrap();
        rpc.add_module(Baz).unwrap();

        assert_eq!(
            BTreeSet::from_iter(rpc.modules.method_names()),
            BTreeSet::from_iter(["test_bar", "test_baz"]),
        )
    }

    #[tokio::test]
    async fn test_add_module_conflict() {
        let mut rpc = test_service().await;

        rpc.add_module(Foo).unwrap();
        assert!(rpc.add_module(Bar).is_err(),)
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let cancel = CancellationToken::new();
        let rpc = RpcService::new(
            RpcArgs {
                rpc_listen_address: test_listen_address(),
                metrics_address: test_listen_address(),
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();

        let handle = rpc.run().await.unwrap();

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("Shutdown should not timeout")
            .expect("Shutdown should succeed");
    }

    #[tokio::test]
    async fn test_rpc_discovery() {
        let cancel = CancellationToken::new();
        let rpc_listen_address = test_listen_address();

        let mut rpc = RpcService::new(
            RpcArgs {
                rpc_listen_address,
                metrics_address: test_listen_address(),
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();

        rpc.add_module(Foo).unwrap();
        rpc.add_module(Baz).unwrap();

        let handle = rpc.run().await.unwrap();

        let url = format!("http://{}/", rpc_listen_address);
        let client = Client::new();

        let resp: Value = client
            .post(&url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "rpc.discover",
                "id": 1,
            }))
            .send()
            .await
            .expect("Request should succeed")
            .json()
            .await
            .expect("Deserialization should succeed");

        assert_eq!(resp["result"]["info"]["title"], "Sui JSON-RPC");
        assert_eq!(
            resp["result"]["methods"],
            json!([
                {
                    "name": "test_bar",
                    "tags": [{
                        "name": "Test API"
                    }],
                    "params": [],
                    "result": {
                        "name": "u64",
                        "required": true,
                        "schema": {
                            "type": "integer",
                            "format": "uint64",
                            "minimum": 0.0
                        }
                    }
                },
                {
                    "name": "test_baz",
                    "tags": [{
                        "name": "Test API"
                    }],
                    "params": [],
                    "result": {
                        "name": "u64",
                        "required": true,
                        "schema": {
                            "type": "integer",
                            "format": "uint64",
                            "minimum": 0.0
                        }
                    }
                }
            ])
        );

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("Shutdown should not timeout")
            .expect("Shutdown should succeed");
    }

    #[tokio::test]
    async fn test_request_metrics() {
        let cancel = CancellationToken::new();
        let rpc_listen_address = test_listen_address();

        let mut rpc = RpcService::new(
            RpcArgs {
                rpc_listen_address,
                metrics_address: test_listen_address(),
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();

        rpc.add_module(Foo).unwrap();

        let metrics = rpc.metrics();
        let handle = rpc.run().await.unwrap();

        let url = format!("http://{}/", rpc_listen_address);
        let client = Client::new();

        client
            .post(&url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "test_bar",
                "id": 1,
            }))
            .send()
            .await
            .expect("Request should succeed");

        client
            .post(&url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "test_baz",
                "id": 1,
            }))
            .send()
            .await
            .expect("Request should succeed");

        assert_eq!(
            metrics
                .requests_received
                .with_label_values(&["test_bar"])
                .get(),
            1
        );

        assert_eq!(
            metrics
                .requests_succeeded
                .with_label_values(&["test_bar"])
                .get(),
            1
        );

        assert_eq!(
            metrics
                .requests_received
                .with_label_values(&["<UNKNOWN>"])
                .get(),
            1
        );

        assert_eq!(
            metrics
                .requests_succeeded
                .with_label_values(&["<UNKNOWN>"])
                .get(),
            0
        );

        assert_eq!(
            metrics
                .requests_failed
                .with_label_values(&["<UNKNOWN>", &format!("{METHOD_NOT_FOUND_CODE}")])
                .get(),
            1
        );

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("Shutdown should not timeout")
            .expect("Shutdown should succeed");
    }

    // Test Helpers

    #[open_rpc(namespace = "test", tag = "Test API")]
    #[rpc(server, namespace = "test")]
    trait FooApi {
        #[method(name = "bar")]
        fn bar(&self) -> RpcResult<u64>;
    }

    #[open_rpc(namespace = "test", tag = "Test API")]
    #[rpc(server, namespace = "test")]
    trait BarApi {
        #[method(name = "bar")]
        fn bar(&self) -> RpcResult<u64>;

        #[method(name = "baz")]
        fn baz(&self) -> RpcResult<u64>;
    }

    #[open_rpc(namespace = "test", tag = "Test API")]
    #[rpc(server, namespace = "test")]
    trait BazApi {
        #[method(name = "baz")]
        fn baz(&self) -> RpcResult<u64>;
    }

    struct Foo;
    struct Bar;
    struct Baz;

    impl FooApiServer for Foo {
        fn bar(&self) -> RpcResult<u64> {
            Ok(42)
        }
    }

    impl BarApiServer for Bar {
        fn bar(&self) -> RpcResult<u64> {
            Ok(43)
        }

        fn baz(&self) -> RpcResult<u64> {
            Ok(44)
        }
    }

    impl BazApiServer for Baz {
        fn baz(&self) -> RpcResult<u64> {
            Ok(45)
        }
    }

    impl RpcModule for Foo {
        fn schema(&self) -> Module {
            FooApiOpenRpc::module_doc()
        }

        fn into_impl(self) -> jsonrpsee::RpcModule<Self> {
            self.into_rpc()
        }
    }

    impl RpcModule for Bar {
        fn schema(&self) -> Module {
            BarApiOpenRpc::module_doc()
        }

        fn into_impl(self) -> jsonrpsee::RpcModule<Self> {
            self.into_rpc()
        }
    }

    impl RpcModule for Baz {
        fn schema(&self) -> Module {
            BazApiOpenRpc::module_doc()
        }

        fn into_impl(self) -> jsonrpsee::RpcModule<Self> {
            self.into_rpc()
        }
    }

    fn test_listen_address() -> SocketAddr {
        let port = get_available_port();
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    async fn test_service() -> RpcService {
        let cancel = CancellationToken::new();
        RpcService::new(
            RpcArgs {
                rpc_listen_address: test_listen_address(),
                metrics_address: test_listen_address(),
                ..Default::default()
            },
            cancel,
        )
        .expect("Failed to create test JSON-RPC service")
    }
}
