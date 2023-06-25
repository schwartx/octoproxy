use std::{fs::read_to_string, io::Write, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use pem::Pem;
use time::format_description;
use x509_parser::prelude::X509Certificate;

#[derive(Parser)]
pub(crate) struct Show {
    /// file path for target cerificate
    cert_path: PathBuf,
    /// no color
    #[arg(short = 'c', default_value_t = false)]
    no_color: bool,
}

impl Show {
    pub(crate) fn run(self) -> Result<()> {
        let cert_content = read_to_string(self.cert_path).context("Failed to open cert")?;
        let pem_content = load_pem(&cert_content)?;
        let cert = load_cert_from_der(&pem_content)?;

        let is_ca = match cert.is_ca() {
            true => "yes",
            false => "no",
        }
        .to_owned();

        // Issuer
        let issuer = cert.issuer().to_string();

        // SAN
        let sans = cert
            .subject_alternative_name()?
            .map(|x| {
                let s = x
                    .value
                    .general_names
                    .iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<String>>()
                    .join(", ");
                s
            })
            .unwrap_or("".to_owned());

        // Subject
        let subject = cert.subject().to_string();

        let format = format_description::parse(
            "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour \
         sign:mandatory]:[offset_minute]:[offset_second]",
        )
        .unwrap();

        let not_before = cert.validity().not_before.to_datetime().format(&format)?;

        let not_after = cert.validity().not_after.to_datetime().format(&format)?;

        // is valid
        let is_valid = match cert.validity().is_valid() {
            true => "yes",
            false => "no",
        }
        .to_owned();

        let mut table = Table::new();
        table.push("Is CA", is_ca);
        table.push("Issuer", issuer);
        table.push("SAN", sans);
        table.push("Subject", subject);
        table.push("Not Before", not_before);
        table.push("Not After", not_after);
        table.push("Is Valid", is_valid);

        let mut stdout = std::io::stdout();
        table.write_human_readable(&mut stdout, !self.no_color)?;

        Ok(())
    }
}

fn load_pem(cert_content: &str) -> Result<Pem> {
    let pem_content = pem::parse(cert_content).context("Failed to parse pem")?;
    Ok(pem_content)
}

fn load_cert_from_der(pem_content: &Pem) -> Result<X509Certificate<'_>> {
    let (_, x509) = x509_parser::parse_x509_certificate(pem_content.contents())?;
    Ok(x509)
}

/// Awesome code taken from
/// https://github.com/casey/intermodal/blob/09279ab1a2db7ef6efd58c583ad19b2051fd7982/src/table.rs#L3
struct Table(Vec<(String, String)>);

impl Table {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, name: &str, value: String) {
        self.0.push((name.to_owned(), value))
    }

    fn rows(&self) -> &Vec<(String, String)> {
        &self.0
    }

    /// Find the longest name in rows and return its length.
    fn name_width(&self) -> usize {
        self.rows()
            .iter()
            .map(|row| row.0.len())
            // .map(|row| UnicodeWidthStr::width(row.0))
            .max()
            .unwrap_or(0)
    }

    fn write_human_readable(&self, out: &mut dyn Write, is_styled: bool) -> Result<()> {
        let name_width = self.name_width();

        for (name, value) in self.rows() {
            if is_styled {
                let s = style(name).blue().bold();
                self.write_name(out, s, name_width - name.len())?;
            } else {
                self.write_name(out, name, name_width - name.len())?;
            }

            writeln!(out, "  {}", value)?;
        }

        Ok(())
    }

    fn write_name(
        &self,
        out: &mut dyn Write,
        name: impl std::fmt::Display,
        width: usize,
    ) -> Result<()> {
        write!(
            out,
            "{:width$}{}",
            "",
            name,
            width = width,
            // width = name_width - UnicodeWidthStr::width(*name),
        )?;
        Ok(())
    }
}
