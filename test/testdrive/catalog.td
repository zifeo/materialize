# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-sql-timeout duration=1s

# What schemas do we have by default?
> SHOW SCHEMAS
public
> SHOW EXTENDED SCHEMAS
name
-------
public
mz_catalog
mz_internal
pg_catalog

# What objects do we have by default?
> SHOW OBJECTS
name
----

> SHOW FULL OBJECTS
name   type
-----------

# Creating a schema should be reflected in the output of SHOW SCHEMAS.
> CREATE SCHEMA s
> SHOW SCHEMAS
name
----
public
s

# Creating a schema with a name that already exists should fail.
! CREATE SCHEMA s
schema 's' already exists

# Dropping a schema with a view should only succeed with CASCADE.
> CREATE VIEW s.v AS SELECT 1
! DROP SCHEMA s
schema 'materialize.s' cannot be dropped without CASCADE while it contains objects
> DROP SCHEMA s CASCADE

# Dropping a schema with no objects should succeed without CASCADE.
> CREATE SCHEMA s
> CREATE VIEW s.v AS SELECT 1
> DROP VIEW s.v
> DROP SCHEMA s

# What databases do we have by default?
> SHOW DATABASES
name
----
materialize

> SELECT id, name FROM mz_databases
id          name
-----------------------
1           materialize

# Creating a database should be reflected in the output of SHOW DATABASES.
> CREATE DATABASE d
> SHOW DATABASES
name
----
d
materialize

> SELECT id, name FROM mz_databases
id          name
-----------------------
1           materialize
2           d

# The same catalog information should be accessible with any amount of
# database or schema qualification.
> SELECT count(*) FROM materialize.mz_catalog.mz_databases
2
> SELECT count(*) FROM d.mz_catalog.mz_databases
2

# SHOW DATABASES should filter its output according to the provided LIKE or
# WHERE clause.
> SHOW DATABASES LIKE 'foo'
> SHOW DATABASES LIKE 'd'
d
> SHOW DATABASES LIKE 'mat%'
materialize
> SHOW DATABASES WHERE (SELECT name = name)
materialize
d
> SHOW DATABASES WHERE (name = (SELECT min(name) FROM mz_databases))
d
! SHOW DATABASES WHERE 7
WHERE clause must have type boolean, not type integer

# Creating a database with a name that already exists should fail.
! CREATE DATABASE d
database 'd' already exists

# The new database should have a default public schema.
> SHOW SCHEMAS FROM d
name
----
public

# New schemas in the database should appear in the output of SHOW SCHEMAS FROM.
> CREATE SCHEMA d.s
> SHOW SCHEMAS FROM d
name
------
public
s

# SHOW SCHEMAS should filter its output based on the provided LIKE or WHERE
# clause.
> SHOW SCHEMAS LIKE 'pub%'
public
> SHOW SCHEMAS LIKE 'private'
> SHOW SCHEMAS WHERE name = 'public'
public

# New views in the database should work.
> CREATE MATERIALIZED VIEW d.public.v1 AS SELECT 1
> SHOW VIEWS FROM d.public
name
----
v1

# Setting the session database should update name resolution appropriately.
> SET DATABASE = d
> SHOW DATABASE
d
> SHOW SCHEMAS
public
s
> CREATE MATERIALIZED VIEW v2 AS SELECT 2
> SHOW VIEWS
v1
v2
> SELECT * FROM v1 UNION ALL SELECT * FROM v2
1
2

> SHOW OBJECTS
name
----
v1
v1_primary_idx
v2
v2_primary_idx

# Test minimizing name qualification

> CREATE TYPE int_list AS list (element_type=int4)

> SELECT pg_typeof('{1}'::d.public.int_list)
int_list

> CREATE SCHEMA other
> CREATE TYPE other.int_list AS list (element_type=int4)
> SELECT pg_typeof('{1}'::d.other.int_list)
other.int_list

> CREATE DATABASE foo
> CREATE SCHEMA foo.other
> CREATE TYPE foo.other.int_list AS LIST (element_type=int4)
> SELECT pg_typeof('{1}'::foo.other.int_list)
foo.other.int_list

> CREATE TYPE bool AS LIST (element_type=int4)
! SELECT '{1}'::bool
invalid input syntax for type boolean: "{1}"

> SELECT pg_typeof('{1}'::public.bool);
public.bool

> SELECT pg_typeof('{1}'::d.public.bool);
public.bool

> DROP DATABASE foo

! DROP OBJECT v1
Expected DATABASE, INDEX, ROLE, SCHEMA, SINK, SOURCE, TABLE, TYPE, USER, VIEW after DROP, found identifier

> SHOW FULL OBJECTS
name            type
--------------------------
v1              user
v1_primary_idx  user
v2              user
v2_primary_idx  user

