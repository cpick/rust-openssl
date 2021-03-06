use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};

use dh::Dh;
use error::ErrorStack;
use ssl::{self, HandshakeError, Ssl, SslRef, SslContext, SslContextBuilder, SslMethod, SslStream,
          SSL_VERIFY_PEER};
use pkey::PKeyRef;
use version;
#[cfg(target_os = "android")]
use x509::X509;
use x509::X509Ref;

#[cfg(ossl101)]
lazy_static! {
    static ref HOSTNAME_IDX: ::ex_data::Index<Ssl, String> = Ssl::new_ex_index().unwrap();
}

// ffdhe2048 from https://wiki.mozilla.org/Security/Server_Side_TLS#ffdhe2048
const DHPARAM_PEM: &'static str = "
-----BEGIN DH PARAMETERS-----
MIIBCAKCAQEA//////////+t+FRYortKmq/cViAnPTzx2LnFg84tNpWp4TZBFGQz
+8yTnc4kmz75fS/jY2MMddj2gbICrsRhetPfHtXV/WVhJDP1H18GbtCFY2VVPe0a
87VXE15/V8k1mE8McODmi3fipona8+/och3xWKE2rec1MKzKT0g6eXq8CrGCsyT7
YdEIqUuyyOP7uWrat2DX9GgdT0Kj3jlN9K5W7edjcrsZCwenyO4KbXCeAvzhzffi
7MA0BM0oNC9hkXL+nOmFg/+OTxIy7vKBg8P+OxtMb61zO7X8vC7CIAXFjvGDfRaD
ssbzSibBsu/6iGtCOGEoXJf//////////wIBAg==
-----END DH PARAMETERS-----
";

fn ctx(method: SslMethod) -> Result<SslContextBuilder, ErrorStack> {
    let mut ctx = SslContextBuilder::new(method)?;

    let mut opts = ssl::SSL_OP_ALL;
    opts &= !ssl::SSL_OP_NETSCAPE_REUSE_CIPHER_CHANGE_BUG;
    opts &= !ssl::SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS;
    opts |= ssl::SSL_OP_NO_TICKET;
    opts |= ssl::SSL_OP_NO_COMPRESSION;
    opts |= ssl::SSL_OP_NO_SSLV2;
    opts |= ssl::SSL_OP_NO_SSLV3;
    opts |= ssl::SSL_OP_SINGLE_DH_USE;
    opts |= ssl::SSL_OP_SINGLE_ECDH_USE;
    opts |= ssl::SSL_OP_CIPHER_SERVER_PREFERENCE;
    ctx.set_options(opts);

    let mut mode = ssl::SSL_MODE_AUTO_RETRY | ssl::SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER
        | ssl::SSL_MODE_ENABLE_PARTIAL_WRITE;

    // This is quite a useful optimization for saving memory, but historically
    // caused CVEs in OpenSSL pre-1.0.1h, according to
    // https://bugs.python.org/issue25672
    if version::number() >= 0x1000108f {
        mode |= ssl::SSL_MODE_RELEASE_BUFFERS;
    }

    ctx.set_mode(mode);

    Ok(ctx)
}

/// A builder for `SslConnector`s.
pub struct SslConnectorBuilder(SslContextBuilder);

impl SslConnectorBuilder {
    /// Creates a new builder for TLS connections.
    ///
    /// The default configuration is subject to change, and is currently derived from Python.
    pub fn new(method: SslMethod) -> Result<SslConnectorBuilder, ErrorStack> {
        let mut ctx = ctx(method)?;
        ctx.set_default_verify_paths()?;

        #[cfg(target_os = "android")]
        {
            use std::fs;
            use std::io::Read;

            let cert_store = ctx.cert_store_mut();

            if let Ok(certs) = fs::read_dir("/system/etc/security/cacerts") {
                for entry in certs.filter_map(|r| r.ok()).filter(|e| e.path().is_file()) {
                    let mut cert = String::new();
                    if let Ok(_) = fs::File::open(entry.path())
                            .and_then(|mut f| f.read_to_string(&mut cert)) {
                        if let Ok(cert) = X509::from_pem(cert.as_bytes()) {
                            try!(cert_store.add_cert(cert));
                        }
                    }
                }
            }
        }

        // From https://github.com/python/cpython/blob/a170fa162dc03f0a014373349e548954fff2e567/Lib/ssl.py#L193
        ctx.set_cipher_list(
            "TLS13-AES-256-GCM-SHA384:TLS13-CHACHA20-POLY1305-SHA256:\
             TLS13-AES-128-GCM-SHA256:\
             ECDH+AESGCM:ECDH+CHACHA20:DH+AESGCM:DH+CHACHA20:ECDH+AES256:DH+AES256:\
             ECDH+AES128:DH+AES:ECDH+HIGH:DH+HIGH:RSA+AESGCM:RSA+AES:RSA+HIGH:\
             !aNULL:!eNULL:!MD5:!3DES",
        )?;
        setup_verify(&mut ctx);

        Ok(SslConnectorBuilder(ctx))
    }

