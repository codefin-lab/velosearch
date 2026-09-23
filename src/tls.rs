//! Serving over TLS.
//!
//! OpenSearch's security plugin puts the REST layer behind TLS by default;
//! so does this server once told to. The certificate and key are read from
//! the config directory (PEM), or made up as a self-signed pair the first
//! time nothing is there, the way the plugin's demo configuration does.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use serde_json::Value;

/// What the server was told about its TLS, from settings and environment.
#[derive(Clone, Debug, Default)]
pub struct TlsSettings {
    pub enabled: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub trusted_cas: Option<PathBuf>,
    /// whether a client certificate is asked for
    pub client_auth: String,
}

/// The config directory: `VELOSEARCH_CONFIG`, else `<data>/config`, else
/// `./config`.
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("VELOSEARCH_CONFIG") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("VELOSEARCH_DATA")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("config");
    }
    PathBuf::from("config")
}

/// The node's own settings file, `config/velosearch.yml`, read as JSON-like
/// YAML; an absent file is an empty one.
pub fn node_settings() -> Value {
    let path = config_dir().join("velosearch.yml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Value::Object(Default::default()) };
    serde_yaml::from_str::<serde_yaml::Value>(&text)
        .ok()
        .and_then(|y| serde_json::to_value(y).ok())
        .unwrap_or(Value::Object(Default::default()))
}

/// One dotted setting, from the environment first (`VELOSEARCH_` + the
/// dotted name upper-cased with `_`), then the settings file.
pub fn node_setting(settings: &Value, key: &str) -> Option<String> {
    let env_name = format!(
        "VELOSEARCH_{}",
        key.trim_start_matches("plugins.security.").replace('.', "_").to_ascii_uppercase()
    );
    if let Ok(v) = std::env::var(&env_name) {
        return Some(v);
    }
    // the full dotted name spelled out is read as well
    let full = format!("VELOSEARCH_{}", key.replace('.', "_").to_ascii_uppercase());
    if let Ok(v) = std::env::var(&full) {
        return Some(v);
    }
    // written flat, or nested
    if let Some(v) = settings.get(key) {
        return Some(text_of(v));
    }
    let mut cur = settings;
    for part in key.split('.') {
        cur = cur.get(part)?;
    }
    Some(text_of(cur))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl TlsSettings {
    pub fn read(settings: &Value) -> TlsSettings {
        let get = |k: &str| node_setting(settings, k);
        let enabled = get("plugins.security.ssl.http.enabled")
            .or_else(|| get("http.ssl.enabled"))
            .map(|v| v == "true")
            .unwrap_or(false);
        let dir = config_dir();
        let path_of = |v: Option<String>| -> Option<PathBuf> {
            v.map(|p| {
                let p = PathBuf::from(p);
                if p.is_absolute() { p } else { dir.join(p) }
            })
        };
        TlsSettings {
            enabled,
            cert: path_of(get("plugins.security.ssl.http.pemcert_filepath")),
            key: path_of(get("plugins.security.ssl.http.pemkey_filepath")),
            trusted_cas: path_of(get("plugins.security.ssl.http.pemtrustedcas_filepath")),
            client_auth: get("plugins.security.ssl.http.clientauth_mode")
                .unwrap_or_else(|| "OPTIONAL".into()),
        }
    }
}

/// The certificate and key to serve with: the ones named, or a self-signed
/// pair written into the config directory the first time.
pub fn load_or_make(
    settings: &TlsSettings,
) -> anyhow::Result<(
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
)> {
    let dir = config_dir();
    let cert_path = settings.cert.clone().unwrap_or_else(|| dir.join("certs").join("node.pem"));
    let key_path = settings.key.clone().unwrap_or_else(|| dir.join("certs").join("node-key.pem"));
    if !cert_path.exists() || !key_path.exists() {
        if settings.cert.is_some() || settings.key.is_some() {
            anyhow::bail!(
                "TLS certificate or key not found: {} / {}",
                cert_path.display(),
                key_path.display()
            );
        }
        make_self_signed(&cert_path, &key_path)?;
        eprintln!("velosearch: made a self-signed certificate at {}", cert_path.display());
    }
    // PEM is read by rustls's own types rather than by `rustls-pemfile`,
    // which is archived upstream (RUSTSEC-2025-0134) and was in its last
    // release a wrapper around exactly this code.
    let certs = CertificateDer::pem_file_iter(&cert_path)?.collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .map_err(|e| anyhow::anyhow!("no private key in {}: {e}", key_path.display()))?;
    Ok((certs, key))
}

fn make_self_signed(cert_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "velosearch node");
    params.distinguished_name.push(rcgen::DnType::OrganizationName, "VeloSearch");
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key.serialize_pem())?;
    Ok(())
}

