# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ file-append path=test.csv
a,b
jack,jill
goofus,gallant

> CREATE SOURCE src
  FROM FILE '${testdrive.temp-dir}/test.csv'
  FORMAT CSV WITH HEADER

> CREATE MATERIALIZED SOURCE src_materialized
  FROM FILE '${testdrive.temp-dir}/test.csv'
  FORMAT CSV WITH HEADER

> CREATE VIEW v1 AS
  SELECT a || b AS c FROM src

> CREATE VIEW v2 AS
  SELECT a || b AS c FROM src_materialized

> CREATE MATERIALIZED VIEW v3 AS
  SELECT a || b AS c FROM src

# We should refuse to create a sink with invalid WITH options

! CREATE SINK invalid_with_option FROM src
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk1'
  WITH (badoption=true)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
unexpected parameters for CREATE SINK: badoption

> SHOW SINKS
name
----

# We should refuse to create a sink with an invalid schema registry URL.
# We use the Kafka address as the invalid schema registry address, as it is
# known to immediately produce an error. Previous attempts used
# `example.com:8080` and `materialize.com:8080` which can blackhole the request
# for minutes rather than surfacing an error in a timely manner.

! CREATE SINK bad_schema_registry FROM src
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk1'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.kafka-addr}'
unable to publish value schema to registry in kafka sink

> SHOW SINKS
name
----

# N.B. it is important to test sinks that depend on sources directly vs. sinks
# that depend on views, as the code paths are different.

> CREATE SINK snk1 FROM src
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk1'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK snk2 FROM src_materialized
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk2'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK snk3 FROM v1
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk3'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK snk4 FROM v2
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk4'
  WITH (retention_ms=1000000)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK snk5 FROM v3
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk5'
  WITH (retention_bytes=1000000000000)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> SHOW SINKS
name
----
snk1
snk2
snk3
snk4
snk5

> SHOW FULL SINKS
name   type  volatility
-----------------------
snk1   user  unknown
snk2   user  unknown
snk3   user  unknown
snk4   user  unknown
snk5   user  unknown

$ kafka-verify format=avro sink=materialize.public.snk1 sort-messages=true
{"before": null, "after": {"row":{"a": "goofus", "b": "gallant", "mz_line_no": 2}}}
{"before": null, "after": {"row":{"a": "jack", "b": "jill", "mz_line_no": 1}}}

$ kafka-verify format=avro sink=materialize.public.snk2 sort-messages=true
{"before": null, "after": {"row":{"a": "goofus", "b": "gallant", "mz_line_no": 2}}}
{"before": null, "after": {"row":{"a": "jack", "b": "jill", "mz_line_no": 1}}}

$ kafka-verify format=avro sink=materialize.public.snk3 sort-messages=true
{"before": null, "after": {"row":{"c": "goofusgallant"}}}
{"before": null, "after": {"row":{"c": "jackjill"}}}

$ kafka-verify format=avro sink=materialize.public.snk4 sort-messages=true
{"before": null, "after": {"row":{"c": "goofusgallant"}}}
{"before": null, "after": {"row":{"c": "jackjill"}}}

$ kafka-verify format=avro sink=materialize.public.snk5 sort-messages=true
{"before": null, "after": {"row":{"c": "goofusgallant"}}}
{"before": null, "after": {"row":{"c": "jackjill"}}}

# Test the case where we have non +/- 1 multiplicities

> CREATE MATERIALIZED VIEW v4 AS
  SELECT true AS c FROM src

> CREATE SINK snk6 FROM v4
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk6'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

$ kafka-verify format=avro sink=materialize.public.snk6
{"before": null, "after": {"row":{"c": true}}}
{"before": null, "after": {"row":{"c": true}}}

# Test AS OF and WITH/WITHOUT SNAPSHOT.
> CREATE MATERIALIZED SOURCE dynamic_src
  FROM FILE '${testdrive.temp-dir}/test.csv' WITH (tail = true)
  FORMAT CSV WITH HEADER

# Ensure the data is read into the source before creating sinks from it
# to correctly test WITH/WITHOUT SNAPSHOT.
> SELECT * FROM dynamic_src
jack jill 1
goofus gallant 2

> CREATE SINK snk7 FROM dynamic_src
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk7'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITHOUT SNAPSHOT

> CREATE SINK snk8 FROM dynamic_src
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'snk8'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH SNAPSHOT

$ file-append path=test.csv
extra,row

$ kafka-verify format=avro sink=materialize.public.snk7
{"before": null, "after": {"row":{"a": "extra", "b": "row", "mz_line_no": 3}}}

$ kafka-verify format=avro sink=materialize.public.snk8 sort-messages=true
{"before": null, "after": {"row":{"a": "extra", "b": "row", "mz_line_no": 3}}}
{"before": null, "after": {"row":{"a": "goofus", "b": "gallant", "mz_line_no": 2}}}
{"before": null, "after": {"row":{"a": "jack", "b": "jill", "mz_line_no": 1}}}

# Test that we are correctly handling WITH/WITHOUT SNAPSHOT on views with
# empty upper frontier
> CREATE MATERIALIZED VIEW foo AS VALUES (1), (2), (3);

> CREATE SINK sink9 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink9'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITHOUT SNAPSHOT

$ kafka-verify format=avro sink=materialize.public.sink9

> CREATE SINK sink10 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink10'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH SNAPSHOT

$ kafka-verify format=avro sink=materialize.public.sink10 sort-messages=true
{"before": null, "after": {"row":{"column1": 1}}}
{"before": null, "after": {"row":{"column1": 2}}}
{"before": null, "after": {"row":{"column1": 3}}}

> SHOW FULL SINKS
name        type  volatility
----------------------------
snk1        user  unknown
snk2        user  unknown
snk3        user  unknown
snk4        user  unknown
snk5        user  unknown
snk6        user  unknown
snk7        user  unknown
snk8        user  unknown
sink9       user  nonvolatile
sink10      user  nonvolatile

# test explicit partition count
> CREATE SINK sink11 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink11'
  WITH (partition_count=1)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

# test explicit replication factor
> CREATE SINK sink12 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink12'
  WITH (replication_factor=1)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

# test explicit partition count and replication factor
> CREATE SINK sink13 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink13'
  WITH (partition_count=1, replication_factor=1)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

# test broker defaulted partition count and replication factor
> CREATE SINK sink14 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink14'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

# test explicit request for broker defaulted partition count and replication factor
> CREATE SINK sink15 FROM foo
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'sink15'
  WITH (partition_count=-1, replication_factor=-1)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