    #[deprecated(since = "0.9.23",
                 note = "SslConnectorBuilder now implements Deref<Target=SslContextBuilder>")]
    pub fn builder(&self) -> &SslContextBuilder {
        self
    }

    #[deprecated(since = "0.9.23",
                 note = "SslConnectorBuilder now implements DerefMut<Target=SslContextBuilder>")]
    pub fn builder_mut(&mut self) -> &mut SslContextBuilder {
        self
    }

    /// Consumes the builder, returning an `SslConnector`.
    pub fn build(self) -> SslConnector {
        SslConnector(self.0.build())
    }
}

impl Deref for SslConnectorBuilder {
    type Target = SslContextBuilder;

    fn deref(&self) -> &SslContextBuilder {
        &self.0
    }
}

impl DerefMut for SslConnectorBuilder {
    fn deref_mut(&mut self) -> &mut SslContextBuilder {
        &mut self.0
    }
}

/// A type which wraps client-side streams in a TLS session.
///
/// OpenSSL's default configuration is highly insecure. This connector manages the OpenSSL
/// structures, configuring cipher suites, session options, hostname verification, and more.
///
/// OpenSSL's built in hostname verification is used when linking against OpenSSL 1.0.2 or 1.1.0,
/// and a custom implementation is used when linking against OpenSSL 1.0.1.
#[derive(Clone)]
pub struct SslConnector(SslContext);

impl SslConnector {
    /// Initiates a client-side TLS session on a stream.
    ///
    /// The domain is used for SNI and hostname verification.
    pub fn connect<S>(&self, domain: &str, stream: S) -> Result<SslStream<S>, HandshakeError<S>>
    where
        S: Read + Write,
    {
        self.configure()?.connect(domain, stream)
    }

    /// Initiates a client-side TLS session on a stream without performing hostname verification.
    ///
    /// # Warning
    ///
    /// You should think very carefully before you use this method. If hostname verification is not
    /// used, *any* valid certificate for *any* site will be trusted for use from any other. This
    /// introduces a significant vulnerability to man-in-the-middle attacks.
    pub fn danger_connect_without_providing_domain_for_certificate_verification_and_server_name_indication<
        S,
    >(
        &self,
        stream: S,
    ) -> Result<SslStream<S>, HandshakeError<S>>
    where
        S: Read + Write,
    {
        self.configure()?
            .danger_connect_without_providing_domain_for_certificate_verification_and_server_name_indication(stream)
    }

    /// Returns a structure allowing for configuration of a single TLS session before connection.
    pub fn configure(&self) -> Result<ConnectConfiguration, ErrorStack> {
        Ssl::new(&self.0).map(ConnectConfiguration)
    }
}

/// A type which allows for configuration of a client-side TLS session before connection.
pub struct ConnectConfiguration(Ssl);

impl ConnectConfiguration {
    #[deprecated(since = "0.9.23",
                 note = "ConnectConfiguration now implements Deref<Target=SslRef>")]
    pub fn ssl(&self) -> &Ssl {
        &self.0
    }

    #[deprecated(since = "0.9.23",
                 note = "ConnectConfiguration now implements DerefMut<Target=SslRef>")]
    pub fn ssl_mut(&mut self) -> &mut Ssl {
        &mut self.0
    }

    /// Initiates a client-side TLS session on a stream.
    ///
    /// The domain is used for SNI and hostname verification.
    pub fn connect<S>(mut self, domain: &str, stream: S) -> Result<SslStream<S>, HandshakeError<S>>
    where
        S: Read + Write,
    {
        self.0.set_hostname(domain)?;
        setup_verify_hostname(&mut self.0, domain)?;

        self.0.connect(stream)
    }

    /// Initiates a client-side TLS session on a stream without performing hostname verification.
    ///
    /// The verification configuration of the connector's `SslContext` is not overridden.
    ///
    /// # Warning
    ///
    /// You should think very carefully before you use this method. If hostname verification is not
    /// used, *any* valid certificate for *any* site will be trusted for use from any other. This
    /// introduces a significant vulnerability to man-in-the-middle attacks.
    pub fn danger_connect_without_providing_domain_for_certificate_verification_and_server_name_indication<
        S,
    >(
        self,
        stream: S,
    ) -> Result<SslStream<S>, HandshakeError<S>>
    where
        S: Read + Write,
    {
        self.0.connect(stream)
    }
}

