// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Command-line interface for Materialize Cloud.

use std::fs;
use std::process;

use serde::{Deserialize, Serialize};
use structopt::StructOpt;

use mzcloud::apis::configuration::Configuration;
use mzcloud::apis::deployments_api::{
    deployments_certs_retrieve, deployments_create, deployments_destroy, deployments_list,
    deployments_logs_retrieve, deployments_partial_update, deployments_retrieve,
};
use mzcloud::apis::mz_versions_api::mz_versions_list;
use mzcloud::models::deployment_request::DeploymentRequest;
use mzcloud::models::patched_deployment_request::PatchedDeploymentRequest;
use mzcloud::models::size_enum::SizeEnum;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

/// Command-line interface for Materialize Cloud.
#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(flatten)]
    oauth: OAuthArgs,

    /// Materialize Cloud domain.
    #[structopt(
        short,
        long,
        env = "MZCLOUD_DOMAIN",
        default_value = "cloud.materialize.com"
    )]
    domain: String,

    /// Whether to use HTTP instead of HTTPS when accessing the core API.
    ///
    /// Defaults to false unless `domain` is set to `localhost`.
    #[structopt(long, env = "MZCLOUD_INSECURE", hidden = true)]
    insecure: Option<bool>,

    /// The domain of the admin API.
    ///
    /// Defaults to `admin.{domain}` unless `domain` is set to `localhost`, in
    /// which case it assumes the standard local development environment setup
    /// for Materialize Cloud and defaults to
    /// `admin.staging.cloud.materialize.com`.
    #[structopt(long, env = "MZCLOUD_ADMIN_DOMAIN", hidden = true)]
    admin_domain: Option<String>,

    /// Which resources to operate on.
    #[structopt(subcommand)]
    category: Category,
}

impl Args {
    /// Reports whether the requested API domain is localhost.
    fn is_localhost(&self) -> bool {
        self.domain.starts_with("localhost:") || self.domain == "localhost"
    }

    /// Returns the base URL at which the core API is hosted.
    fn url(&self) -> String {
        let insecure = self.insecure.unwrap_or_else(|| self.is_localhost());
        match insecure {
            true => format!("http://{}", self.domain),
            false => format!("https://{}", self.domain),
        }
    }

    /// Returns the base URL at which the admin API is hosted.
    fn admin_url(&self) -> String {
        match &self.admin_domain {
            Some(admin_domain) => format!("https://{}", admin_domain),
            None if self.is_localhost() => "https://admin.staging.cloud.materialize.com".into(),
            None => format!("https://admin.{}", self.domain),
        }
    }
}

#[derive(Debug, StructOpt, Serialize)]
#[serde(rename_all = "camelCase")]
struct OAuthArgs {
    /// OAuth Client ID for authentication.
    #[structopt(long, env = "MZCLOUD_CLIENT_ID", hide_env_values = true)]
    client_id: String,

    /// OAuth Secret Key for authentication.
    #[structopt(long, env = "MZCLOUD_SECRET_KEY", hide_env_values = true)]
    secret: String,
}

#[derive(Debug, StructOpt)]
enum Category {
    /// Manage deployments.
    Deployments(DeploymentsCommand),
    /// List Materialize versions.
    MzVersions(MzVersionsCommand),
}

#[derive(Debug, StructOpt)]
enum DeploymentsCommand {
    /// Create a new Materialize deployment.
    Create {
        /// Version of materialized to deploy. Defaults to latest available version.
        #[structopt(short = "v", long)]
        mz_version: Option<String>,
        /// Size of the deployment.
        #[structopt(short, long, parse(try_from_str = parse_size))]
        size: Option<SizeEnum>,
        /// The number of megabytes of storage to allocate.
        #[structopt(long)]
        storage_mb: Option<i32>,
        /// Extra arguments to provide to materialized.
        #[structopt(long)]
        materialized_extra_args: Option<Vec<String>>,
    },

    /// Describe a Materialize deployment.
    Get {
        /// ID of the deployment.
        id: String,
    },

    /// Change the version or size of a Materialize deployment.
    Update {
        /// ID of the deployment.
        id: String,
        /// Version of materialized to upgrade to. Defaults to the current
        /// version.
        #[structopt(short = "v", long)]
        mz_version: Option<String>,
        /// Size of the deployment. Defaults to current size.
        #[structopt(short, long, parse(try_from_str = parse_size))]
        size: Option<SizeEnum>,
        /// Extra arguments to provide to materialized. Defaults to the
        /// currently set extra arguments.
        #[structopt(long)]
        materialized_extra_args: Option<Vec<String>>,
    },