/// Serve the router over TLS on the listener, one task per connection.
/// How long a peer has to finish a TLS handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: Router,
    settings: &TlsSettings,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let (certs, key) = load_or_make(settings)?;
    // a client certificate is asked for when a trust store is named:
    // `clientauth_mode` says whether it must be presented
    let builder = rustls::ServerConfig::builder();
    let mode = settings.client_auth.to_ascii_uppercase();
    let mut config = match (&settings.trusted_cas, mode.as_str()) {
        (Some(ca_path), "OPTIONAL" | "REQUIRE") => {
            let mut roots = rustls::RootCertStore::empty();
            let pem = std::fs::read(ca_path)?;
            for c in CertificateDer::pem_slice_iter(&pem).flatten() {
                let _ = roots.add(c);
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots));
            let verifier = if mode == "REQUIRE" {
                verifier.build()?
            } else {
                verifier.allow_unauthenticated().build()?
            };
            builder.with_client_cert_verifier(verifier).with_single_cert(certs, key)?
        }
        (None, "REQUIRE") => {
            // asking for a certificate that nothing can verify is not asking
            // for one: it would let every client in and say it had not
            anyhow::bail!(
                "plugins.security.ssl.http.clientauth_mode is REQUIRE but \
                 plugins.security.ssl.http.pemtrustedcas_filepath names nothing: there is \
                 nothing to verify a client certificate against"
            );
        }
        _ => builder.with_no_client_auth().with_single_cert(certs, key)?,
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    // a client that comes back resumes rather than shaking hands again:
    // tickets for TLS 1.3, a session cache for TLS 1.2 (rustls issues
    // neither unless told to)
    if let Ok(ticketer) = rustls::crypto::ring::Ticketer::new() {
        config.ticketer = ticketer;
    }
    // one ticket is enough for a client that will resume; a second is a
    // record the client must read and decrypt for nothing
    config.send_tls13_tickets = 1;
    config.session_storage = rustls::server::ServerSessionMemoryCache::new(8192);
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    tokio::pin!(shutdown);
    // the connections being served: a shutdown that dropped them would cut
    // every request in flight off mid-answer, which the plain listener has
    // never done
    let mut serving = tokio::task::JoinSet::new();
    loop {
        // finished connections are reaped as they finish rather than piling
        // up until the shutdown
        while serving.try_join_next().is_some() {}
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = &mut shutdown => {
                let grace = std::time::Duration::from_secs(30);
                let waited = tokio::time::timeout(grace, async {
                    while serving.join_next().await.is_some() {}
                })
                .await;
                if waited.is_err() {
                    eprintln!(
                        "velosearch: stopping with requests still in flight after {}s",
                        grace.as_secs()
                    );
                }
                return Ok(());
            }
        };
        let (stream, peer) = match accepted {
            Ok(s) => s,
            Err(_) => continue,
        };
        // an answer goes out as soon as it is written: waiting to fill a
        // packet costs a request more than the packet saves
        let _ = stream.set_nodelay(true);
        let acceptor = acceptor.clone();
        let app = app.clone();
        serving.spawn(async move {
            // A handshake that never finishes is a task and a socket held
            // for as long as the peer cares to hold them: a few thousand
            // half-open ClientHellos and the node has no descriptors left.
            // The plain listener is bounded by `Lenient`'s own head timeout,
            // which cannot help here because it sits above the handshake.
            let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await;
            let Ok(Ok(tls)) = accepted else { return };
            // who the connection is from: the peer address, and the subject
            // of the client certificate when one was presented
            let peer_dn = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .and_then(|c| x509_parser::parse_x509_certificate(c.as_ref()).ok())
                .map(|(_, cert)| crate::security::normalize_dn(&cert.subject().to_string()));
            // the same lenient request line the plain listener reads
            // the answer is gathered before it is encrypted: hyper writes a
            // response in pieces, and each piece on its own would be a TLS
            // record and a packet of its own
            let buffered = tokio::io::BufWriter::with_capacity(32 * 1024, tls);
            let io = hyper_util::rt::TokioIo::new(crate::http_compat::Lenient::new(buffered));
            let mut app = app;
            app = app.layer(axum::Extension(axum::extract::ConnectInfo(peer)));
            if let Some(dn) = peer_dn {
                app = app.layer(axum::Extension(crate::security::layer::PeerDn(dn)));
            }
            let service = hyper_util::service::TowerToHyperService::new(app);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, service)
                    .await;
        });
    }
}

/// What the node was told about the transport's TLS.
///
/// The transport is node to node, so it is mutual: every connection presents
/// a certificate and verifies the one it is given. `nodes_dn` narrows what a
/// verified certificate is allowed to be -- a certificate issued to a person
/// chains to the same CA and is not a node.
#[derive(Clone, Debug, Default)]
pub struct TransportTls {
    pub enabled: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub trusted_cas: Option<PathBuf>,
    pub nodes_dn: Vec<String>,
}