impl Deref for ConnectConfiguration {
    type Target = SslRef;

    fn deref(&self) -> &SslRef {
        &self.0
    }
}

impl DerefMut for ConnectConfiguration {
    fn deref_mut(&mut self) -> &mut SslRef {
        &mut self.0
    }
}

/// A builder for `SslAcceptor`s.
pub struct SslAcceptorBuilder(SslContextBuilder);

impl SslAcceptorBuilder {
    /// Creates a new builder configured to connect to non-legacy clients. This should generally be
    /// considered a reasonable default choice.
    ///
    /// This corresponds to the intermediate configuration of Mozilla's server side TLS
    /// recommendations. See its [documentation][docs] for more details on specifics.
    ///
    /// [docs]: https://wiki.mozilla.org/Security/Server_Side_TLS
    pub fn mozilla_intermediate<I>(
        method: SslMethod,
        private_key: &PKeyRef,
        certificate: &X509Ref,
        chain: I,
    ) -> Result<SslAcceptorBuilder, ErrorStack>
    where
        I: IntoIterator,
        I::Item: AsRef<X509Ref>,
    {
        let builder = SslAcceptorBuilder::mozilla_intermediate_raw(method)?;
        builder.finish_setup(private_key, certificate, chain)
    }

    /// Creates a new builder configured to connect to modern clients.
    ///
    /// This corresponds to the modern configuration of Mozilla's server side TLS recommendations.
    /// See its [documentation][docs] for more details on specifics.
    ///
    /// [docs]: https://wiki.mozilla.org/Security/Server_Side_TLS
    pub fn mozilla_modern<I>(
        method: SslMethod,
        private_key: &PKeyRef,
        certificate: &X509Ref,
        chain: I,
    ) -> Result<SslAcceptorBuilder, ErrorStack>
    where
        I: IntoIterator,
        I::Item: AsRef<X509Ref>,
    {
        let builder = SslAcceptorBuilder::mozilla_modern_raw(method)?;
        builder.finish_setup(private_key, certificate, chain)
    }

    /// Like `mozilla_intermediate`, but does not load the certificate chain and private key.
    pub fn mozilla_intermediate_raw(method: SslMethod) -> Result<SslAcceptorBuilder, ErrorStack> {
        let mut ctx = ctx(method)?;
        let dh = Dh::from_pem(DHPARAM_PEM.as_bytes())?;
        ctx.set_tmp_dh(&dh)?;
        setup_curves(&mut ctx)?;
        ctx.set_cipher_list(
            "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
             ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
             ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
             DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES128-SHA256:\
             ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:\
             ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA384:ECDHE-ECDSA-AES256-SHA:\
             ECDHE-RSA-AES256-SHA:DHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA:DHE-RSA-AES256-SHA256:\
             DHE-RSA-AES256-SHA:ECDHE-ECDSA-DES-CBC3-SHA:ECDHE-RSA-DES-CBC3-SHA:\
             EDH-RSA-DES-CBC3-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:AES256-SHA256:\
             AES128-SHA:AES256-SHA:DES-CBC3-SHA:!DSS",
        )?;
        Ok(SslAcceptorBuilder(ctx))
    }

    /// Like `mozilla_modern`, but does not load the certificate chain and private key.
    pub fn mozilla_modern_raw(method: SslMethod) -> Result<SslAcceptorBuilder, ErrorStack> {
        let mut ctx = ctx(method)?;
        setup_curves(&mut ctx)?;
        ctx.set_cipher_list(
            "ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
             ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
             ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-SHA384:\
             ECDHE-RSA-AES256-SHA384:ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256",
        )?;
        Ok(SslAcceptorBuilder(ctx))
    }

