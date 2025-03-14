---
title: "Get Started with Cloud"
description: "Connect to Cloud and create Materialize deployments."
menu:
  main:
    parent: "cloud"
    weight: 2
aliases:
  - materialize-cloud-quickstart
  - materialize-cloud-get-started
  - quickstart
  - get-started
---

{{< cloud-notice >}}

This guide walks you through getting started with Materialize Cloud, from setting up an account to creating your first materialized view on top of streaming data. We'll cover:

* Signing up for Materialize Cloud

* Creating and connecting to a Materialize Cloud deployment

* Connecting to a streaming data source

* Creating a materialized view

* Exploring common patterns like joins and time-windowing

## Sign up

1. Sign up for Materialize Cloud at [https://cloud.materialize.com](https://cloud.materialize.com/signup/).

1. Once your account has been created, [log in](https://cloud.materialize.com).

1. If you've been invited to an existing workspace, jump directly to [Deploy and Connect](#deploy-and-connect). Otherwise, a dialog will ask you to create a workspace.

    Enter a name and click **Next**.

## Deploy and connect

1. In the deployments page, click [**Create deployment**](../create-deployments) in the upper right corner. Enter a unique **Name** (or use the default) and choose **Extra small** (XS) as the deployment size.

    Then, click **Create**.

    **Note:** The size of new deployments is set to extra small (XS) by default, which is enough to run this walkthrough. For more information on deployment sizes, check [Account Limits](../account-limits).

1. Once the status message reads `HEALTHY`, the deployment is ready for connections!

    Before you can connect, though, you need to install some TLS certificates on your local machine.

{{% cloud-connection-details %}}

## Explore a streaming source

Materialize allows you to work with streaming data from multiple external sources using nothing but standard SQL. You write arbitrarily complex queries; Materialize takes care of maintaining the results automatically up to date with very low latency.

We'll start with some sample real-time data from a [PubNub stream](https://www.pubnub.com/developers/realtime-data-streams/) receiving the latest market orders for a given marketplace.

1. Let's create a [PubNub source](/sql/create-source/json-pubnub/#pubnub-source-details) that connects to the market orders channel with a subscribe key:

    ```sql
    CREATE SOURCE market_orders_raw
    FROM PUBNUB
    SUBSCRIBE KEY 'sub-c-4377ab04-f100-11e3-bffd-02ee2ddab7fe'
    CHANNEL 'pubnub-market-orders';
    ```

    The `CREATE SOURCE` statement is a definition of where to find and how to connect to our data source — Materialize won't start ingesting data just yet.

    To list the columns created:

    ```sql
    SHOW COLUMNS FROM market_orders_raw;
    ```


1. The PubNub source produces data as a single text column containing JSON. To extract the JSON fields for each market order, you can use the built-in `jsonb` [operators](/sql/types/jsonb/#jsonb-functions--operators):

    ```sql
    CREATE VIEW market_orders AS
    SELECT
        ((text::jsonb)->>'bid_price')::float AS bid_price,
        (text::jsonb)->>'order_quantity' AS order_quantity,
        (text::jsonb)->>'symbol' AS symbol,
        (text::jsonb)->>'trade_type' AS trade_type,
        to_timestamp(((text::jsonb)->'timestamp')::bigint) AS ts
    FROM market_orders_raw;
    ```

    One thing to note here is that we created a [non-materialized view](/overview/api-components/#non-materialized-views), which doesn't store the results of the query but simply provides an alias for the embedded `SELECT` statement.

1. We can now use this view as a base to create a [materialized view](/overview/api-components/#materialized-views) that computes the average bid price:

    ```sql
    CREATE MATERIALIZED VIEW avg_bid AS
    SELECT symbol,
           AVG(bid_price) AS avg
    FROM market_orders
    GROUP BY symbol;
    ```

    The `avg_bid` view is incrementally updated as new data streams in, so you get fresh and correct results with millisecond latency. Behind the scenes, Materialize is indexing the results of the embedded query in memory (i.e. _materializing_ the view).

1. Let's check the results:

    ```sql
    SELECT * FROM avg_bid;
    ```

    ```
      symbol    |        avg
    ------------+--------------------
    Apple       | 199.3392717416626
    Google      | 299.40371152970334
    Elerium     | 155.04668809209852
    Bespin Gas  | 202.0260593073953
    Linen Cloth | 254.34273792647863
    ```

     If you re-run the `SELECT` statement at different points in time, you can see the updated results based on the latest data.

1. To see the sequence of updates affecting the results over time, you can use `TAIL`:

    ```sql
    COPY (TAIL avg_bid) TO stdout;
    ```

    To cancel out of the stream, press **CTRL+C**.

### Joins

Materialize efficiently supports [all types of SQL joins](/sql/join/#examples) under all the conditions you would expect from a traditional relational database. Let's enrich the PubNub stream with some reference data as an example!

1. Create and populate a table with static reference data:

    ```sql
    CREATE TABLE symbols (
        symbol text,
        ticker text
    );

    INSERT INTO symbols
    SELECT *
    FROM (VALUES ('Apple','AAPL'),
                 ('Google','GOOG'),
                 ('Elerium','ELER'),
                 ('Bespin Gas','BGAS'),
                 ('Linen Cloth','LCLO')
    );

    ```

    **Note:** We are using a table for convenience to avoid adding complexity to the guide. It's [unlikely](/sql/create-table/#when-to-use-a-table) that you'll need to use tables in real-world scenarios.

1. Now we can enrich our aggregated data with the ticker for each stock using a regular `JOIN`:

    ```sql
    CREATE MATERIALIZED VIEW cnt_ticker AS
    SELECT s.ticker AS ticker,
           COUNT(*) AS cnt
    FROM market_orders m
    JOIN symbols s ON m.symbol = s.symbol
    GROUP BY s.ticker;
    ```

1. To see the results:

    ```sql
    SELECT * FROM cnt_ticker;
     ticker | cnt
    --------+-----
     AAPL   |  42
     BGAS   |  49
     ELER   |  68
     GOOG   |  51
     LCLO   |  70
    ```

    If you re-run the `SELECT` statement at different points in time, you can see the updated results based on the latest data.

### Temporal Filters

In Materialize, [temporal filters](/guides/temporal-filters/) allow you to define time-windows over otherwise unbounded streams of data. This can be useful for things like modeling business processes or limiting resource usage.

1. If, instead of computing and maintaining the _overall_ count of market orders, we want to get the _moving_ count from the past minute, we'd use a temporal filter defined by the `mz_logical_timestamp()` function:

    ```sql
    CREATE MATERIALIZED VIEW cnt_sliding AS
    SELECT symbol,
           COUNT(*) AS cnt
    FROM market_orders m
    WHERE EXTRACT(EPOCH FROM (ts + INTERVAL '1 minute'))::bigint * 1000 > mz_logical_timestamp()
    GROUP BY symbol;
    ```

    The `mz_logical_timestamp()` function is used to keep track of the logical time for your query ([similar to `now()` in other systems](/sql/functions/now_and_mz_logical_timestamp/)).

1. To see the results:

    ```sql
    SELECT * FROM cnt_sliding;

       symbol    | cnt
    -------------+-----
     Apple       |  31
     Google      |  40
     Elerium     |  46
     Bespin Gas  |  35
     Linen Cloth |  45
    ```

    As it advances, only the records that satisfy the time constraint are used in the materialized view and contribute to the in-memory footprint.

## Learn more

That's it! You just got up and running with Materialize Cloud, creating your first materialized view and trying out some common queries enabled by SQL on streams. We encourage you to continue exploring the PubNub source using the supported [SQL commands](/sql/). You can read through the following resources for more comprehensive overviews:

* [Connect to Materialize Cloud](../connect-to-cloud)
* [Materialize Cloud Account Limits](../account-limits)
* [Materialize Architecture](../../overview/architecture)
* [`CREATE SOURCE`](../../sql/create-source)
