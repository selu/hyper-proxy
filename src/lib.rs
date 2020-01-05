//! A Proxy Connector crate for Hyper based applications
//!
//! # Example
//! ```rust,no_run
//! use hyper::{Client, Request, Uri};
//! use hyper::client::HttpConnector;
//! use futures::{TryFutureExt, TryStreamExt};
//! use hyper_proxy::{Proxy, ProxyConnector, Intercept};
//! use typed_headers::Credentials;
//! use std::error::Error;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn Error>> {
//!     let proxy = {
//!         let proxy_uri = "http://my-proxy:8080".parse().unwrap();
//!         let mut proxy = Proxy::new(Intercept::All, proxy_uri);
//!         proxy.set_authorization(Credentials::basic("John Doe", "Agent1234").unwrap());
//!         let connector = HttpConnector::new();
//!         # #[cfg(not(any(feature = "tls", feature = "rustls")))]
//!         # let proxy_connector = ProxyConnector::from_proxy_unsecured(connector, proxy);
//!         # #[cfg(any(feature = "tls", feature = "rustls"))]
//!         let proxy_connector = ProxyConnector::from_proxy(connector, proxy).unwrap();
//!         proxy_connector
//!     };
//!
//!     // Connecting to http will trigger regular GETs and POSTs.
//!     // We need to manually append the relevant headers to the request
//!     let uri: Uri = "http://my-remote-website.com".parse().unwrap();
//!     let mut req = Request::get(uri.clone()).body(hyper::Body::empty()).unwrap();
//!
//!     if let Some(headers) = proxy.http_headers(&uri) {
//!         req.headers_mut().extend(headers.clone().into_iter());
//!     }
//!
//!     let client = Client::builder().build(proxy);
//!     let fut_http = client.request(req)
//!         .and_then(|res| res.into_body().map_ok(|x|x.to_vec()).try_concat())
//!         .map_ok(move |body| ::std::str::from_utf8(&body).unwrap().to_string());
//!
//!     // Connecting to an https uri is straightforward (uses 'CONNECT' method underneath)
//!     let uri = "https://my-remote-websitei-secured.com".parse().unwrap();
//!     let fut_https = client.get(uri)
//!         .and_then(|res| res.into_body().map_ok(|x|x.to_vec()).try_concat())
//!         .map_ok(move |body| ::std::str::from_utf8(&body).unwrap().to_string());
//!
//!     let (http_res, https_res) = futures::future::join(fut_http, fut_https).await;
//!     let (_, _) = (http_res?, https_res?);
//!
//!     Ok(())
//! }
//! ```

#![deny(missing_docs)]

mod stream;
mod tunnel;

use http::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{service::Service, Uri};

use futures::future::TryFutureExt;
use std::{fmt, io, sync::Arc};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use stream::ProxyStream;
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(feature = "tls")]
use native_tls::TlsConnector as NativeTlsConnector;

#[cfg(feature = "rustls")]
use tokio_rustls::TlsConnector;
#[cfg(feature = "tls")]
use tokio_tls::TlsConnector;
use typed_headers::{Authorization, Credentials, HeaderMapExt, ProxyAuthorization};
#[cfg(feature = "rustls")]
use webpki::DNSNameRef;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The Intercept enum to filter connections
#[derive(Debug, Clone)]
pub enum Intercept {
    /// All incoming connection will go through proxy
    All,
    /// Only http connections will go through proxy
    Http,
    /// Only https connections will go through proxy
    Https,
    /// No connection will go through this proxy
    None,
    /// A custom intercept
    Custom(Custom),
}

/// A trait for matching between Destination and Uri
pub trait Dst {
    /// Returns the connection scheme, e.g. "http" or "https"
    fn scheme(&self) -> Option<&str>;
    /// Returns the host of the connection
    fn host(&self) -> Option<&str>;
    /// Returns the port for the connection
    fn port(&self) -> Option<u16>;
}

impl Dst for Uri {
    fn scheme(&self) -> Option<&str> {
        self.scheme_str()
    }

    fn host(&self) -> Option<&str> {
        self.host()
    }

    fn port(&self) -> Option<u16> {
        self.port_u16()
    }
}

#[inline]
pub(crate) fn io_err<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

/// A Custom struct to proxy custom uris
#[derive(Clone)]
pub struct Custom(Arc<dyn Fn(Option<&str>, Option<&str>, Option<u16>) -> bool + Send + Sync>);

impl fmt::Debug for Custom {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "_")
    }
}

impl<F: Fn(Option<&str>, Option<&str>, Option<u16>) -> bool + Send + Sync + 'static> From<F>
    for Custom
{
    fn from(f: F) -> Custom {
        Custom(Arc::new(f))
    }
}

