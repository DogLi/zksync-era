#![feature(generic_const_exprs)]

use std::time::Duration;

use anyhow::Context as _;
use structopt::StructOpt;
use tokio::sync::{oneshot, watch};
use zksync_config::configs::{
    fri_prover_group::FriProverGroupConfig, DatabaseSecrets, FriProverConfig,
    FriWitnessVectorGeneratorConfig, ObservabilityConfig,
};
use zksync_env_config::{object_store::ProverObjectStoreConfig, FromEnv};
use zksync_object_store::ObjectStoreFactory;
use zksync_prover_dal::ConnectionPool;
use zksync_prover_fri_types::PROVER_PROTOCOL_SEMANTIC_VERSION;
use zksync_prover_fri_utils::{get_all_circuit_id_round_tuples_for, region_fetcher::get_zone};
use zksync_queued_job_processor::JobProcessor;
use zksync_utils::wait_for_tasks::ManagedTasks;
use zksync_vlog::prometheus::PrometheusExporterConfig;

use crate::generator::WitnessVectorGenerator;

mod generator;
mod metrics;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "zksync_witness_vector_generator",
    about = "Tool for generating witness vectors for circuits"
)]
struct Opt {
    /// Number of times `witness_vector_generator` should be run.
    #[structopt(short = "n", long = "n_iterations")]
    number_of_iterations: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let observability_config =
        ObservabilityConfig::from_env().context("ObservabilityConfig::from_env()")?;
    let log_format: zksync_vlog::LogFormat = observability_config
        .log_format
        .parse()
        .context("Invalid log format")?;

    let mut builder = zksync_vlog::ObservabilityBuilder::new().with_log_format(log_format);
    if let Some(sentry_url) = &observability_config.sentry_url {
        builder = builder
            .with_sentry_url(sentry_url)
            .expect("Invalid Sentry URL")
            .with_sentry_environment(observability_config.sentry_environment);
    }
    if let Some(opentelemetry) = observability_config.opentelemetry {
        builder = builder
            .with_opentelemetry(
                &opentelemetry.level,
                opentelemetry.endpoint,
                "zksync-witness-vector-generator".into(),
            )
            .expect("Invalid OpenTelemetry config");
    }
    let _guard = builder.build();

    let opt = Opt::from_args();
    let config = FriWitnessVectorGeneratorConfig::from_env()
        .context("FriWitnessVectorGeneratorConfig::from_env()")?;
    let specialized_group_id = config.specialized_group_id;
    let exporter_config = PrometheusExporterConfig::pull(config.prometheus_listener_port);

    let database_secrets = DatabaseSecrets::from_env().context("DatabaseSecrets::from_env()")?;
    let pool = ConnectionPool::singleton(database_secrets.prover_url()?)
        .build()
        .await
        .context("failed to build a connection pool")?;
    let object_store_config =
        ProverObjectStoreConfig::from_env().context("ProverObjectStoreConfig::from_env()")?;
    let object_store = ObjectStoreFactory::new(object_store_config.0)
        .create_store()
        .await?;
    let circuit_ids_for_round_to_be_proven = FriProverGroupConfig::from_env()
        .context("FriProverGroupConfig::from_env()")?
        .get_circuit_ids_for_group_id(specialized_group_id)
        .unwrap_or_default();
    let circuit_ids_for_round_to_be_proven =
        get_all_circuit_id_round_tuples_for(circuit_ids_for_round_to_be_proven);
    let fri_prover_config = FriProverConfig::from_env().context("FriProverConfig::from_env()")?;
    let zone_url = &fri_prover_config.zone_read_url;
    let zone = get_zone(zone_url).await.context("get_zone()")?;

    let protocol_version = PROVER_PROTOCOL_SEMANTIC_VERSION;

    let witness_vector_generator = WitnessVectorGenerator::new(
        object_store,
        pool,
        circuit_ids_for_round_to_be_proven.clone(),
        zone.clone(),
        config,
        protocol_version,
        fri_prover_config.max_attempts,
    );

    let (stop_sender, stop_receiver) = watch::channel(false);

    let (stop_signal_sender, stop_signal_receiver) = oneshot::channel();
    let mut stop_signal_sender = Some(stop_signal_sender);
    ctrlc::set_handler(move || {
        if let Some(stop_signal_sender) = stop_signal_sender.take() {
            stop_signal_sender.send(()).ok();
        }
    })
    .expect("Error setting Ctrl+C handler");

    tracing::info!("Starting witness vector generation for group: {} with circuits: {:?} in zone: {} with protocol_version: {:?}", specialized_group_id, circuit_ids_for_round_to_be_proven, zone, protocol_version);

    let tasks = vec![
        tokio::spawn(exporter_config.run(stop_receiver.clone())),
        tokio::spawn(witness_vector_generator.run(stop_receiver, opt.number_of_iterations)),
    ];

    let mut tasks = ManagedTasks::new(tasks);
    tokio::select! {
        _ = tasks.wait_single() => {},
        _ = stop_signal_receiver => {
            tracing::info!("Stop signal received, shutting down");
        }
    };
    stop_sender.send(true).ok();
    tasks.complete(Duration::from_secs(5)).await;
    Ok(())
}
