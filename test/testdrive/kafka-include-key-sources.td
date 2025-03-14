# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set conflictkeyschema={
    "type": "record",
    "name": "Key",
    "fields": [
        {"name": "id", "type": "long"}
    ]
  }

$ set schema={
    "name": "row",
    "type": "record",
    "fields": [
      {"name": "id", "type": "long"},
      {"name": "b", "type": "long"}
    ]
  }

$ kafka-create-topic topic=avro-data
$ kafka-ingest format=avro key-format=avro topic=avro-data key-schema=${conflictkeyschema} schema=${schema} timestamp=1
{"id": 1} {"id": 2, "b": 3}

! CREATE SOURCE missing_key_format
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY
INCLUDE KEY requires specifying KEY FORMAT .. VALUE FORMAT, got bare FORMAT

! CREATE SOURCE missing_key_format
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY AS key_col
INCLUDE KEY requires specifying KEY FORMAT .. VALUE FORMAT, got bare FORMAT

> CREATE MATERIALIZED SOURCE avro_data_conflict
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${conflictkeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY

! SELECT id, b FROM avro_data_conflict
column name "id" is ambiguous

> SELECT * FROM avro_data_conflict
id id b
-------
1 2 3

> CREATE MATERIALIZED SOURCE avro_data_explicit (key_id, id, b)
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${conflictkeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY

> SELECT key_id, id, b FROM avro_data_explicit
key_id id b
------------
1 2 3

> CREATE MATERIALIZED SOURCE avro_data_as
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${conflictkeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY AS renamed_id

> SELECT * FROM avro_data_as
renamed_id id b
------------
1 2 3


> CREATE MATERIALIZED SOURCE avro_avro_data (key_id, id, b)
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${conflictkeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY


> CREATE MATERIALIZED SOURCE avro_data_upsert
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${conflictkeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY AS renamed
  ENVELOPE UPSERT

> SELECT * FROM avro_data_upsert
renamed id b
------------
1 2 3

$ set multikeyschema={
    "type": "record",
    "name": "Key",
    "fields": [
      {"name": "id", "type": "long"},
      {"name": "geo", "type": "string"}
    ]
  }

$ set noconflictschema={
    "name": "row",
    "type": "record",
    "fields": [
      {"name": "a", "type": "long"}
    ]
  }

$ kafka-create-topic topic=avro-data-record
$ kafka-ingest format=avro key-format=avro topic=avro-data-record key-schema=${multikeyschema} schema=${noconflictschema} timestamp=1
{"id": 1, "geo": "nyc"} {"a": 99}

> CREATE MATERIALIZED SOURCE avro_key_record_flattened
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-record-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${multikeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${noconflictschema}'
  INCLUDE KEY
  ENVELOPE NONE

> SELECT * FROM avro_key_record_flattened
id geo a
--------
1 nyc 99

> CREATE MATERIALIZED SOURCE avro_key_record_renamed
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-data-record-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${multikeyschema}'
  VALUE FORMAT AVRO USING SCHEMA '${noconflictschema}'
  INCLUDE KEY AS named
  ENVELOPE NONE

> SELECT (named).id as named_id, (named).geo as named_geo, a FROM avro_key_record_renamed
named_id named_geo a
--------------------
1 nyc 99

! CREATE MATERIALIZED SOURCE avro_debezium
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-avro-dbz-${testdrive.seed}'
  KEY FORMAT AVRO USING SCHEMA '${noconflictschema}'
  VALUE FORMAT AVRO USING SCHEMA '${schema}'
  INCLUDE KEY AS named
  ENVELOPE debezium
Cannot use INCLUDE KEY with ENVELOPE DEBEZIUM: Debezium values include all keys.

# formats: TEXT and REGEX
$ kafka-create-topic topic=textsrc

$ kafka-ingest topic=textsrc format=bytes key-format=bytes key-terminator=:
one,1:horse,apple
two,2:bee,honey

> CREATE MATERIALIZED SOURCE textsrc
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-textsrc-${testdrive.seed}'
  KEY FORMAT TEXT
  VALUE FORMAT TEXT
  INCLUDE KEY

> SELECT * FROM textsrc
key   text        mz_offset
---------------------------
one,1 horse,apple 1
two,2 bee,honey   2


> CREATE MATERIALIZED SOURCE regexvalue
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-textsrc-${testdrive.seed}'
  KEY FORMAT TEXT
  VALUE FORMAT REGEX '(?P<animal>[^,]+),(?P<food>\w+)'
  INCLUDE KEY

> SELECT * FROM regexvalue
key   animal food  mz_offset
----------------------------
one,1 horse  apple 1
two,2 bee    honey 2

> CREATE MATERIALIZED SOURCE regexboth
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-textsrc-${testdrive.seed}'
  KEY FORMAT REGEX '(?P<id_name>[^,]+),(?P<id>\w+)'
  VALUE FORMAT REGEX '(?P<animal>[^,]+),(?P<food>\w+)'
  INCLUDE KEY

> SELECT * FROM regexboth
id_name id animal food  mz_offset
---------------------------------
one     1  horse  apple 1
two     2  bee    honey 2


> CREATE MATERIALIZED SOURCE regexbothnest
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-textsrc-${testdrive.seed}'
  KEY FORMAT REGEX '(?P<id_name>[^,]+),(?P<id>\w+)'
  VALUE FORMAT REGEX '(?P<animal>[^,]+),(?P<food>\w+)'
  INCLUDE KEY AS nest

> SELECT (nest).id_name, (nest).id, animal FROM regexbothnest
id_name id animal
-----------------
one     1  horse
two     2  bee

$ kafka-create-topic topic=proto

$ kafka-ingest topic=proto key-format=protobuf key-message=simpleid format=protobuf message=measurement timestamp=1
{"id": "a"} {"resource": "CPU", "measured_value": 10}

> CREATE MATERIALIZED SOURCE input_proto
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-proto-${testdrive.seed}'
  KEY FORMAT PROTOBUF MESSAGE '.SimpleId' USING SCHEMA  '${testdrive.protobuf-descriptors}'
  VALUE FORMAT PROTOBUF MESSAGE '.Measurement' USING SCHEMA '${testdrive.protobuf-descriptors}'
  INCLUDE KEY

> SELECT * FROM input_proto
id resource measured_value mz_offset
------------------------------------
a  CPU      10             1


$ kafka-create-topic topic=proto-structured

$ kafka-ingest topic=proto-structured key-format=protobuf key-message=measurement format=protobuf message=simpleid timestamp=1
{"resource": "CPU", "measured_value": 10} {"id": "a"}

> CREATE MATERIALIZED SOURCE input_proto_structured
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-proto-structured-${testdrive.seed}'
  KEY FORMAT PROTOBUF MESSAGE '.Measurement' USING SCHEMA '${testdrive.protobuf-descriptors}'
  VALUE FORMAT PROTOBUF MESSAGE '.SimpleId' USING SCHEMA  '${testdrive.protobuf-descriptors}'
  INCLUDE KEY AS measurement

> SELECT (measurement).resource as r, (measurement).measured_value as v, id FROM input_proto_structured
r    v   id
-----------
CPU  10  a
