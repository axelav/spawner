use anyhow::Result;
use async_nats::jetstream::consumer::DeliverPolicy;
use clap::{Parser, Subcommand};
use colored::Colorize;
use plane_core::{
    messages::{
        agent::{
            BackendStateMessage, DockerExecutableConfig, DroneStatusMessage, ResourceLimits,
            TerminationRequest,
        },
        dns::SetDnsRecord,
        scheduler::{DrainDrone, ScheduleRequest, ScheduleResponse},
    },
    nats_connection::NatsConnectionSpec,
    types::{BackendId, ClusterName, DroneId},
};
use std::{collections::HashMap, time::Duration};

#[derive(Parser)]
struct Opts {
    #[clap(long)]
    nats: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    ListDrones,
    ListDns,
    Spawn {
        cluster: String,
        image: String,
        /// Grace period with no connections before shutting down the drone.
        #[clap(long, default_value = "300")]
        timeout: u64,
    },
    Status {
        backend: Option<String>,
    },
    Drain {
        drone: String,
        cluster: String,

        /// Cancel draining and allow a drone to accept backends again.
        #[clap(long)]
        cancel: bool,
    },
    Terminate {
        cluster: String,
        backend: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    tracing_subscriber::fmt().init();

    let nats = NatsConnectionSpec::from_url(opts.nats.as_deref().unwrap_or("nats://localhost"))?
        .connect()
        .await?;

    match opts.command {
        Command::Status { backend } => {
            let mut sub = if let Some(backend) = backend {
                nats.subscribe_jetstream(BackendStateMessage::subscribe_subject(&BackendId::new(
                    backend,
                )))
                .await?
            } else {
                nats.subscribe_jetstream(BackendStateMessage::wildcard_subject())
                    .await?
            };

            while let Some(message) = sub.next().await {
                println!(
                    "{}\t{}\t{}",
                    message.backend.to_string().bright_cyan(),
                    message.state.to_string().bright_magenta(),
                    message.time.to_string().blue()
                );
            }
        }
        Command::ListDrones => {
            let drones = nats
                .get_all(
                    &DroneStatusMessage::subscribe_subject(),
                    DeliverPolicy::LastPerSubject,
                )
                .await?;

            println!("Found {} drones:", drones.len());

            for drone in drones {
                println!(
                    "{}\t{}",
                    drone.drone_id.to_string().bright_green(),
                    drone.cluster.to_string().bright_cyan()
                );
            }
        }
        Command::Spawn {
            image,
            cluster,
            timeout,
        } => {
            let result = nats
                .request(&ScheduleRequest {
                    backend_id: None,
                    cluster: ClusterName::new(&cluster),
                    max_idle_secs: Duration::from_secs(timeout),
                    metadata: HashMap::new(),
                    executable: DockerExecutableConfig {
                        image,
                        env: HashMap::new(),
                        credentials: None,
                        resource_limits: ResourceLimits::default(),
                    },
                    require_bearer_token: false,
                })
                .await?;

            match result {
                ScheduleResponse::Scheduled {
                    drone,
                    backend_id,
                    bearer_token,
                } => {
                    let url = format!("https://{}.{}", backend_id, cluster);

                    println!("Backend scheduled.");
                    println!("URL: {}", url.bright_green());
                    println!("Drone: {}", drone.to_string().bright_blue());
                    println!("Backend ID: {}", backend_id.to_string().bright_blue());
                    if let Some(bearer_token) = bearer_token {
                        println!("Bearer token: {}", bearer_token.bright_blue());
                    }
                }
                ScheduleResponse::NoDroneAvailable => tracing::error!(
                    %cluster,
                    "Could not schedule backend because no drone was available for cluster."
                ),
            }
        }
        Command::ListDns => {
            let results = nats
                .get_all(
                    &SetDnsRecord::subscribe_subject(),
                    DeliverPolicy::LastPerSubject,
                )
                .await?;

            println!("Found {} DNS records:", results.len());

            for result in results {
                println!(
                    "{}.{}\t{}\t{}",
                    result.name.to_string().bright_magenta(),
                    result.cluster.to_string().bright_blue(),
                    result.kind.to_string().bright_cyan(),
                    result.value.to_string().bold()
                );
            }
        }
        Command::Terminate {
            cluster, backend, ..
        } => {
            nats.request(&TerminationRequest {
                backend_id: BackendId::new(backend),
                cluster_id: ClusterName::new(&cluster),
            })
            .await?;

            println!("{}", "Terminated successfully".bright_green());
        }
        Command::Drain {
            drone,
            cluster,
            cancel,
        } => {
            let drain = !cancel;
            nats.request(&DrainDrone {
                cluster: ClusterName::new(&cluster),
                drone: DroneId::new(drone),
                drain,
            })
            .await?;

            if drain {
                println!("{}", "Draining started on drone.".bright_green());
            } else {
                println!("{}", "Draining cancelled on drone.".bright_green());
            }
        }
    }

    Ok(())
}
