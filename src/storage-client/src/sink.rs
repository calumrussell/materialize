// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use mz_kafka_util::client::{MzClientContext, DEFAULT_FETCH_METADATA_TIMEOUT};
use mz_ore::collections::CollectionExt;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, ResourceSpecifier, TopicReplication};
use rdkafka::ClientContext;
use tracing::warn;

use crate::types::connections::inline::ReferencedConnection;
use crate::types::connections::ConnectionContext;
use crate::types::sinks::{
    KafkaConsistencyConfig, KafkaSinkConnection, KafkaSinkConnectionBuilder,
    KafkaSinkConnectionRetention, KafkaSinkFormat, KafkaSinkProgressConnection,
    PublishedSchemaInfo, StorageSinkConnection, StorageSinkConnectionBuilder,
};

/// Build a sink connection.
// N.B.: We don't want to use a `StorageError` here because some of those variants should not be
// infinitely retried -- and we don't one to unintentionally be introduced in this function.
pub async fn build_sink_connection(
    builder: StorageSinkConnectionBuilder,
    // TODO: The entire sink connection pipeline needs to be refactored and this
    // will be removed as part of that.
    referenced_builder: StorageSinkConnectionBuilder<ReferencedConnection>,
    connection_context: ConnectionContext,
) -> Result<StorageSinkConnection<ReferencedConnection>, anyhow::Error> {
    match (builder, referenced_builder) {
        (StorageSinkConnectionBuilder::Kafka(k), StorageSinkConnectionBuilder::Kafka(rk)) => {
            build_kafka(k, rk, connection_context).await
        }
    }
}

struct TopicConfigs {
    partition_count: i32,
    replication_factor: i32,
}

async fn discover_topic_configs<C: ClientContext>(
    client: &AdminClient<C>,
    topic: &str,
) -> Result<TopicConfigs, anyhow::Error> {
    let mut partition_count = -1;
    let mut replication_factor = -1;

    let metadata = client
        .inner()
        .fetch_metadata(None, DEFAULT_FETCH_METADATA_TIMEOUT)
        .with_context(|| {
            format!(
                "error fetching metadata when creating new topic {} for sink",
                topic
            )
        })?;

    if metadata.brokers().len() == 0 {
        Err(anyhow!("zero brokers discovered in metadata request"))?;
    }

    let broker = metadata.brokers()[0].id();
    let configs = client
        .describe_configs(
            &[ResourceSpecifier::Broker(broker)],
            &AdminOptions::new().request_timeout(Some(Duration::from_secs(5))),
        )
        .await
        .with_context(|| {
            format!(
                "error fetching configuration from broker {} when creating new topic {} for sink",
                broker, topic
            )
        })?;

    if configs.len() != 1 {
        Err(anyhow!(
                "error creating topic {} for sink: broker {} returned {} config results, but one was expected",
                topic,
                broker,
                configs.len()
            ))?;
    }

    let config = configs.into_element().map_err(|e| {
        anyhow!(
            "error reading broker configuration when creating topic {} for sink: {}",
            topic,
            e
        )
    })?;

    if config.entries.is_empty() {
        bail!("read empty custer configuration; do we have DescribeConfigs permissions?")
    }

    for entry in config.entries {
        if entry.name == "num.partitions" && partition_count == -1 {
            if let Some(s) = entry.value {
                partition_count = s.parse::<i32>().with_context(|| {
                    format!(
                        "default partition count {} cannot be parsed into an integer",
                        s
                    )
                })?;
            }
        } else if entry.name == "default.replication.factor" && replication_factor == -1 {
            if let Some(s) = entry.value {
                replication_factor = s.parse::<i32>().with_context(|| {
                    format!(
                        "default replication factor {} cannot be parsed into an integer",
                        s
                    )
                })?;
            }
        }
    }

    Ok(TopicConfigs {
        partition_count,
        replication_factor,
    })
}

