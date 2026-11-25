//! TLS to Postgres, by the connection string's `sslmode`, with libpq's meanings:
//!
//! - `disable`: plaintext.
//! - `prefer` (tokio-postgres's default) and `require`: encrypted, the server's certificate NOT verified. This is what
//!   libpq does too, and why it is not the mode to use across a network you do not trust.
//! - `verify-full`: encrypted, the chain verified against the Mozilla roots (or `sslrootcert=<pem file>`), and the
//!   certificate verified to name the host. Use this for a hosted Postgres.
//! - `verify-ca` (the chain without the host name) is refused: say `verify-full`, or `require` knowingly.
//!
//! tokio-postgres itself parses only `disable`, `prefer` and `require`, so this reads `sslmode` and `sslrootcert`
//! from a URL-form connection string before handing it on.

use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use std::{path::PathBuf, sync::Arc};
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ssl {
    Disable,
    /// Encrypt if the server offers it; nothing verified.
    Prefer,
    /// Encrypt or fail; nothing verified.
    Require,
    /// Encrypt or fail; chain and host name verified.
    VerifyFull {
        root: Option<PathBuf>,
    },
}

/// The connection string with `sslmode`/`sslrootcert` taken out (tokio-postgres would refuse `verify-full`), and the
/// mode they asked for. A string not in URL form is passed through, and its own `sslmode` is tokio-postgres's.
pub fn parse(conn: &str) -> anyhow::Result<(String, Ssl)> {
    if !(conn.starts_with("postgres://") || conn.starts_with("postgresql://")) {
        let pg: tokio_postgres::Config = conn.parse()?;
        let ssl = match pg.get_ssl_mode() {
            SslMode::Disable => Ssl::Disable,
            SslMode::Require => Ssl::Require,
            _ => Ssl::Prefer,
        };
        return Ok((conn.to_string(), ssl));
    }
    let mut u = url::Url::parse(conn)?;
    let mut mode: Option<String> = None;
    let mut root: Option<PathBuf> = None;
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter_map(|(k, v)| match k.as_ref() {
            "sslmode" => {
                mode = Some(v.into_owned());
                None
            }
            "sslrootcert" => {
                root = Some(PathBuf::from(v.as_ref()));
                None
            }
            _ => Some((k.into_owned(), v.into_owned())),
        })
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    let ssl = match mode.as_deref() {
        None | Some("prefer" | "allow") => Ssl::Prefer,
        Some("disable") => Ssl::Disable,
        Some("require") => Ssl::Require,
        Some("verify-full") => Ssl::VerifyFull { root },
        Some("verify-ca") => anyhow::bail!(
            "sslmode=verify-ca is not supported: use verify-full (chain and host name), or require (encryption only)"
        ),
        Some(other) => anyhow::bail!("unknown sslmode `{other}`"),
    };
    Ok((u.to_string(), ssl))
}

/// The tokio-postgres configuration and the TLS connector for a connection string.
pub fn configure(conn: &str) -> anyhow::Result<(tokio_postgres::Config, MakeRustlsConnect)> {
    let (rest, ssl) = parse(conn)?;
    let mut pg: tokio_postgres::Config = rest.parse()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;
    let client = match &ssl {
        Ssl::Disable => {
            pg.ssl_mode(SslMode::Disable);
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Unverified(provider)))
                .with_no_client_auth()
        }
        Ssl::Prefer | Ssl::Require => {
            pg.ssl_mode(if ssl == Ssl::Require {
                SslMode::Require
            } else {
                SslMode::Prefer
            });
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Unverified(provider)))
                .with_no_client_auth()
        }
        Ssl::VerifyFull { root } => {
            pg.ssl_mode(SslMode::Require);
            let mut roots = RootCertStore::empty();
            match root {
                Some(path) => {
                    let pem = std::fs::read(path)
                        .map_err(|e| anyhow::anyhow!("sslrootcert {}: {e}", path.display()))?;
                    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                        roots.add(cert?)?;
                    }
                    if roots.is_empty() {
                        anyhow::bail!("sslrootcert {} holds no certificate", path.display());
                    }
                }
                None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    Ok((pg, MakeRustlsConnect::new(client)))
}

/// libpq's `require`: the session is encrypted and its handshake signatures are checked, but the certificate's chain
/// and name are not. It protects against a passive observer, not against someone who can intercept the connection.
#[derive(Debug)]
struct Unverified(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sslmode_and_strips_it() {
        let (rest, ssl) =
            parse("postgres://u:p@db.example:5432/app?sslmode=verify-full&application_name=rebacd")
                .unwrap();
        assert_eq!(ssl, Ssl::VerifyFull { root: None });
        assert_eq!(
            rest,
            "postgres://u:p@db.example:5432/app?application_name=rebacd"
        );
        let (rest, ssl) = parse("postgresql://u@h/app?sslmode=require").unwrap();
        assert_eq!((rest.as_str(), ssl), ("postgresql://u@h/app", Ssl::Require));
        let (_, ssl) =
            parse("postgres://u@h/app?sslmode=verify-full&sslrootcert=/etc/ca.pem").unwrap();
        assert_eq!(
            ssl,
            Ssl::VerifyFull {
                root: Some("/etc/ca.pem".into())
            }
        );
        assert_eq!(parse("postgres://u@h/app").unwrap().1, Ssl::Prefer);
        assert_eq!(
            parse("postgres://u@h/app?sslmode=disable").unwrap().1,
            Ssl::Disable
        );
    }

    #[test]
    fn refuses_verify_ca_and_nonsense() {
        assert!(parse("postgres://u@h/app?sslmode=verify-ca").is_err());
        assert!(parse("postgres://u@h/app?sslmode=sometimes").is_err());
    }

    #[test]
    fn key_value_strings_pass_through() {
        let (rest, ssl) = parse("host=h user=u dbname=app sslmode=require").unwrap();
        assert_eq!(
            (rest.as_str(), ssl),
            ("host=h user=u dbname=app sslmode=require", Ssl::Require)
        );
    }

    #[test]
    fn configures_every_mode() {
        for m in ["disable", "prefer", "require", "verify-full"] {
            configure(&format!("postgres://u@h/app?sslmode={m}")).unwrap();
        }
        assert!(
            configure("postgres://u@h/app?sslmode=verify-full&sslrootcert=/nonexistent.pem")
                .is_err()
        );
    }
}