# DROP DATABASE should not support RESTRICT or CASCADE.
! DROP DATABASE d RESTRICT
Expected end of statement, found RESTRICT
! DROP DATABASE d RESTRICT
Expected end of statement, found RESTRICT

# DROP DATABASE should succeed even when there are objects in the database.
> DROP DATABASE d
# SHOW DATABASES should work, even if the current database has been dropped.
> SHOW DATABASES
name
----
materialize

> SELECT id, name FROM mz_databases
id          name
-----------------------
1           materialize

# The session database should remain set to the dropped database, but future
# queries that depend on the session database should fail with sensible error
# messages.
> SHOW DATABASE
d
! SELECT * FROM v
unknown catalog item 'v'

# But queries that do not depend on the session database should work fine.
> CREATE VIEW materialize.public.v AS SELECT 1
> CREATE DATABASE d

# Dropping the public schema is okay, but dropping the catalog schemas is not.
> DROP SCHEMA public
! DROP SCHEMA mz_catalog
cannot drop schema mz_catalog because it is required by the database system
! DROP SCHEMA pg_catalog
cannot drop schema pg_catalog because it is required by the database system

# Schema names that start with "mz_" or "pg_" are reserved for future use by the
# system.
! CREATE SCHEMA mz_foo
unacceptable schema name 'mz_foo'
! CREATE SCHEMA pg_bar
unacceptable schema name 'pg_bar'

# The search path is currently hardcoded.
> SHOW search_path
"mz_catalog, pg_catalog, public, mz_temp"
! SET search_path = foo
parameter "search_path" cannot be changed

# Creating views in non-existent databases should fail.
! CREATE VIEW noexist.ignored AS SELECT 1
unknown schema 'noexist'
! CREATE VIEW materialize.noexist.ignored AS SELECT 1
unknown schema 'noexist'
! CREATE VIEW noexist.ignored.ignored AS SELECT 1
unknown database 'noexist'

# As should showing views.
! SHOW VIEWS FROM noexist
unknown schema 'noexist'
! SHOW VIEWS FROM noexist_db.noexist_schema
unknown database 'noexist_db'

# Dropping database with cross-schema dependencies is ok.
> CREATE DATABASE d1;
> CREATE SCHEMA d1.s1;
> CREATE VIEW d1.s1.t as select 1;
> CREATE VIEW d1.public.tt as select * from d1.s1.t;
> DROP DATABASE d1;

# Dropping database with cross-database dependencies is ok and drops the
# dependent views.
> CREATE DATABASE d1;
> CREATE VIEW d1.public.t as select 1;
> CREATE DATABASE d2;
> CREATE VIEW d2.public.t AS SELECT * FROM d1.public.t;
> DROP DATABASE d1;
> SHOW DATABASES
name
----
d
d2
materialize
> SELECT id, name FROM mz_databases
id          name
-----------------------
1           materialize
2           d
4           d2

> SHOW VIEWS FROM d2.public;
 name
------

# Check default sources, tables, and views in mz_catalog.

> SHOW SOURCES FROM mz_catalog
mz_arrangement_sharing_internal
mz_arrangement_batches_internal
mz_arrangement_records_internal
mz_dataflow_channels
mz_dataflow_operator_addresses
mz_dataflow_operator_reachability_internal
mz_dataflow_operators
mz_kafka_broker_rtt
mz_kafka_consumer_partitions
mz_materialization_dependencies
mz_materializations
mz_message_counts_received_internal
mz_message_counts_sent_internal
mz_peek_active
mz_peek_durations
mz_scheduling_elapsed_internal
mz_scheduling_histogram_internal
mz_scheduling_parks_internal
mz_source_info
mz_worker_materialization_frontiers

> SHOW FULL SOURCES FROM mz_catalog
name                                          type   materialized  volatility  connector_type
---------------------------------------------------------------------------------------------
mz_arrangement_sharing_internal               system true          volatile    local
mz_arrangement_batches_internal               system true          volatile    local
mz_arrangement_records_internal               system true          volatile    local
mz_dataflow_channels                          system true          volatile    local
mz_dataflow_operator_addresses                system true          volatile    local
mz_dataflow_operator_reachability_internal    system true          volatile    local
mz_dataflow_operators                         system true          volatile    local
mz_kafka_broker_rtt                           system true          volatile    local
mz_kafka_consumer_partitions                  system true          volatile    local
mz_materialization_dependencies               system true          volatile    local
mz_materializations                           system true          volatile    local
mz_message_counts_received_internal           system true          volatile    local
mz_message_counts_sent_internal               system true          volatile    local
mz_peek_active                                system true          volatile    local
mz_peek_durations                             system true          volatile    local
mz_scheduling_elapsed_internal                system true          volatile    local
mz_scheduling_histogram_internal              system true          volatile    local
mz_scheduling_parks_internal                  system true          volatile    local
mz_source_info                                system true          volatile    local
mz_worker_materialization_frontiers           system true          volatile    local