impl Intercept {
    /// A function to check if given `Uri` is proxied
    pub fn matches<D: Dst>(&self, uri: &D) -> bool {
        match (self, uri.scheme()) {
            (&Intercept::All, _)
            | (&Intercept::Http, Some("http"))
            | (&Intercept::Https, Some("https")) => true,
            (&Intercept::Custom(Custom(ref f)), _) => f(uri.scheme(), uri.host(), uri.port()),
            _ => false,
        }
    }
}

impl<F: Fn(Option<&str>, Option<&str>, Option<u16>) -> bool + Send + Sync + 'static> From<F>
    for Intercept
{
    fn from(f: F) -> Intercept {
        Intercept::Custom(f.into())
    }
}

/// A Proxy strcut
#[derive(Clone, Debug)]
pub struct Proxy {
    intercept: Intercept,
    headers: HeaderMap,
    uri: Uri,
}

impl Proxy {
    /// Create a new `Proxy`
    pub fn new<I: Into<Intercept>>(intercept: I, uri: Uri) -> Proxy {
        Proxy {
            intercept: intercept.into(),
            uri: uri,
            headers: HeaderMap::new(),
        }
    }

    /// Set `Proxy` authorization
    pub fn set_authorization(&mut self, credentials: Credentials) {
        match self.intercept {
            Intercept::Http => {
                self.headers.typed_insert(&Authorization(credentials));
            }
            Intercept::Https => {
                self.headers.typed_insert(&ProxyAuthorization(credentials));
            }
            _ => {
                self.headers
                    .typed_insert(&Authorization(credentials.clone()));
                self.headers.typed_insert(&ProxyAuthorization(credentials));
            }
        }
    }

    /// Set a custom header
    pub fn set_header(&mut self, name: HeaderName, value: HeaderValue) {
        self.headers.insert(name, value);
    }

    /// Get current intercept
    pub fn intercept(&self) -> &Intercept {
        &self.intercept
    }

    /// Get current `Headers` which must be sent to proxy
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Get proxy uri
    pub fn uri(&self) -> &Uri {
        &self.uri
    }
}

/// A wrapper around `Proxy`s with a connector.
#[derive(Clone)]
pub struct ProxyConnector<C> {
    proxies: Vec<Proxy>,
    connector: C,

    #[cfg(feature = "tls")]
    tls: Option<NativeTlsConnector>,

    #[cfg(feature = "rustls")]
    tls: Option<TlsConnector>,

    #[cfg(not(any(feature = "tls", feature = "rustls")))]
    tls: Option<()>,
}

impl<C: fmt::Debug> fmt::Debug for ProxyConnector<C> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(
            f,
            "ProxyConnector {}{{ proxies: {:?}, connector: {:?} }}",
            if self.tls.is_some() {
                ""
            } else {
                "(unsecured)"
            },
            self.proxies,
            self.connector
        )
    }
}

impl<C> ProxyConnector<C> {
    /// Create a new secured Proxies
    #[cfg(feature = "tls")]
    pub fn new(connector: C) -> Result<Self, io::Error> {
        let tls = NativeTlsConnector::builder()
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(ProxyConnector {
            proxies: Vec::new(),
            connector: connector,
            tls: Some(tls),
        })
    }

    /// Create a new secured Proxies
    #[cfg(feature = "rustls")]
    pub fn new(connector: C) -> Result<Self, io::Error> {
        let mut config = tokio_rustls::rustls::ClientConfig::new();

        config.root_store = rustls_native_certs::load_native_certs()?;

        let cfg = Arc::new(config);
        let tls = TlsConnector::from(cfg);

        Ok(ProxyConnector {
            proxies: Vec::new(),
            connector: connector,
            tls: Some(tls),
        })
    }

    /// Create a new unsecured Proxy
    pub fn unsecured(connector: C) -> Self {
        ProxyConnector {
            proxies: Vec::new(),
            connector: connector,
            tls: None,
        }
    }

    /// Create a proxy connector and attach a particular proxy
    #[cfg(any(feature = "tls", feature = "rustls"))]
    pub fn from_proxy(connector: C, proxy: Proxy) -> Result<Self, io::Error> {
        let mut c = ProxyConnector::new(connector)?;
        c.proxies.push(proxy);
        Ok(c)
    }

    /// Create a proxy connector and attach a particular proxy
    pub fn from_proxy_unsecured(connector: C, proxy: Proxy) -> Self {
        let mut c = ProxyConnector::unsecured(connector);
        c.proxies.push(proxy);
        c
    }

