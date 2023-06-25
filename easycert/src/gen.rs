use clap::Parser;
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

use anyhow::Result;
use std::{
    fs::{self, read_to_string},
    net::IpAddr,
    path::PathBuf,
};

#[derive(Parser)]
pub(crate) struct Gen {
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

impl Gen {
    pub(crate) fn run(self) -> Result<()> {
        let san = parse_san(self.subject_alt_names)?;
        let ca = load_ca(self.ca_key, self.ca_cert)?;
        let cert = gen_cert(self.common_name, self.days, san)?;

        let cert_signed = cert.serialize_pem_with_signer(&ca)?;

        let output = self.output.join(&self.name);
        std::fs::create_dir_all(&output)?;

        let cert_path = output.join(self.name.clone() + ".crt");
        fs::write(cert_path, cert_signed)?;

        let key_path = output.join(self.name + ".key");
        fs::write(key_path, cert.serialize_private_key_pem().as_bytes())?;

        Ok(())
    }
}

fn gen_cert(common_name: String, days: u32, san: Vec<SanType>) -> Result<Certificate> {
    let mut cert_params: CertificateParams = Default::default();
    cert_params.not_before = time::OffsetDateTime::now_utc();
    cert_params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(days as i64);
    cert_params.distinguished_name = DistinguishedName::new();
    cert_params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    cert_params.subject_alt_names = san;

    Ok(Certificate::from_params(cert_params)?)
}

/// Turns a vec of san str(e.g. "DNS:example.com", "IP:1.1.1.1") into
/// a vec of rcgen::SanType.
fn parse_san(subject_alt_names_str: Vec<String>) -> std::io::Result<Vec<SanType>> {
    if subject_alt_names_str.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "at least provide one SAN",
        ));
    }
    let mut subject_alt_names = Vec::new();
    for san_str in subject_alt_names_str {
        let san: Vec<_> = san_str.split(':').take(2).collect();
        if san.len() != 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("subject alt name should be in pair: {}", san_str),
            ));
        }
        let san_value = san[1];
        match san[0].to_uppercase().as_str() {
            "DNS" => {
                if san_value.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Dns name is empty",
                    ));
                };
                subject_alt_names.push(SanType::DnsName(san_value.into()))
            }
            "IP" => {
                let san_value = san_value.parse::<IpAddr>().map_err(|x| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("Failed to parse ip address: {}", x),
                    )
                })?;
                subject_alt_names.push(SanType::IpAddress(san_value))
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("subject alt name type currently not support: {}", san[0]),
                ));
            }
        }
    }
    Ok(subject_alt_names)
}

fn load_ca(ca_key: PathBuf, ca_cert: PathBuf) -> Result<Certificate> {
    let ca_keypair = KeyPair::from_pem(&read_to_string(ca_key)?)?;
    let ca = read_to_string(ca_cert)?;
    let ca = CertificateParams::from_ca_cert_pem(&ca, ca_keypair)?;
    let ca = Certificate::from_params(ca)?;
    Ok(ca)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn vec_str2vec_string(a: Vec<&str>) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn test_parse_san_ok() {
        let sans = vec!["IP:1.1.1.1"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_ok());

        let sans = vec!["iP:1.1.1.1"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_ok(), "Case insensive");

        let sans = vec!["dns:example.com"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_ok());

        let sans = vec!["ip:1.1.1.1", "dns:example.com"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_ok(), "Case insensive");
    }

    #[test]
    fn test_parse_san_invalid() {
        // empty
        let sans: Vec<String> = Vec::new();
        let res = parse_san(sans);
        assert!(res.is_err(), "empty should fail");
        assert_eq!(res.unwrap_err().to_string(), "at least provide one SAN");

        // not in pair
        let sans = vec!["IP:"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_err(), "not in pair should fail");
        assert_eq!(
            res.unwrap_err().to_string(),
            "Failed to parse ip address: invalid IP address syntax"
        );

        // not in pair
        let sans = vec!["IP"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_err(), "not in pair should fail");
        assert_eq!(
            res.unwrap_err().to_string(),
            format!("subject alt name should be in pair: {}", "IP")
        );

        // not in pair
        let sans = vec!["IP:1.1.1.1", "IP:"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_err(), "not in pair should fail");
        assert_eq!(
            res.unwrap_err().to_string(),
            "Failed to parse ip address: invalid IP address syntax"
        );

        // cannot parse ip
        let sans = vec!["IP:1.1.1"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_err(), "not in pair should fail");

        // empty dns name
        let sans = vec!["DNS:"];
        let sans = vec_str2vec_string(sans);
        let res = parse_san(sans);
        assert!(res.is_err(), "Dns name is empty");
    }
}
