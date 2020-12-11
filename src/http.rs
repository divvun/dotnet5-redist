use std::{net::ToSocketAddrs, sync::Arc};

use anyhow::{Context, Error, bail, Result};
use async_rustls::{TlsConnector, rustls::ClientConfig, webpki::DNSNameRef};
use http_types::{Method, Request, Response, Url};
use smol::{Async, io};

pub async fn get(url: &str) -> Result<Response> {
    let url = Url::parse(&url)?;
    let req = Request::new(Method::Get, url);

    fetch(req).await
}

/// Sends a request and fetches the response.
async fn fetch(req: Request) -> Result<Response> {
    // Figure out the host and the port.
    let host = req.url().host().context("cannot parse host")?.to_string();
    let port = req
        .url()
        .port_or_known_default()
        .context("cannot guess port")?;

    // Connect to the host.
    let socket_addr = {
        let host = host.clone();
        smol::unblock(move || (host.as_str(), port).to_socket_addrs())
            .await?
            .next()
            .context("cannot resolve address")?
    };
    let stream = Async::<std::net::TcpStream>::connect(socket_addr).await?;
    
    // Send the request and wait for the response.
    let resp = match req.url().scheme() {
        "http" => async_h1::connect(stream, req).await.map_err(Error::msg)?,
        "https" => {
            let mut config = ClientConfig::new();
            config
                .root_store
                .add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
            let connector = TlsConnector::from(Arc::new(config));

            let stream = smol::net::TcpStream::connect(&socket_addr).await?;

            let domain = DNSNameRef::try_from_ascii_str(&host)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dnsname"))?;

            let stream = connector.connect(domain, stream).await?;
            async_h1::connect(stream, req).await.map_err(Error::msg)?
        }
        scheme => bail!("unsupported scheme: {}", scheme),
    };
    Ok(resp)
}
