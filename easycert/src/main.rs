use std::{
    fs::{self, read_to_string},
    net::IpAddr,
    path::PathBuf,
};

use clap::Parser;
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

#[derive(Debug, Parser)]
struct Opts {
    /// CA certificate path
    #[arg(long = "cacert")]
    ca_cert: PathBuf,
    /// CA private key path
    #[arg(long = "cakey")]
    ca_key: PathBuf,
    /// common name for target certificate
    #[arg(long)]
    common_name: String,
    /// list of subject alt names, e.g. --san DNS:example.com --san IP:1.1.1.1
    #[arg(long = "san")]
    subject_alt_names: Vec<String>,
    #[arg(long = "days", default_value_t = 365)]
    days: u32,
    /// output dir
    #[arg(long, short)]
    output: PathBuf,
    /// file name for target cerificate
    name: String,
}
fn main() {
    let opts = Opts::parse();

    if let Err(e) = run(opts) {
        eprintln!("{}", e)
    }
}

fn run(opts: Opts) -> Result<(), Box<dyn std::error::Error>> {
    let san = parse_san(opts.subject_alt_names)?;
    let ca = parse_ca(opts.ca_key, opts.ca_cert)?;

    let mut params: CertificateParams = Default::default();
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(opts.days as i64);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, opts.common_name);
    params.subject_alt_names = san;

    let cert = Certificate::from_params(params)?;

    let cert_signed = cert.serialize_pem_with_signer(&ca)?;

    let name = opts.name;
    let output = opts.output.join(&name);
    std::fs::create_dir_all(&output)?;

    let cert_path = output.join(name.clone() + ".crt");
    fs::write(cert_path, cert_signed)?;

    let key_path = output.join(name + ".key");
    fs::write(key_path, cert.serialize_private_key_pem().as_bytes())?;

    Ok(())
}

fn parse_san(
    subject_alt_names_str: Vec<String>,
) -> Result<Vec<SanType>, Box<dyn std::error::Error>> {
    if subject_alt_names_str.is_empty() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "at least provide one SAN",
        )));
    }
    let mut subject_alt_names = Vec::new();
    for san_str in subject_alt_names_str {
        let san: Vec<_> = san_str.split(':').take(2).collect();
        if san.len() != 2 {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("subject alt name should be in pair: {}", san_str),
            )));
        }
        let san_value = san[1];
        match san[0].to_uppercase().as_str() {
            "DNS" => subject_alt_names.push(SanType::DnsName(san_value.into())),
            "IP" => {
                let san_value = san_value.parse::<IpAddr>()?;
                subject_alt_names.push(SanType::IpAddress(san_value))
            }
            _ => {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("subject alt name type currently not support: {}", san[0]),
                )));
            }
        }
    }
    Ok(subject_alt_names)
}

fn parse_ca(ca_key: PathBuf, ca_cert: PathBuf) -> Result<Certificate, Box<dyn std::error::Error>> {
    let ca_keypair = KeyPair::from_pem(&read_to_string(ca_key)?)?;
    let ca = read_to_string(ca_cert)?;
    let ca = CertificateParams::from_ca_cert_pem(&ca, ca_keypair)?;
    let ca = Certificate::from_params(ca)?;
    Ok(ca)
}
