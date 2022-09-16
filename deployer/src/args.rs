use clap::Parser;
use fqdn::FQDN;
use shuttle_common::Port;

/// Program to handle the deploys for a single project
/// Handling includes, building, testing, and running each service
#[derive(Debug, Parser)]
#[clap(author, version, about)]
pub struct Args {
    /// Address to connect to the provisioning service
    #[clap(long)]
    pub provisioner_address: String,

    /// Port provisioner is running on
    #[clap(long, default_value = "5000")]
    pub provisioner_port: Port,

    /// FQDN where the proxy can be reached at
    #[clap(long)]
    pub proxy_fqdn: FQDN,

    /// Secret that will be used to perform admin tasks on this deployer
    #[clap(long)]
    pub admin_secret: String,
}