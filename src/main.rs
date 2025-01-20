use std::net::SocketAddr;
use std::time::Duration;
use std::sync::Arc;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, client::conn::http1::Builder, Uri, Method};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{self, AsyncWriteExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use bytes::Bytes;
use tokio::sync::Semaphore;
use rustls::ServerConfig;
use std::fs::File;
use std::io::BufReader;
use tokio_rustls::TlsAcceptor;

// Constants for configuration
const MAX_CONCURRENT_CONNECTIONS: usize = 1000;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_SIZE: usize = 100 * 1024 * 1024; // 100MB

// Function to load TLS configuration
fn load_tls_config() -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cert_file = &mut BufReader::new(File::open("cert.pem")?);
    let key_file = &mut BufReader::new(File::open("key.pem")?);

    let certs: Vec<_> = rustls_pemfile::certs(cert_file)
        .filter_map(|result| result.ok())
        .collect();

    if certs.is_empty() {
        return Err("No certificate found in cert.pem".into());
    }

    let key = match rustls_pemfile::private_key(key_file)? {
        Some(key) => key,
        None => return Err("No private key found in key.pem".into()),
    };

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| format!("TLS config error: {}", err))?;

    Ok(config)
}

// Function to handle HTTP(S) tunneling
async fn tunnel(mut client_stream: TcpStream, host: String, port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client_stream.set_nodelay(true)?;
    
    let server_stream = match tokio::time::timeout(
        CONNECTION_TIMEOUT,
        TcpStream::connect((host, port))
    ).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            let error_msg = format!("HTTP/1.1 502 Bad Gateway\r\n\r\n{}", e);
            client_stream.write_all(error_msg.as_bytes()).await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            let error_msg = b"HTTP/1.1 504 Gateway Timeout\r\n\r\n";
            client_stream.write_all(error_msg).await?;
            return Err("Connection timeout".into());
        }
    };

    server_stream.set_nodelay(true)?;
    client_stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;

    let (mut client_read, mut client_write) = io::split(client_stream);
    let (mut server_read, mut server_write) = io::split(server_stream);

    tokio::select! {
        res1 = io::copy(&mut client_read, &mut server_write) => {
            if let Ok(_) = res1 {
                server_write.shutdown().await?;
            }
        }
        res2 = io::copy(&mut server_read, &mut client_write) => {
            if let Ok(_) = res2 {
                client_write.shutdown().await?;
            }
        }
    }

    Ok(())
}

// Function to forward HTTP requests
async fn forward_request(
    mut req: Request<Incoming>,
    client_stream: TcpStream,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    if let Err(e) = client_stream.set_nodelay(true) {
        let error_body = Full::new(Bytes::from(format!("Failed to set TCP_NODELAY: {}", e)))
            .map_err(|never| match never {})
            .boxed();
        return Ok(Response::builder()
            .status(500)
            .body(error_body)
            .unwrap());
    }

    let uri = if let Some(host) = req.headers().get("host") {
        let host_str = host.to_str().unwrap_or("").to_string();
        let path = req.uri().path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        
        format!("http://{}{}", host_str, path)
            .parse::<Uri>()
            .unwrap_or_else(|_| req.uri().clone())
    } else {
        req.uri().clone()
    };

    *req.uri_mut() = uri;

    let io = TokioIo::new(client_stream);
    let (mut sender, conn) = Builder::new()
        .handshake(io)
        .await?;

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            eprintln!("Connection error: {}", err);
        }
    });

    let response = match tokio::time::timeout(
        CONNECTION_TIMEOUT,
        sender.send_request(req)
    ).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            let error_body = Full::new(Bytes::from(format!("Request failed: {}", e)))
                .map_err(|never| match never {})
                .boxed();
            return Ok(Response::builder()
                .status(502)
                .body(error_body)
                .unwrap());
        }
        Err(_) => {
            let error_body = Full::new(Bytes::from("Request timeout"))
                .map_err(|never| match never {})
                .boxed();
            return Ok(Response::builder()
                .status(504)
                .body(error_body)
                .unwrap());
        }
    };
    
    let (parts, body) = response.into_parts();
    let boxed_body = body.map_err(|e| e.into()).boxed();
    
    Ok(Response::from_parts(parts, boxed_body))
}