    /// Destroy a Materialize deployment.
    Destroy {
        /// ID of the deployment.
        id: String,
    },

    /// List existing Materialize deployments.
    List,

    /// Download the certificates bundle for a Materialize deployment.
    Certs {
        /// ID of the deployment.
        id: String,
        /// Path to save the certs bundle to.
        #[structopt(short, long, default_value = "mzcloud-certs.zip")]
        output_file: String,
    },

    /// Download the logs from a Materialize deployment.
    Logs {
        /// ID of the deployment.
        id: String,
    },
}

#[derive(Debug, StructOpt)]
enum MzVersionsCommand {
    /// List available Materialize versions.
    List,
}

fn parse_size(s: &str) -> Result<SizeEnum, String> {
    match s {
        "XS" => Ok(SizeEnum::XS),
        "S" => Ok(SizeEnum::S),
        "M" => Ok(SizeEnum::M),
        "L" => Ok(SizeEnum::L),
        "XL" => Ok(SizeEnum::XL),
        _ => Err("Invalid size.".to_owned()),
    }
}

async fn handle_mz_version_operations(
    config: &Configuration,
    operation: MzVersionsCommand,
) -> anyhow::Result<()> {
    Ok(match operation {
        MzVersionsCommand::List => {
            let versions = mz_versions_list(&config).await?;
            println!("{}", serde_json::to_string_pretty(&versions)?);
        }
    })
}

async fn handle_deployment_operations(
    config: &Configuration,
    operation: DeploymentsCommand,
) -> anyhow::Result<()> {
    Ok(match operation {
        DeploymentsCommand::Create {
            size,
            mz_version,
            storage_mb,
            materialized_extra_args,
        } => {
            let deployment = deployments_create(
                &config,
                Some(DeploymentRequest {
                    size: size.map(Box::new),
                    mz_version,
                    storage_mb,
                    materialized_extra_args,
                }),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&deployment)?);
        }
        DeploymentsCommand::Get { id } => {
            let deployment = deployments_retrieve(&config, &id).await?;
            println!("{}", serde_json::to_string_pretty(&deployment)?);
        }
        DeploymentsCommand::Update {
            id,
            size,
            mz_version,
            materialized_extra_args,
        } => {
            let deployment = deployments_partial_update(
                &config,
                &id,
                Some(PatchedDeploymentRequest {
                    size: size.map(Box::new),
                    mz_version,
                    storage_mb: None,
                    materialized_extra_args,
                }),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&deployment)?);
        }
        DeploymentsCommand::Destroy { id } => {
            deployments_destroy(&config, &id).await?;
        }
        DeploymentsCommand::List => {
            let deployments = deployments_list(&config).await?;
            println!("{}", serde_json::to_string_pretty(&deployments)?);
        }
        DeploymentsCommand::Certs { id, output_file } => {
            let bytes = deployments_certs_retrieve(&config, &id).await?;
            fs::write(&output_file, &bytes)?;
            println!("Certificate bundle saved to {}", &output_file);
        }
        DeploymentsCommand::Logs { id } => {
            let logs = deployments_logs_retrieve(&config, &id).await?;
            print!("{}", logs);
        }
    })
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct OauthResponse {
    access_token: String,
}

async fn get_oauth_token(args: &Args) -> Result<String, reqwest::Error> {
    Ok(reqwest::Client::new()
        .post(format!(
            "{}/identity/resources/auth/v1/api-token",
            args.admin_url()
        ))
        .json(&args.oauth)
        .send()
        .await?
        .error_for_status()?
        .json::<OauthResponse>()
        .await?
        .access_token)
}

async fn run() -> anyhow::Result<()> {
    let args = Args::from_args();

    let access_token = get_oauth_token(&args).await?;
    let config = Configuration {
        base_path: args.url(),
        user_agent: Some(format!("mzcloud-cli/{}/rust", VERSION)),
        // Yes, this came from OAuth, but Frontegg wants it as a bearer token.
        bearer_access_token: Some(access_token),
        ..Default::default()
    };

    Ok(match args.category {
        Category::Deployments(operation) => {
            handle_deployment_operations(&config, operation).await?
        }
        Category::MzVersions(operation) => handle_mz_version_operations(&config, operation).await?,
    })
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {:#?}", e);
        process::exit(1);
    }
}
