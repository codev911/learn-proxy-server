use std::net::SocketAddr;
use std::time::Duration;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, client::conn::http1::Builder, Uri, Method};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{self, AsyncWriteExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use bytes::Bytes;
use tokio::sync::Semaphore;
use std::sync::Arc;

// Constants for configuration
const MAX_CONCURRENT_CONNECTIONS: usize = 1000;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_SIZE: usize = 100 * 1024 * 1024; // 100MB

// Function to tunnel HTTPS connections with timeout
async fn tunnel(mut client_stream: TcpStream, host: String, port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Set timeouts
    client_stream.set_nodelay(true)?;
    
    // Connect to remote server with timeout
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

    // Send success response to client
    client_stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;

    // Split both streams
    let (mut client_read, mut client_write) = io::split(client_stream);
    let (mut server_read, mut server_write) = io::split(server_stream);

    // Copy bidirectionally with timeout
    let client_to_server = async {
        let result = tokio::time::timeout(
            CONNECTION_TIMEOUT,
            io::copy(&mut client_read, &mut server_write)
        ).await;
        match result {
            Ok(Ok(_)) => server_write.shutdown().await,
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(()),
        }
    };

    let server_to_client = async {
        let result = tokio::time::timeout(
            CONNECTION_TIMEOUT,
            io::copy(&mut server_read, &mut client_write)
        ).await;
        match result {
            Ok(Ok(_)) => client_write.shutdown().await,
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(()),
        }
    };

    // Run both directions concurrently
    tokio::select! {
        res1 = client_to_server => res1?,
        res2 = server_to_client => res2?,
    }

    Ok(())
}

async fn forward_request(
    mut req: Request<Incoming>,
    client_stream: TcpStream,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // Set timeout for client stream
    if let Err(e) = client_stream.set_nodelay(true) {
        let error_body = Full::new(Bytes::from(format!("Failed to set TCP_NODELAY: {}", e)))
            .map_err(|never| match never {})
            .boxed();
        return Ok(Response::builder()
            .status(500)
            .body(error_body)
            .unwrap());
    }

    // Extract the target URI from the request
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

    // Update request URI to only include path
    *req.uri_mut() = uri;

    // Set up client connection with timeout
    let io = TokioIo::new(client_stream);
    let (mut sender, conn) = Builder::new()
        .handshake(io)
        .await?;

    // Spawn connection handling task
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            eprintln!("Connection error: {}", err);
        }
    });

    // Forward the request with timeout
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
    
    // Convert response body to BoxBody
    let (parts, body) = response.into_parts();
    let boxed_body = body.map_err(|e| e.into()).boxed();
    
    Ok(Response::from_parts(parts, boxed_body))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Increase file descriptor limit
    #[cfg(unix)]
    {
        let nofile = rlimit::Resource::NOFILE;
        let (_soft, hard) = rlimit::getrlimit(nofile)?;
        rlimit::setrlimit(nofile, hard, hard)?;
    }

    // Create connection semaphore
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    // Bind to address
    let addr: SocketAddr = ([0, 0, 0, 0], 8080).into();
    let listener = TcpListener::bind(addr).await?;
    println!("Proxy server listening on http://{}", addr);

    // Accept connections
    loop {
        // Acquire semaphore permit
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                eprintln!("Semaphore error - too many connections");
                continue;
            }
        };

        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Failed to accept connection: {}", e);
                continue;
            }
        };

        println!("New connection from: {}", addr);
        
        // Spawn new task for each connection
        tokio::task::spawn(async move {
            let _permit = permit; // Keep permit alive for the duration of the connection
            let io = TokioIo::new(stream);
            
            // Create service function
            let service = hyper::service::service_fn(|req: Request<Incoming>| async move {
                // Handle CONNECT method differently (HTTPS)
                if req.method() == Method::CONNECT {
                    // Extract host and port from authority
                    if let Some(addr) = req.uri().authority() {
                        let host = addr.host().to_string();
                        let port = addr.port_u16().unwrap_or(443);
                        
                        // Create new connection for tunneling
                        match TcpStream::connect((host.clone(), port)).await {
                            Ok(tunnel_stream) => {
                                // Spawn HTTPS tunneling
                                tokio::spawn(async move {
                                    if let Err(e) = tunnel(tunnel_stream, host, port).await {
                                        eprintln!("Tunnel error: {}", e);
                                    }
                                });
                                
                                // Return an empty response
                                let empty_body = Full::new(Bytes::from(""))
                                    .map_err(|never| match never {})
                                    .boxed();
                                return Ok(Response::new(empty_body));
                            }
                            Err(e) => {
                                eprintln!("Failed to establish tunnel: {}", e);
                                let error_body = Full::new(Bytes::from(format!("Failed to establish tunnel: {}", e)))
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

                // Handle HTTP requests
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

                // Connect to target server
                match TcpStream::connect((host, port)).await {
                    Ok(target_stream) => forward_request(req, target_stream).await,
                    Err(e) => {
                        eprintln!("Failed to connect to target server: {}", e);
                        let error_body = Full::new(Bytes::from(format!("Failed to connect to target server: {}", e)))
                            .map_err(|never| match never {})
                            .boxed();
                        Ok(Response::builder()
                            .status(502)
                            .body(error_body)
                            .unwrap())
                    }
                }
            });

            // Process HTTP connection with timeout
            if let Err(err) = http1::Builder::new()
                .max_buf_size(MAX_REQUEST_SIZE)
                .serve_connection(io, service)
                .await {
                eprintln!("Server error: {}", err);
            }
        });
    }
}