    fn finish_setup<I>(
        mut self,
        private_key: &PKeyRef,
        certificate: &X509Ref,
        chain: I,
    ) -> Result<SslAcceptorBuilder, ErrorStack>
    where
        I: IntoIterator,
        I::Item: AsRef<X509Ref>,
    {
        self.0.set_private_key(private_key)?;
        self.0.set_certificate(certificate)?;
        self.0.check_private_key()?;
        for cert in chain {
            self.0.add_extra_chain_cert(cert.as_ref().to_owned())?;
        }
        Ok(self)
    }

    #[deprecated(since = "0.9.23",
                 note = "SslAcceptorBuilder now implements Deref<Target=SslContextBuilder>")]
    pub fn builder(&self) -> &SslContextBuilder {
        self
    }

    #[deprecated(since = "0.9.23",
                 note = "SslAcceptorBuilder now implements DerefMut<Target=SslContextBuilder>")]
    pub fn builder_mut(&mut self) -> &mut SslContextBuilder {
        self
    }

    /// Consumes the builder, returning a `SslAcceptor`.
    pub fn build(self) -> SslAcceptor {
        SslAcceptor(self.0.build())
    }
}

impl Deref for SslAcceptorBuilder {
    type Target = SslContextBuilder;

    fn deref(&self) -> &SslContextBuilder {
        &self.0
    }
}

impl DerefMut for SslAcceptorBuilder {
    fn deref_mut(&mut self) -> &mut SslContextBuilder {
        &mut self.0
    }
}

#[cfg(ossl101)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    use ec::EcKey;
    use nid;

    let curve = EcKey::from_curve_name(nid::X9_62_PRIME256V1)?;
    ctx.set_tmp_ecdh(&curve)
}

#[cfg(ossl102)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    ctx._set_ecdh_auto(true)
}

#[cfg(ossl110)]
fn setup_curves(_: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    Ok(())
}

/// A type which wraps server-side streams in a TLS session.
///
/// OpenSSL's default configuration is highly insecure. This connector manages the OpenSSL
/// structures, configuring cipher suites, session options, and more.
#[derive(Clone)]
pub struct SslAcceptor(SslContext);

impl SslAcceptor {
    /// Initiates a server-side TLS session on a stream.
    pub fn accept<S>(&self, stream: S) -> Result<SslStream<S>, HandshakeError<S>>
    where
        S: Read + Write,
    {
        let ssl = Ssl::new(&self.0)?;
        ssl.accept(stream)
    }
}

#[cfg(any(ossl102, ossl110))]
fn setup_verify(ctx: &mut SslContextBuilder) {
    ctx.set_verify(SSL_VERIFY_PEER);
}

#[cfg(ossl101)]
fn setup_verify(ctx: &mut SslContextBuilder) {
    ctx.set_verify_callback(SSL_VERIFY_PEER, |p, x509| {
        let hostname = match x509.ssl() {
            Ok(Some(ssl)) => ssl.ex_data(*HOSTNAME_IDX),
            _ => None,
        };
        match hostname {
            Some(hostname) => verify::verify_callback(hostname, p, x509),
            None => p,
        }
    });
}

#[cfg(any(ossl102, ossl110))]
fn setup_verify_hostname(ssl: &mut Ssl, domain: &str) -> Result<(), ErrorStack> {
    let param = ssl._param_mut();
    param.set_hostflags(::verify::X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS);
    match domain.parse() {
        Ok(ip) => param.set_ip(ip),
        Err(_) => param.set_host(domain),
    }
}

#[cfg(ossl101)]
fn setup_verify_hostname(ssl: &mut Ssl, domain: &str) -> Result<(), ErrorStack> {
    let domain = domain.to_string();
    ssl.set_ex_data(*HOSTNAME_IDX, domain);
    Ok(())
}

#[cfg(ossl101)]
mod verify {
    use std::net::IpAddr;
    use std::str;

    use nid;
    use x509::{GeneralName, X509NameRef, X509Ref, X509StoreContextRef};
    use stack::Stack;

    pub fn verify_callback(
        domain: &str,
        preverify_ok: bool,
        x509_ctx: &X509StoreContextRef,
    ) -> bool {
        if !preverify_ok || x509_ctx.error_depth() != 0 {
            return preverify_ok;
        }

        match x509_ctx.current_cert() {
            Some(x509) => verify_hostname(domain, &x509),
            None => true,
        }
    }

    fn verify_hostname(domain: &str, cert: &X509Ref) -> bool {
        match cert.subject_alt_names() {
            Some(names) => verify_subject_alt_names(domain, names),
            None => verify_subject_name(domain, &cert.subject_name()),
        }
    }