    /// Change proxy connector
    pub fn with_connector<CC>(self, connector: CC) -> ProxyConnector<CC> {
        ProxyConnector {
            connector: connector,
            proxies: self.proxies,
            tls: self.tls,
        }
    }

    /// Set or unset tls when tunneling
    #[cfg(any(feature = "tls"))]
    pub fn set_tls(&mut self, tls: Option<NativeTlsConnector>) {
        self.tls = tls;
    }

    /// Set or unset tls when tunneling
    #[cfg(any(feature = "rustls"))]
    pub fn set_tls(&mut self, tls: Option<TlsConnector>) {
        self.tls = tls;
    }

    /// Get the current proxies
    pub fn proxies(&self) -> &[Proxy] {
        &self.proxies
    }

    /// Add a new additional proxy
    pub fn add_proxy(&mut self, proxy: Proxy) {
        self.proxies.push(proxy);
    }

    /// Extend the list of proxies
    pub fn extend_proxies<I: IntoIterator<Item = Proxy>>(&mut self, proxies: I) {
        self.proxies.extend(proxies)
    }

    /// Get http headers for a matching uri
    ///
    /// These headers must be appended to the hyper Request for the proxy to work properly.
    /// This is needed only for http requests.
    pub fn http_headers(&self, uri: &Uri) -> Option<&HeaderMap> {
        if uri.scheme_str().map_or(true, |s| s != "http") {
            return None;
        }

        self.match_proxy(uri).map(|p| &p.headers)
    }

    fn match_proxy<D: Dst>(&self, uri: &D) -> Option<&Proxy> {
        self.proxies.iter().find(|p| p.intercept.matches(uri))
    }
}

macro_rules! mtry {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => break Err(e.into()),
        }
    };
}

impl<C> Service<Uri> for ProxyConnector<C>
where
    C: Service<Uri>,
    C::Response: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = ProxyStream<C::Response>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.connector.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io_err(e.into()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        if let (Some(ref p), Some(host)) = (self.match_proxy(&uri), uri.host()) {
            if uri.scheme_str() == Some("https")
            {
                let host = host.to_owned();
                let port = uri.port_u16().unwrap_or(443);
                let tunnel = tunnel::new(&host, port, &p.headers);
                let connection =
                    proxy_dst(&uri, &p.uri).map(|proxy_url| self.connector.call(proxy_url));
                let tls = self.tls.clone();
    
                Box::pin(async move {
                    loop {
                        // this hack will gone once `try_blocks` will eventually stabilized
                        let proxy_stream = mtry!(mtry!(connection).await.map_err(io_err));
                        let tunnel_stream = mtry!(tunnel.with_stream(proxy_stream).await);
    
                        break match tls {
                            #[cfg(feature = "tls")]
                            Some(tls) => {
                                let tls = TlsConnector::from(tls);
                                let secure_stream =
                                    mtry!(tls.connect(&host, tunnel_stream).await.map_err(io_err));
    
                                Ok(ProxyStream::Secured(secure_stream))
                            }
    
                            #[cfg(feature = "rustls")]
                            Some(tls) => {
                                let dnsref =
                                    mtry!(DNSNameRef::try_from_ascii_str(&host).map_err(io_err));
                                let tls = TlsConnector::from(tls);
                                let secure_stream =
                                    mtry!(tls.connect(dnsref, tunnel_stream).await.map_err(io_err));
    
                                Ok(ProxyStream::Secured(secure_stream))
                            }
    
                            #[cfg(not(any(feature = "tls", feature = "rustls")))]
                            Some(_) => panic!("hyper-proxy was not built with TLS support"),
    
                            None => Ok(ProxyStream::Regular(tunnel_stream)),
                        };
                    }
                })
            } else {
                let proxy_uri = proxy_dst(&uri, &p.uri).unwrap();
                Box::pin(
                    self.connector
                        .call(proxy_uri)
                        .map_ok(ProxyStream::Regular)
                        .map_err(|err| io_err(err.into()))
                )
            }
        } else {
            Box::pin(
                self.connector
                    .call(uri)
                    .map_ok(ProxyStream::Regular)
                    .map_err(|err| io_err(err.into())),
            )
        }
    }
}

fn proxy_dst(dst: &Uri, proxy: &Uri) -> io::Result<Uri> {
    Uri::builder()
        .scheme(
            proxy
                .scheme_str()
                .ok_or_else(|| io_err(format!("proxy uri missing scheme: {}", proxy)))?,
        )
        .authority(
            proxy
                .authority()
                .ok_or_else(|| io_err(format!("proxy uri missing host: {}", proxy)))?
                .clone(),
        )
        .path_and_query(dst.path_and_query().unwrap().clone())
        .build()
        .map_err(|err| io_err(format!("other error: {}", err)))
}
