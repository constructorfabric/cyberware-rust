//! `cf-evctl` - a minimal event-broker CLI over the SDK REST transport.
//!
//! A kafka/nats-style dev tool: `produce` publishes one event, `consume` joins a
//! group and prints the delivered frames. It is a thin wrapper over `RestBroker`
//! and reaches the broker only through `EventBrokerApi`, so it doubles as a
//! manual end-to-end check of the transport (including both stream framings).

use std::io::{Read, Write};

use clap::{Parser, Subcommand, ValueEnum};
use event_broker_sdk::api::{JoinRequest, Position, SeekPosition, SubscriptionInterest};
use event_broker_sdk::rest::{RestBroker, StreamTransport};
use event_broker_sdk::{ConsumerGroupId, EventBrokerApi};
use event_broker_sdk::models::Event;
use futures_util::StreamExt;
use toolkit_security::SecurityContext;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "cf-evctl", about = "Minimal event-broker CLI over the SDK REST transport")]
struct Cli {
    /// Broker base URL, e.g. https://host:8080
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    broker: String,
    /// Caller tenant id.
    #[arg(long)]
    tenant: Uuid,
    /// Caller principal (subject) id. Defaults to a fresh uuid per run.
    #[arg(long)]
    principal: Option<Uuid>,
    /// Bearer token for the tenant plane.
    #[arg(long)]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Copy, Clone, ValueEnum)]
enum Framing {
    Multipart,
    Sse,
}

impl From<Framing> for StreamTransport {
    fn from(f: Framing) -> Self {
        match f {
            Framing::Multipart => StreamTransport::Multipart,
            Framing::Sse => StreamTransport::Sse,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Publish one event.
    Produce {
        /// Event GTS type id (ends with `~`).
        #[arg(long = "type")]
        type_id: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        subject_type: String,
        #[arg(long, default_value = "cf-evctl")]
        source: String,
        /// Event data as JSON: a literal, `@file`, or `-` for stdin.
        #[arg(long, default_value = "-")]
        data: String,
    },
    /// Join a group and stream its events to stdout.
    Consume {
        /// Consumer group GTS instance id.
        #[arg(long)]
        group: String,
        /// Topic GTS instance id.
        #[arg(long)]
        topic: String,
        /// Event GTS type id (or pattern).
        #[arg(long = "type")]
        type_id: String,
        #[arg(long, value_enum, default_value_t = Framing::Multipart)]
        framing: Framing,
    },
}

fn read_data(arg: &str) -> Result<serde_json::Value, String> {
    let raw = if arg == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("reading stdin: {e}"))?;
        s
    } else if let Some(path) = arg.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?
    } else {
        arg.to_owned()
    };
    serde_json::from_str(&raw).map_err(|e| format!("data is not valid JSON: {e}"))
}

fn security_context(
    tenant: Uuid,
    principal: Option<Uuid>,
    token: Option<String>,
) -> Result<SecurityContext, String> {
    let mut builder = SecurityContext::builder()
        .subject_tenant_id(tenant)
        .subject_id(principal.unwrap_or_else(Uuid::new_v4));
    if let Some(token) = token {
        builder = builder.bearer_token(token);
    }
    builder.build().map_err(|e| format!("building security context: {e}"))
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("cf-evctl: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let ctx = security_context(cli.tenant, cli.principal, cli.token)?;

    match cli.command {
        Command::Produce {
            type_id,
            subject,
            subject_type,
            source,
            data,
        } => {
            let broker = RestBroker::new(cli.broker).map_err(|e| e.to_string())?;
            let event = Event {
                id: Uuid::new_v4(),
                type_id,
                tenant_id: cli.tenant,
                source,
                subject,
                subject_type,
                occurred_at: chrono::Utc::now(),
                trace_parent: None,
                data: Some(read_data(&data)?),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            };
            let outcome = broker.publish(&ctx, &event).await.map_err(|e| e.to_string())?;
            println!("{outcome:?}");
            Ok(())
        }
        Command::Consume {
            group,
            topic,
            type_id,
            framing,
        } => {
            let broker = RestBroker::new(cli.broker)
                .map_err(|e| e.to_string())?
                .with_stream_transport(framing.into());
            let group = ConsumerGroupId::try_from_gts(&group).ok_or_else(|| {
                "group must be an anonymous consumer-group GTS id \
                 (gts.cf.core.events.consumer_group.v1~<uuid>)"
                    .to_owned()
            })?;
            let interest = SubscriptionInterest::builder()
                .topic(topic)
                .tenant_id(cli.tenant)
                .types([type_id])
                .build()
                .map_err(|e| e.to_string())?;
            let assignment = broker
                .join(
                    &ctx,
                    JoinRequest {
                        group,
                        client_agent: "cf-evctl".to_owned(),
                        interests: vec![interest],
                        session_timeout: None,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            // Every assigned partition must have a starting position before the
            // stream opens (else the broker rejects it as unseeded). This is a
            // minimal one-shot CLI, so it seeks each to `earliest`; the Consumer
            // runtime is the thing that persists and resumes real offsets.
            let positions: Vec<SeekPosition> = assignment
                .assigned
                .iter()
                .map(|p| SeekPosition {
                    topic: p.topic.clone(),
                    partition: p.partition,
                    value: Position::Earliest,
                })
                .collect();
            if !positions.is_empty() {
                broker
                    .seek(&ctx, assignment.subscription_id, &positions)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let mut stream = broker
                .stream(&ctx, assignment.subscription_id)
                .await
                .map_err(|e| e.to_string())?;
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(frame) => {
                        // Flush per frame: a streaming CLI's stdout is block-
                        // buffered when piped, so unflushed frames are lost.
                        println!("{frame:?}");
                        let _ = std::io::stdout().flush();
                    }
                    Err(err) => {
                        eprintln!("stream error: {err}");
                        break;
                    }
                }
            }
            Ok(())
        }
    }
}