async fn ensure_kafka_topic<C>(
    client: &AdminClient<C>,
    topic: &str,
    mut partition_count: i32,
    mut replication_factor: i32,
    retention: KafkaSinkConnectionRetention,
) -> Result<(), anyhow::Error>
where
    C: ClientContext,
{
    // if either partition count or replication factor should be defaulted to the broker's config
    // (signaled by a value of -1), explicitly poll the broker to discover the defaults.
    // Newer versions of Kafka can instead send create topic requests with -1 and have this happen
    // behind the scenes, but this is unsupported and will result in errors on pre-2.4 Kafka.
    if partition_count == -1 || replication_factor == -1 {
        match discover_topic_configs(client, topic).await {
            Ok(configs) => {
                if partition_count == -1 {
                    partition_count = configs.partition_count;
                }
                if replication_factor == -1 {
                    replication_factor = configs.replication_factor;
                }
            }
            Err(e) => {
                // Since recent versions of Kafka can handle an explicit -1 config, this
                // request will probably still succeed. Logging anyways for visibility.
                warn!("Failed to discover default values for topic configs: {e}");
            }
        };
    }

    let mut kafka_topic = NewTopic::new(
        topic,
        partition_count,
        TopicReplication::Fixed(replication_factor),
    );

    let retention_ms_str = retention.duration.map(|d| d.to_string());
    let retention_bytes_str = retention.bytes.map(|s| s.to_string());
    if let Some(ref retention_ms) = retention_ms_str {
        kafka_topic = kafka_topic.set("retention.ms", retention_ms);
    }
    if let Some(ref retention_bytes) = retention_bytes_str {
        kafka_topic = kafka_topic.set("retention.bytes", retention_bytes);
    }

    mz_kafka_util::admin::ensure_topic(
        client,
        &AdminOptions::new().request_timeout(Some(Duration::from_secs(5))),
        &kafka_topic,
    )
    .await
    .with_context(|| format!("Error creating topic {} for sink", topic))?;

    Ok(())
}

/// Publish value and optional key schemas for a given topic.
///
/// TODO(benesch): do we need to delete the Kafka topic if publishing the
/// schema fails?
async fn publish_kafka_schemas(
    ccsr: &mz_ccsr::Client,
    topic: &str,
    key_schema: Option<&str>,
    key_schema_type: Option<mz_ccsr::SchemaType>,
    value_schema: &str,
    value_schema_type: mz_ccsr::SchemaType,
) -> Result<(Option<i32>, i32), anyhow::Error> {
    let value_schema_id = ccsr
        .publish_schema(
            &format!("{}-value", topic),
            value_schema,
            value_schema_type,
            &[],
        )
        .await
        .context("unable to publish value schema to registry in kafka sink")?;

    let key_schema_id = if let Some(key_schema) = key_schema {
        let key_schema_type =
            key_schema_type.ok_or_else(|| anyhow!("expected schema type for key schema"))?;
        Some(
            ccsr.publish_schema(&format!("{}-key", topic), key_schema, key_schema_type, &[])
                .await
                .context("unable to publish key schema to registry in kafka sink")?,
        )
    } else {
        None
    };

    Ok((key_schema_id, value_schema_id))
}

async fn build_kafka(
    builder: KafkaSinkConnectionBuilder,
    referenced_builder: KafkaSinkConnectionBuilder<ReferencedConnection>,
    connection_context: ConnectionContext,
) -> Result<StorageSinkConnection<ReferencedConnection>, anyhow::Error> {
    // Create Kafka topic.
    let client: AdminClient<_> = builder
        .connection
        .create_with_context(
            &connection_context,
            MzClientContext::default(),
            &BTreeMap::new(),
        )
        .await
        .context("creating admin client failed")?;
    ensure_kafka_topic(
        &client,
        &builder.topic_name,
        builder.partition_count,
        builder.replication_factor,
        builder.retention,
    )
    .await
    .context("error registering kafka topic for sink")?;

    let published_schema_info = match builder.format {
        KafkaSinkFormat::Avro {
            key_schema,
            value_schema,
            csr_connection,
            ..
        } => {
            let ccsr = csr_connection.connect(&connection_context).await?;
            let (key_schema_id, value_schema_id) = publish_kafka_schemas(
                &ccsr,
                &builder.topic_name,
                key_schema.as_deref(),
                Some(mz_ccsr::SchemaType::Avro),
                &value_schema,
                mz_ccsr::SchemaType::Avro,
            )
            .await
            .context("error publishing kafka schemas for sink")?;
            Some(PublishedSchemaInfo {
                key_schema_id,
                value_schema_id,
            })
        }
        KafkaSinkFormat::Json => None,
    };

    let progress = match builder.consistency_config {
        KafkaConsistencyConfig::Progress { topic } => {
            ensure_kafka_topic(
                &client,
                &topic,
                1,
                builder.replication_factor,
                KafkaSinkConnectionRetention::default(),
            )
            .await
            .context("error registering kafka consistency topic for sink")?;

            KafkaSinkProgressConnection { topic }
        }
    };

    Ok(StorageSinkConnection::Kafka(KafkaSinkConnection {
        connection: referenced_builder.connection,
        connection_id: builder.connection_id,
        topic: builder.topic_name,
        relation_key_indices: builder.relation_key_indices,
        key_desc_and_indices: builder.key_desc_and_indices,
        value_desc: builder.value_desc,
        published_schema_info,
        progress,
        fuel: builder.fuel,
    }))
}