impl TransportTls {
    pub fn read(settings: &Value) -> TransportTls {
        let get = |k: &str| node_setting(settings, k);
        let dir = config_dir();
        let path_of = |v: Option<String>| -> Option<PathBuf> {
            v.map(|p| {
                let p = PathBuf::from(p);
                if p.is_absolute() { p } else { dir.join(p) }
            })
        };
        let nodes_dn = get("plugins.security.nodes_dn")
            .map(|v| {
                v.trim_matches(['[', ']'].as_slice())
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        TransportTls {
            enabled: get("plugins.security.ssl.transport.enabled")
                .or_else(|| get("transport.ssl.enabled"))
                .map(|v| v == "true")
                .unwrap_or(false),
            cert: path_of(get("plugins.security.ssl.transport.pemcert_filepath")),
            key: path_of(get("plugins.security.ssl.transport.pemkey_filepath")),
            trusted_cas: path_of(get("plugins.security.ssl.transport.pemtrustedcas_filepath")),
            nodes_dn,
        }
    }

    /// Whether a verified certificate's subject is one a node may have.
    /// With nothing named, any certificate the CA signed is a node.
    pub fn is_a_node(&self, dn: &str) -> bool {
        // an empty list is not "everyone": a transport with TLS on refuses
        // to start without one, so reaching here with none is a mistake and
        // the answer is no
        if self.nodes_dn.is_empty() {
            return false;
        }
        let dn = crate::security::normalize_dn(dn);
        self.nodes_dn.iter().any(|pattern| {
            let pattern = crate::security::normalize_dn(pattern);
            crate::store::glob_match(&pattern, &dn)
        })
    }

    /// Whether the operator has said which certificates are nodes.
    ///
    /// The authority that signs a node's certificate is usually the one that
    /// signs a person's, and a person's certificate reaching the transport
    /// is a person with a node's privileges. Naming the subjects is the only
    /// thing that tells them apart, so transport TLS without `nodes_dn` is
    /// refused rather than trusted.
    fn named_its_nodes(&self) -> anyhow::Result<()> {
        if self.nodes_dn.is_empty() {
            anyhow::bail!(
                "transport TLS is on but plugins.security.nodes_dn names nothing: any \
                 certificate the authority signed would be a node of this cluster, including \
                 one it issued to a person. Name the subjects a node may have, for example \
                 plugins.security.nodes_dn: ['CN=*.nodes.example.com']"
            );
        }
        Ok(())
    }

    fn material(
        &self,
    ) -> anyhow::Result<(
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
        rustls::RootCertStore,
    )> {
        let as_http = TlsSettings {
            enabled: true,
            cert: self.cert.clone(),
            key: self.key.clone(),
            trusted_cas: self.trusted_cas.clone(),
            client_auth: "REQUIRE".into(),
        };
        let (certs, key) = load_or_make(&as_http)?;
        let Some(ca_path) = self.trusted_cas.clone() else {
            anyhow::bail!(
                "transport TLS is on but plugins.security.ssl.transport.pemtrustedcas_filepath \
                 names nothing: without it there is nothing to verify a peer against"
            );
        };
        let mut roots = rustls::RootCertStore::empty();
        let pem = std::fs::read(&ca_path)?;
        let mut added = 0usize;
        for c in CertificateDer::pem_slice_iter(&pem).flatten() {
            if roots.add(c).is_ok() {
                added += 1;
            }
        }
        if added == 0 {
            anyhow::bail!("no certificate authority in {}", ca_path.display());
        }
        Ok((certs, key, roots))
    }

    /// How this node answers a connection: a certificate is required, and it
    /// must be one the cluster's authority signed.
    pub fn server_config(&self) -> anyhow::Result<rustls::ServerConfig> {
        self.named_its_nodes()?;
        let (certs, key, roots) = self.material()?;
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
        Ok(rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)?)
    }

    /// How this node opens one.
    pub fn client_config(&self) -> anyhow::Result<rustls::ClientConfig> {
        self.named_its_nodes()?;
        let (certs, key, roots) = self.material()?;
        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)?)
    }
}

/// The subject of the certificate a peer presented, if it presented one.
pub fn peer_subject(
    certs: Option<&[rustls::pki_types::CertificateDer<'static>]>,
) -> Option<String> {
    certs
        .and_then(|chain| chain.first())
        .and_then(|c| x509_parser::parse_x509_certificate(c.as_ref()).ok())
        .map(|(_, cert)| crate::security::normalize_dn(&cert.subject().to_string()))
}