// Function to handle HTTP connections
async fn handle_http(stream: TcpStream, addr: SocketAddr) {
    println!("New HTTP connection from: {}", addr);
    
    let io = TokioIo::new(stream);
    
    let service = hyper::service::service_fn(|req: Request<Incoming>| async move {
        if req.method() == Method::CONNECT {
            if let Some(addr) = req.uri().authority() {
                let host = addr.host().to_string();
                let port = addr.port_u16().unwrap_or(443);
                
                match TcpStream::connect((host.clone(), port)).await {
                    Ok(tunnel_stream) => {
                        tokio::spawn(async move {
                            if let Err(e) = tunnel(tunnel_stream, host, port).await {
                                eprintln!("Tunnel error: {}", e);
                            }
                        });
                        
                        let empty_body = Full::new(Bytes::from(""))
                            .map_err(|never| match never {})
                            .boxed();
                        return Ok(Response::new(empty_body));
                    }
                    Err(e) => {
                        let error_body = Full::new(Bytes::from(format!("Tunnel failed: {}", e)))
                            .map_err(|never| match never {})
                            .boxed();
                        return Ok(Response::builder()
                            .status(502)
                            .body(error_body)
                            .unwrap());
                    }
                }
            }
        }

        let host = req.headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        
        let port = if host.contains(":") {
            host.split(":").nth(1).unwrap_or("80").parse().unwrap_or(80)
        } else {
            80
        };
        
        let host = host.split(":").next().unwrap_or("localhost");

        match TcpStream::connect((host, port)).await {
            Ok(target_stream) => forward_request(req, target_stream).await,
            Err(e) => {
                let error_body = Full::new(Bytes::from(format!("Connection failed: {}", e)))
                    .map_err(|never| match never {})
                    .boxed();
                Ok(Response::builder()
                    .status(502)
                    .body(error_body)
                    .unwrap())
            }
        }
    });

    if let Err(err) = http1::Builder::new()
        .max_buf_size(MAX_REQUEST_SIZE)
        .serve_connection(io, service)
        .await {
        eprintln!("HTTP Server error: {}", err);
    }
}

// Function to handle HTTPS connections
async fn handle_https(stream: TcpStream, addr: SocketAddr, tls_acceptor: TlsAcceptor) {
    println!("New HTTPS connection from: {}", addr);
    
    let tls_stream = match tls_acceptor.accept(stream).await {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("TLS error: {}", e);
            return;
        }
    };
    
    let io = TokioIo::new(tls_stream);
    
    let service = hyper::service::service_fn(|req: Request<Incoming>| async move {
        if req.method() == Method::CONNECT {
            if let Some(addr) = req.uri().authority() {
                let host = addr.host().to_string();
                let port = addr.port_u16().unwrap_or(443);
                
                match TcpStream::connect((host.clone(), port)).await {
                    Ok(tunnel_stream) => {
                        tokio::spawn(async move {
                            if let Err(e) = tunnel(tunnel_stream, host, port).await {
                                eprintln!("Tunnel error: {}", e);
                            }
                        });
                        
                        let empty_body = Full::new(Bytes::from(""))
                            .map_err(|never| match never {})
                            .boxed();
                        return Ok(Response::new(empty_body));
                    }
                    Err(e) => {
                        let error_body = Full::new(Bytes::from(format!("Tunnel failed: {}", e)))
                            .map_err(|never| match never {})
                            .boxed();
                        return Ok(Response::builder()
                            .status(502)
                            .body(error_body)
                            .unwrap());
                    }
                }
            }
        }

        let host = req.headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        
        let port = if host.contains(":") {
            host.split(":").nth(1).unwrap_or("80").parse().unwrap_or(80)
        } else {
            80
        };
        
        let host = host.split(":").next().unwrap_or("localhost");

        match TcpStream::connect((host, port)).await {
            Ok(target_stream) => forward_request(req, target_stream).await,
            Err(e) => {
                let error_body = Full::new(Bytes::from(format!("Connection failed: {}", e)))
                    .map_err(|never| match never {})
                    .boxed();
                Ok(Response::builder()
                    .status(502)
                    .body(error_body)
                    .unwrap())
            }
        }
    });

    if let Err(err) = http1::Builder::new()
        .max_buf_size(MAX_REQUEST_SIZE)
        .serve_connection(io, service)
        .await {
        eprintln!("HTTPS Server error: {}", err);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        let nofile = rlimit::Resource::NOFILE;
        let (_soft, hard) = rlimit::getrlimit(nofile)?;
        rlimit::setrlimit(nofile, hard, hard)?;
    }

    let tls_config = load_tls_config()?;
    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    // Create HTTP listener
    let http_addr: SocketAddr = ([0, 0, 0, 0], 8080).into();
    let http_listener = TcpListener::bind(http_addr).await?;
    println!("HTTP Proxy server listening on http://{}", http_addr);

    // Create HTTPS listener
    let https_addr: SocketAddr = ([0, 0, 0, 0], 8443).into();
    let https_listener = TcpListener::bind(https_addr).await?;
    println!("HTTPS Proxy server listening on https://{}", https_addr);

    loop {
        let semaphore = semaphore.clone();
        let tls_acceptor = tls_acceptor.clone();

        tokio::select! {
            Ok((http_stream, addr)) = http_listener.accept() => {
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };

                tokio::spawn(async move {
                    let _permit = permit;
                    handle_http(http_stream, addr).await;
                });
            }
            Ok((https_stream, addr)) = https_listener.accept() => {
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };

                tokio::spawn(async move {
                    let _permit = permit;
                    handle_https(https_stream, addr, tls_acceptor).await;
                });
            }
        }
    }
}