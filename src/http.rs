//! Shared HTTP client construction: trust roots, extra CA bundles and TLS error hints.
//!
//! Every `reqwest` client in the crate starts from [`client_builder`], so a corporate root CA set in
//! `OUTLOOK_EXTRA_CA_PEM`, `SSL_CERT_FILE` or `NODE_EXTRA_CA_CERTS` reaches Graph, the token
//! endpoints and the updater alike. Warnings go to stderr only: stdout carries `--json` output and
//! the MCP stdio protocol.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Environment variables read for extra root CAs, in this order. All of them are additive.
pub(crate) const EXTRA_CA_ENV_VARS: [&str; 3] =
    ["OUTLOOK_EXTRA_CA_PEM", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"];

/// Text added to an error whose cause is an untrusted server certificate.
pub(crate) const TLS_TRUST_HINT: &str = "TLS certificate was not trusted (unknown issuer). If your \
network uses a TLS-inspecting proxy, export its root CA as PEM and set OUTLOOK_EXTRA_CA_PEM=<path> \
(SSL_CERT_FILE and NODE_EXTRA_CA_CERTS are also honoured), or install the CA into the OS trust \
store. Run 'outlook doctor' to see which CA sources were loaded.";

/// What happened to one extra CA source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CaSourceReport {
    /// The environment variable that named the file.
    pub source: &'static str,
    pub path: PathBuf,
    /// At least one certificate from the file is in use.
    pub loaded: bool,
    /// Certificates in use from this file.
    pub certs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Extra root certificates ready to add to a client, with a report per source.
#[derive(Debug, Clone)]
pub(crate) struct ExtraCas {
    certs: Vec<reqwest::Certificate>,
    reports: Vec<CaSourceReport>,
    /// `false` when the OS store cannot be used: `SSL_CERT_FILE` redirects it (see
    /// `rustls-native-certs`) to a file without one usable certificate, which would make every
    /// client fail to build.
    native_roots: bool,
}

impl Default for ExtraCas {
    fn default() -> Self {
        Self {
            certs: Vec::new(),
            reports: Vec::new(),
            native_roots: true,
        }
    }
}

impl ExtraCas {
    pub(crate) fn reports(&self) -> &[CaSourceReport] {
        &self.reports
    }

    pub(crate) fn native_roots(&self) -> bool {
        self.native_roots
    }
}

/// The set values of [`EXTRA_CA_ENV_VARS`] from `lookup`, in order: trimmed, non-empty,
/// tilde-expanded, a path named twice kept once.
pub(crate) fn extra_ca_sources_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(&'static str, PathBuf)> {
    let mut sources: Vec<(&'static str, PathBuf)> = Vec::new();
    for var in EXTRA_CA_ENV_VARS {
        let Some(value) = lookup(var) else {
            continue;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = crate::config::expand_tilde(trimmed);
        if !sources.iter().any(|(_, seen)| seen == &path) {
            sources.push((var, path));
        }
    }
    sources
}

/// Reads and checks every source. Never fails: a missing, unreadable or invalid file is reported
/// and skipped.
pub(crate) fn load_extra_cas(sources: Vec<(&'static str, PathBuf)>) -> ExtraCas {
    let mut cas = ExtraCas::default();
    for (source, path) in sources {
        let (certs, error) = match load_pem_file(&path) {
            Ok((certs, rejected)) if certs.is_empty() => (
                certs,
                Some(if rejected > 0 {
                    format!("none of the {rejected} certificates could be used as a root CA")
                } else {
                    "no PEM certificates found".to_string()
                }),
            ),
            Ok((certs, 0)) => (certs, None),
            Ok((certs, rejected)) => (
                certs,
                Some(format!("skipped {rejected} certificates that could not be used")),
            ),
            Err(err) => (Vec::new(), Some(err)),
        };
        if source == "SSL_CERT_FILE" && certs.is_empty() {
            cas.native_roots = false;
        }
        cas.reports.push(CaSourceReport {
            source,
            path,
            loaded: !certs.is_empty(),
            certs: certs.len(),
            error,
        });
        cas.certs.extend(certs);
    }
    cas
}

/// The usable certificates of a PEM bundle and how many others it held.
fn load_pem_file(path: &Path) -> std::result::Result<(Vec<reqwest::Certificate>, usize), String> {
    let bytes = fs::read(path).map_err(|err| format!("cannot read file: {err}"))?;
    let certs = reqwest::Certificate::from_pem_bundle(&bytes)
        .map_err(|err| format!("not a PEM certificate bundle: {err}"))?;
    // A certificate reqwest cannot add makes the whole client fail to build, so keep only those
    // that a client accepts on their own.
    if usable_roots(&certs) {
        return Ok((certs, 0));
    }
    let total = certs.len();
    let usable: Vec<_> = certs
        .into_iter()
        .filter(|cert| usable_roots(std::slice::from_ref(cert)))
        .collect();
    let rejected = total - usable.len();
    Ok((usable, rejected))
}

/// Whether a client with exactly these extra roots (and no built-in ones, to keep it cheap) builds.
fn usable_roots(certs: &[reqwest::Certificate]) -> bool {
    certs
        .iter()
        .fold(
            reqwest::Client::builder().tls_built_in_root_certs(false),
            |builder, cert| builder.add_root_certificate(cert.clone()),
        )
        .build()
        .is_ok()
}

/// The extra CAs named by this process's environment, loaded once; problems are printed to
/// stderr the first time.
pub(crate) fn extra_cas() -> &'static ExtraCas {
    static CAS: OnceLock<ExtraCas> = OnceLock::new();
    CAS.get_or_init(|| {
        let cas = load_extra_cas(extra_ca_sources_from(|var| std::env::var(var).ok()));
        for report in cas.reports() {
            if let Some(error) = &report.error {
                eprintln!(
                    "warning: {}={}: {error}",
                    report.source,
                    report.path.display()
                );
            }
        }
        if !cas.native_roots() {
            eprintln!(
                "warning: SSL_CERT_FILE has no usable certificates; using the bundled root CAs only"
            );
        }
        cas
    })
}

/// `reqwest::Client::builder()` trusting the bundled Mozilla roots, the OS store and the extra
/// CAs from the environment.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    client_builder_with(extra_cas())
}

/// [`client_builder`] with an explicit set of extra CAs.
pub(crate) fn client_builder_with(cas: &ExtraCas) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().tls_built_in_native_certs(cas.native_roots);
    cas.certs
        .iter()
        .fold(builder, |builder, cert| builder.add_root_certificate(cert.clone()))
}

/// [`TLS_TRUST_HINT`] when any cause in the chain is an untrusted server certificate. Matching
/// text on purpose: it holds across the rustls versions of reqwest and this crate.
pub(crate) fn tls_trust_hint(err: &anyhow::Error) -> Option<&'static str> {
    err.chain()
        .any(|cause| {
            let text = cause.to_string();
            text.contains("UnknownIssuer")
                || text.contains("invalid peer certificate")
                || text.contains("self-signed certificate")
                || text.contains("self signed certificate")
        })
        .then_some(TLS_TRUST_HINT)
}