    fn verify_subject_alt_names(domain: &str, names: Stack<GeneralName>) -> bool {
        let ip = domain.parse();

        for name in &names {
            match ip {
                Ok(ip) => {
                    if let Some(actual) = name.ipaddress() {
                        if matches_ip(&ip, actual) {
                            return true;
                        }
                    }
                }
                Err(_) => {
                    if let Some(pattern) = name.dnsname() {
                        if matches_dns(pattern, domain, false) {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    fn verify_subject_name(domain: &str, subject_name: &X509NameRef) -> bool {
        if let Some(pattern) = subject_name.entries_by_nid(nid::COMMONNAME).next() {
            let pattern = match str::from_utf8(pattern.data().as_slice()) {
                Ok(pattern) => pattern,
                Err(_) => return false,
            };

            // Unlike with SANs, IP addresses in the subject name don't have a
            // different encoding. We need to pass this down to matches_dns to
            // disallow wildcard matches with bogus patterns like *.0.0.1
            let is_ip = domain.parse::<IpAddr>().is_ok();

            if matches_dns(&pattern, domain, is_ip) {
                return true;
            }
        }

        false
    }

    fn matches_dns(mut pattern: &str, mut hostname: &str, is_ip: bool) -> bool {
        // first strip trailing . off of pattern and hostname to normalize
        if pattern.ends_with('.') {
            pattern = &pattern[..pattern.len() - 1];
        }
        if hostname.ends_with('.') {
            hostname = &hostname[..hostname.len() - 1];
        }

        matches_wildcard(pattern, hostname, is_ip).unwrap_or_else(|| pattern == hostname)
    }

    fn matches_wildcard(pattern: &str, hostname: &str, is_ip: bool) -> Option<bool> {
        // IP addresses and internationalized domains can't involved in wildcards
        if is_ip || pattern.starts_with("xn--") {
            return None;
        }

        let wildcard_location = match pattern.find('*') {
            Some(l) => l,
            None => return None,
        };

        let mut dot_idxs = pattern.match_indices('.').map(|(l, _)| l);
        let wildcard_end = match dot_idxs.next() {
            Some(l) => l,
            None => return None,
        };

        // Never match wildcards if the pattern has less than 2 '.'s (no *.com)
        //
        // This is a bit dubious, as it doesn't disallow other TLDs like *.co.uk.
        // Chrome has a black- and white-list for this, but Firefox (via NSS) does
        // the same thing we do here.
        //
        // The Public Suffix (https://www.publicsuffix.org/) list could
        // potentially be used here, but it's both huge and updated frequently
        // enough that management would be a PITA.
        if dot_idxs.next().is_none() {
            return None;
        }

        // Wildcards can only be in the first component
        if wildcard_location > wildcard_end {
            return None;
        }

        let hostname_label_end = match hostname.find('.') {
            Some(l) => l,
            None => return None,
        };

        // check that the non-wildcard parts are identical
        if pattern[wildcard_end..] != hostname[hostname_label_end..] {
            return Some(false);
        }

        let wildcard_prefix = &pattern[..wildcard_location];
        let wildcard_suffix = &pattern[wildcard_location + 1..wildcard_end];

        let hostname_label = &hostname[..hostname_label_end];

        // check the prefix of the first label
        if !hostname_label.starts_with(wildcard_prefix) {
            return Some(false);
        }

        // and the suffix
        if !hostname_label[wildcard_prefix.len()..].ends_with(wildcard_suffix) {
            return Some(false);
        }

        Some(true)
    }

    fn matches_ip(expected: &IpAddr, actual: &[u8]) -> bool {
        match (expected, actual.len()) {
            (&IpAddr::V4(ref addr), 4) => actual == addr.octets(),
            (&IpAddr::V6(ref addr), 16) => {
                let segments = [
                    ((actual[0] as u16) << 8) | actual[1] as u16,
                    ((actual[2] as u16) << 8) | actual[3] as u16,
                    ((actual[4] as u16) << 8) | actual[5] as u16,
                    ((actual[6] as u16) << 8) | actual[7] as u16,
                    ((actual[8] as u16) << 8) | actual[9] as u16,
                    ((actual[10] as u16) << 8) | actual[11] as u16,
                    ((actual[12] as u16) << 8) | actual[13] as u16,
                    ((actual[14] as u16) << 8) | actual[15] as u16,
                ];
                segments == addr.segments()
            }
            _ => false,
        }
    }
}
