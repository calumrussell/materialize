# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from materialize.mzcompose import Composition
from materialize.mzcompose.services import (
    Kafka,
    Materialized,
    SchemaRegistry,
    SshBastionHost,
    TestCerts,
    Testdrive,
    Zookeeper,
)

SERVICES = [
    TestCerts(),
    Zookeeper(),
    Kafka(
        depends_on_extra=["test-certs"],
        environment=[
            # Default
            "KAFKA_ZOOKEEPER_CONNECT=zookeeper:2181",
            "KAFKA_CONFLUENT_SUPPORT_METRICS_ENABLE=false",
            "KAFKA_MIN_INSYNC_REPLICAS=1",
            "KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1",
            "KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1",
            "KAFKA_MESSAGE_MAX_BYTES=15728640",
            "KAFKA_REPLICA_FETCH_MAX_BYTES=15728640",
            # For this test
            "KAFKA_SSL_KEYSTORE_FILENAME=kafka.keystore.jks",
            "KAFKA_SSL_KEYSTORE_CREDENTIALS=cert_creds",
            "KAFKA_SSL_KEY_CREDENTIALS=cert_creds",
            "KAFKA_SSL_TRUSTSTORE_FILENAME=kafka.truststore.jks",
            "KAFKA_SSL_TRUSTSTORE_CREDENTIALS=cert_creds",
            "KAFKA_SSL_CLIENT_AUTH=required",
            "KAFKA_SECURITY_INTER_BROKER_PROTOCOL=SSL",
        ],
        listener_type="SSL",
        volumes=["secrets:/etc/kafka/secrets"],
    ),
    SchemaRegistry(
        depends_on_extra=["test-certs"],
        environment=[
            "SCHEMA_REGISTRY_KAFKASTORE_TIMEOUT_MS=10000",
            "SCHEMA_REGISTRY_HOST_NAME=schema-registry",
            "SCHEMA_REGISTRY_LISTENERS=https://0.0.0.0:8081",
            "SCHEMA_REGISTRY_KAFKASTORE_CONNECTION_URL=zookeeper:2181",
            "SCHEMA_REGISTRY_KAFKASTORE_SECURITY_PROTOCOL=SSL",
            "SCHEMA_REGISTRY_KAFKASTORE_SSL_KEYSTORE_LOCATION=/etc/schema-registry/secrets/schema-registry.keystore.jks",
            "SCHEMA_REGISTRY_SSL_KEYSTORE_LOCATION=/etc/schema-registry/secrets/schema-registry.keystore.jks",
            "SCHEMA_REGISTRY_KAFKASTORE_SSL_KEYSTORE_PASSWORD=mzmzmz",
            "SCHEMA_REGISTRY_SSL_KEYSTORE_PASSWORD=mzmzmz",
            "SCHEMA_REGISTRY_KAFKASTORE_SSL_KEY_PASSWORD=mzmzmz",
            "SCHEMA_REGISTRY_SSL_KEY_PASSWORD=mzmzmz",
            "SCHEMA_REGISTRY_KAFKASTORE_SSL_TRUSTSTORE_LOCATION=/etc/schema-registry/secrets/schema-registry.truststore.jks",
            "SCHEMA_REGISTRY_SSL_TRUSTSTORE_LOCATION=/etc/schema-registry/secrets/schema-registry.truststore.jks",
            "SCHEMA_REGISTRY_KAFKASTORE_SSL_TRUSTSTORE_PASSWORD=mzmzmz",
            "SCHEMA_REGISTRY_SSL_TRUSTSTORE_PASSWORD=mzmzmz",
            "SCHEMA_REGISTRY_SCHEMA_REGISTRY_INTER_INSTANCE_PROTOCOL=https",
            "SCHEMA_REGISTRY_SSL_CLIENT_AUTH=true",
            "SCHEMA_REGISTRY_AUTHENTICATION_METHOD=BASIC",
            "SCHEMA_REGISTRY_AUTHENTICATION_ROLES=user",
            "SCHEMA_REGISTRY_AUTHENTICATION_REALM=SchemaRegistry",
            "SCHEMA_REGISTRY_OPTS=-Djava.security.auth.login.config=/etc/schema-registry/jaas_config.conf",
        ],
        volumes=[
            "secrets:/etc/schema-registry/secrets",
            "./jaas_config.conf:/etc/schema-registry/jaas_config.conf",
            "./users.properties:/etc/schema-registry/users.properties",
        ],
        bootstrap_server_type="SSL",
    ),
    Materialized(
        volumes_extra=["secrets:/share/secrets"],
    ),
    Testdrive(
        entrypoint=[
            "bash",
            "-c",
            "cp /share/secrets/ca.crt /usr/local/share/ca-certificates/ca.crt && "
            "update-ca-certificates && "
            "testdrive "
            "--kafka-addr=kafka:9092 "
            "--schema-registry-url=https://schema-registry:8081 "
            "--materialize-url=postgres://materialize@materialized:6875 "
            "--materialize-internal-url=postgres://materialize@materialized:6877 "
            "--cert=/share/secrets/producer.p12 "
            "--cert-password=mzmzmz "
            "--ccsr-password=sekurity "
            "--ccsr-username=materialize "
            '--var=materialized-kafka-key="$$(</share/secrets/materialized-kafka.key)" '
            '--var=materialized-kafka-crt="$$(</share/secrets/materialized-kafka.crt)" '
            '--var=materialized-kafka1-key="$$(</share/secrets/materialized-kafka1.key)" '
            '--var=materialized-kafka1-crt="$$(</share/secrets/materialized-kafka1.crt)" '
            '--var=materialized-schema-registry-key="$$(</share/secrets/materialized-schema-registry.key)" '
            '--var=materialized-schema-registry-crt="$$(</share/secrets/materialized-schema-registry.crt)" '
            '--var=ca-crt="$$(</share/secrets/ca.crt)" '
            '--var=ca-alt-crt="$$(</share/secrets/ca-selective.crt)" '
            '"$$@"',
        ],
        volumes_extra=["secrets:/share/secrets"],
        # Required to install root certs above
        propagate_uid_gid=False,
    ),
    SshBastionHost(),
]


def workflow_default(c: Composition) -> None:
    """Run testdrive against an SSL-enabled Confluent Platform."""
    c.workflow("smoketest")
    c.workflow("ssh-tunnel")


def workflow_smoketest(c: Composition) -> None:
    c.down(destroy_volumes=True)
    c.up("zookeeper", "kafka", "schema-registry", "materialized")
    c.run("testdrive", "multi.td", "smoketest.td")


def workflow_ssh_tunnel(c: Composition) -> None:
    # NOTE(benesch): This workflow and supporting testdrive scripts duplicate
    # too much code with the tests in test/ssh-bastion for my liking, but it
    # was the best I could do on a tight deadline.
    c.down(destroy_volumes=True)
    c.up("zookeeper", "kafka", "schema-registry", "materialized", "ssh-bastion-host")
    c.run("testdrive", "ssh-tunnel-setup.td")
    public_key = c.sql_query("select public_key_1 from mz_ssh_tunnel_connections;")[0][
        0
    ]
    c.exec(
        "ssh-bastion-host",
        "bash",
        "-c",
        f"echo '{public_key}' > /etc/authorized_keys/mz",
    )
    c.run("testdrive", "--no-reset", "ssh-tunnel-test.td")