> SHOW TABLES FROM mz_catalog
mz_array_types
mz_avro_ocf_sinks
mz_base_types
mz_columns
mz_databases
mz_functions
mz_index_columns
mz_indexes
mz_kafka_sinks
mz_list_types
mz_map_types
mz_metric_histograms
mz_metrics
mz_metrics_meta
mz_pseudo_types
mz_roles
mz_schemas
mz_sinks
mz_sources
mz_tables
mz_types
mz_view_foreign_keys
mz_view_keys
mz_views

> SHOW FULL TABLES FROM mz_catalog
name                  type
----------------------------
mz_array_types        system
mz_avro_ocf_sinks     system
mz_base_types         system
mz_columns            system
mz_databases          system
mz_functions          system
mz_index_columns      system
mz_indexes            system
mz_kafka_sinks        system
mz_list_types         system
mz_map_types          system
mz_metric_histograms  system
mz_metrics            system
mz_metrics_meta       system
mz_pseudo_types       system
mz_roles              system
mz_schemas            system
mz_sinks              system
mz_sources            system
mz_tables             system
mz_types              system
mz_view_foreign_keys  system
mz_view_keys          system
mz_views              system

> CREATE SCHEMA tester

> SHOW TABLES FROM tester

> SHOW EXTENDED tables FROM tester
mz_array_types
mz_avro_ocf_sinks
mz_base_types
mz_columns
mz_databases
mz_functions
mz_index_columns
mz_indexes
mz_kafka_sinks
mz_list_types
mz_map_types
mz_metric_histograms
mz_metrics
mz_metrics_meta
mz_pseudo_types
mz_roles
mz_schemas
mz_sinks
mz_sources
mz_tables
mz_types
mz_view_foreign_keys
mz_view_keys
mz_views

> CREATE TABLE tester.test_table (a int)

> SHOW TABLES FROM tester
test_table

> SHOW EXTENDED tables FROM tester
mz_array_types
mz_avro_ocf_sinks
mz_base_types
mz_columns
mz_databases
mz_functions
mz_index_columns
mz_indexes
mz_kafka_sinks
mz_list_types
mz_map_types
mz_metric_histograms
mz_metrics
mz_metrics_meta
mz_pseudo_types
mz_roles
mz_schemas
mz_sinks
mz_sources
mz_tables
mz_types
mz_view_foreign_keys
mz_view_keys
mz_views
test_table

# `SHOW TABLES` and `mz_tables` should agree.
> SELECT COUNT(*) FROM mz_tables WHERE id LIKE 's%'
24

# There is one entry in mz_indexes for each field_number/expression of the index.
> SELECT COUNT(id) FROM mz_indexes WHERE id LIKE 's%'
44

> SHOW VIEWS FROM mz_catalog
mz_arrangement_sharing
mz_arrangement_sizes
mz_catalog_names
mz_dataflow_names
mz_dataflow_operator_dataflows
mz_dataflow_operator_reachability
mz_materialization_frontiers
mz_message_counts
mz_objects
mz_perf_arrangement_records
mz_perf_dependency_frontiers
mz_perf_peek_durations_aggregates
mz_perf_peek_durations_bucket
mz_perf_peek_durations_core
mz_records_per_dataflow
mz_records_per_dataflow_global
mz_records_per_dataflow_operator
mz_relations
mz_scheduling_elapsed
mz_scheduling_histogram
mz_scheduling_parks

> SHOW FULL VIEWS FROM mz_catalog
name                              type   materialized  volatility
-----------------------------------------------------------------
mz_arrangement_sharing            system false         volatile
mz_arrangement_sizes              system false         volatile
mz_dataflow_names                 system false         volatile
mz_dataflow_operator_dataflows    system false         volatile
mz_dataflow_operator_reachability system false         volatile
mz_materialization_frontiers      system false         volatile
mz_message_counts                 system false         volatile
mz_objects                        system false         volatile
mz_perf_arrangement_records       system false         volatile
mz_perf_dependency_frontiers      system false         volatile
mz_perf_peek_durations_aggregates system false         volatile
mz_perf_peek_durations_bucket     system false         volatile
mz_perf_peek_durations_core       system false         volatile
mz_records_per_dataflow           system false         volatile
mz_records_per_dataflow_global    system false         volatile
mz_records_per_dataflow_operator  system false         volatile
mz_relations                      system false         volatile
mz_catalog_names                  system false         volatile
mz_scheduling_elapsed             system false         volatile
mz_scheduling_histogram           system false         volatile
mz_scheduling_parks               system false         volatile

> SHOW MATERIALIZED SOURCES FROM mz_catalog LIKE '%peek%';
mz_peek_active
mz_peek_durations

> SHOW VIEWS FROM mz_catalog LIKE '%peek%';
mz_perf_peek_durations_aggregates
mz_perf_peek_durations_bucket
mz_perf_peek_durations_core