/// Adds [`TLS_TRUST_HINT`] as outer context when it applies.
pub(crate) fn with_tls_hint(err: anyhow::Error) -> anyhow::Error {
    match tls_trust_hint(&err) {
        Some(hint) => err.context(hint),
        None => err,
    }
}

/// `.tls_hint()` on a result: [`with_tls_hint`] on its error.
pub(crate) trait TlsHintExt<T> {
    fn tls_hint(self) -> anyhow::Result<T>;
}

impl<T> TlsHintExt<T> for anyhow::Result<T> {
    fn tls_hint(self) -> anyhow::Result<T> {
        self.map_err(with_tls_hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
    };

    struct TestPki {
        ca_pem: String,
        server_config: Arc<rustls::ServerConfig>,
    }

    /// A root CA and a `localhost` server certificate it signed.
    fn test_pki() -> TestPki {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "outlook-cli test root");
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let leaf_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
            )
            .unwrap();
        TestPki {
            ca_pem: ca.pem(),
            server_config: Arc::new(server_config),
        }
    }

    /// An HTTPS server on 127.0.0.1 answering `ok` to every request; returns its port.
    fn spawn_tls_server(config: Arc<rustls::ServerConfig>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for tcp in listener.incoming().flatten() {
                let Ok(conn) = rustls::ServerConnection::new(config.clone()) else {
                    continue;
                };
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                // A client that rejects the certificate ends the handshake here.
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        port
    }

    fn temp_file(name: &str, contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn extra_ca_sources_keep_env_order_and_drop_blank_and_duplicate_paths() {
        let env: HashMap<&str, &str> = HashMap::from([
            ("OUTLOOK_EXTRA_CA_PEM", " /etc/corp/root.pem "),
            ("SSL_CERT_FILE", "/etc/corp/root.pem"),
            ("NODE_EXTRA_CA_CERTS", "/opt/node/extra.pem"),
        ]);
        let sources = extra_ca_sources_from(|var| env.get(var).map(|v| v.to_string()));
        assert_eq!(
            sources,
            vec![
                ("OUTLOOK_EXTRA_CA_PEM", PathBuf::from("/etc/corp/root.pem")),
                ("NODE_EXTRA_CA_CERTS", PathBuf::from("/opt/node/extra.pem")),
            ]
        );

        let env: HashMap<&str, &str> =
            HashMap::from([("OUTLOOK_EXTRA_CA_PEM", "   "), ("SSL_CERT_FILE", "")]);
        assert!(extra_ca_sources_from(|var| env.get(var).map(|v| v.to_string())).is_empty());

        let home = dirs::home_dir().unwrap();
        let sources = extra_ca_sources_from(|var| {
            (var == "NODE_EXTRA_CA_CERTS").then(|| "~/corp.pem".to_string())
        });
        assert_eq!(sources, vec![("NODE_EXTRA_CA_CERTS", home.join("corp.pem"))]);
    }

    #[test]
    fn pem_bundles_load_every_certificate_and_bad_files_never_fail_the_client() {
        let bundle = format!("{}\n{}", test_pki().ca_pem, test_pki().ca_pem);
        let (_dir, good) = temp_file("bundle.pem", bundle.as_bytes());
        let (_dir2, garbage) = temp_file("garbage.pem", b"this is not a certificate\n");
        let broken = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let (_dir3, invalid) = temp_file("invalid.pem", broken.as_bytes());
        let missing = good.with_file_name("missing.pem");

        let cas = load_extra_cas(vec![
            ("OUTLOOK_EXTRA_CA_PEM", good.clone()),
            ("NODE_EXTRA_CA_CERTS", garbage.clone()),
            ("SSL_CERT_FILE", missing.clone()),
            ("SSL_CERT_FILE", invalid.clone()),
        ]);
        let reports = cas.reports();
        assert_eq!(reports.len(), 4);
        assert!(reports[0].loaded && reports[0].certs == 2 && reports[0].error.is_none());
        for report in &reports[1..] {
            assert!(!report.loaded, "{report:?}");
            assert_eq!(report.certs, 0);
            assert!(report.error.is_some(), "{report:?}");
        }
        assert_eq!(cas.certs.len(), 2);
        // SSL_CERT_FILE replaces the OS store in rustls-native-certs; an unusable one turns it off.
        assert!(!cas.native_roots());

        client_builder_with(&cas)
            .build()
            .expect("bad CA files must not stop the client from building");
        client_builder_with(&ExtraCas::default()).build().unwrap();
    }

    #[test]
    fn tls_hint_matches_untrusted_certificates_only() {
        let untrusted = anyhow::anyhow!("invalid peer certificate: UnknownIssuer")
            .context("client error (Connect)")
            .context("Graph HTTP request failed (GET https://graph.microsoft.com/v1.0/me)");
        assert_eq!(tls_trust_hint(&untrusted), Some(TLS_TRUST_HINT));
        let wrapped = format!("{:#}", with_tls_hint(untrusted));
        assert!(wrapped.starts_with("TLS certificate was not trusted"), "{wrapped}");
        assert!(wrapped.contains("Graph HTTP request failed"), "{wrapped}");

        for other in [
            anyhow::anyhow!("operation timed out").context("Graph HTTP request failed"),
            anyhow::anyhow!("Connection refused (os error 10061)").context("client error (Connect)"),
        ] {
            assert_eq!(tls_trust_hint(&other), None);
            let text = format!("{:#}", with_tls_hint(other));
            assert!(!text.contains("OUTLOOK_EXTRA_CA_PEM"), "{text}");
        }
        let ok: anyhow::Result<u8> = Ok(1);
        assert_eq!(ok.tls_hint().unwrap(), 1);
    }

    #[tokio::test]
    async fn private_ca_fails_with_the_hint_and_works_once_supplied() {
        let pki = test_pki();
        let port = spawn_tls_server(pki.server_config.clone());
        let url = format!("https://localhost:{port}/");

        let err = client_builder_with(&ExtraCas::default())
            .build()
            .unwrap()
            .get(&url)
            .send()
            .await
            .expect_err("a private CA must not be trusted by default");
        let text = format!("{:#}", with_tls_hint(anyhow::Error::new(err)));
        assert!(text.contains("OUTLOOK_EXTRA_CA_PEM"), "{text}");
        assert!(text.contains("outlook doctor"), "{text}");

        let (_dir, ca_file) = temp_file("corp-root.pem", pki.ca_pem.as_bytes());
        let cas = load_extra_cas(vec![("OUTLOOK_EXTRA_CA_PEM", ca_file)]);
        assert_eq!(cas.reports()[0].certs, 1);
        let body = client_builder_with(&cas)
            .build()
            .unwrap()
            .get(&url)
            .send()
            .await
            .expect("the extra CA must be trusted")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
    }
}
