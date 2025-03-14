---
title: "Install"
description: "Get started with Materialize"
menu: "main"
weight: 2
---

{{< cta target="_blank" full_width="true" href="https://materialize.com/s/chat" >}}
Want to connect with Materialize? Join our growing community on Slack! →
{{< /cta >}}

You can access Materialize through the `materialized` binary, which you can
install on macOS and Linux, or [build](#build-from-source) on most OSes (e.g. FreeBSD). These
instructions install the latest release of Materialize, **{{< version >}}**. The latest (unstable)
developer builds are available at https://dev.materialize.com/. For prior releases,
see the [Versions page](/versions).

**Have any questions?** [Ask us on Slack](https://materialize.com/s/chat)

## Docker

We provide the `materialize/materialized` image in Docker Hub. If you already have
[Docker][docker-start] installed, you can run materialized with your tools in the usual
way. For example:

```shell
docker run -p 6875:6875 materialize/materialized:{{< version >}} --workers 1
```

[docker-start]: https://www.docker.com/get-started

## macOS installation

### Homebrew

Assuming you've installed [Homebrew](https://brew.sh/):

```shell
brew install MaterializeInc/materialize/materialized
```

### curl

```shell
curl -L https://binaries.materialize.com/materialized-{{< version >}}-x86_64-apple-darwin.tar.gz \
    | sudo tar -xzC /usr/local --strip-components=1
```

## Linux installation

### apt (Ubuntu, Debian, or variants)

**Note!** These instructions changed between versions 0.8.0 and 0.8.1. If you ran them
previously, you may need to do so again to continue receiving updates.

```shell
# Add the signing key for the Materialize apt repository
sudo apt-key adv --keyserver keyserver.ubuntu.com --recv-keys 79DEC5E1B7AE7694
# Add and update the repository
sudo sh -c 'echo "deb http://apt.materialize.com/ generic main" > /etc/apt/sources.list.d/materialize.list'
sudo apt update
# Install materialized
sudo apt install materialized
```

### curl
```shell
curl -L https://binaries.materialize.com/materialized-{{< version >}}-x86_64-unknown-linux-gnu.tar.gz \
    | sudo tar -xzC /usr/local --strip-components=1
```

## Build from source

Materialize is written primarily in Rust, but incorporates several components
written in C. To build Materialize, you will need to acquire the following
tools:

  * A recent version of Git

  * A C compiler that supports C11

  * CMake v3.2+

  * Rust v{{< rust-version >}}+

Your system's package manager, like Homebrew on macOS or APT on Debian, likely
contains sufficiently recent versions of Git, a C compiler, and CMake. However,
we recommend installing Rust via [rustup]. rustup configures your system so that
running `cargo build` in the Materialize repository will automatically download
and use the correct version of Rust.

{{< warning >}}
Materialize requires a very recent version of Rust. The version of Rust
available in your package manager is likely too old.
{{< /warning >}}

Once you've installed the prerequisites, to build your own `materialized`
binary, you can clone the [`MaterializeInc/materialize` repo from
GitHub][mz-repo], and build it using `cargo build`. Be sure to check out the tag
for the correct release.

```shell
git clone https://github.com/MaterializeInc/materialize.git
cd materialize
git checkout {{< version >}}
cargo build --release
```

## Run the binary

You can start the `materialized` process by simply running the binary, e.g.

```nofmt
./materialized -w 1
```

`-w 1` specifies that the process will use 1 worker. You can also find more detail
about our [command line flags](/cli/#command-line-flags).

By default `materialized` uses:

Detail | Info
----------|------
**Database** | `materialize`
**Port** | `6875`

### CLI Connections

To connect to a running instance, you can use any [Materialize-compatible CLI](/connect/cli/),
like `psql` or `mzcli`. To install the `psql` client:

{{< tabs >}}
{{< tab "macOS">}}

```shell
brew install postgresql
```

{{< /tab >}}

{{< tab "Linux">}}

```shell
apt install postgresql-client
```

{{< /tab >}}

{{< /tabs >}}

Once you have `psql` installed, connect using:

```shell
psql -U materialize -h localhost -p 6875 materialize
```

<p>

{{< cta href="/get-started" >}}
Next, let's get started with Materialize! →
{{</ cta >}}

[Rustup]: https://rustup.rs
[mz-repo]: https://github.com/MaterializeInc/materialize
