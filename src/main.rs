use std::net::SocketAddr;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, client::conn::http1::Builder, Uri, Method};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{self, AsyncWriteExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use bytes::Bytes;

// Function to tunnel HTTPS connections
async fn tunnel(client_stream: TcpStream, host: String, port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Connect to remote server
    let server_stream = match TcpStream::connect((host, port)).await {
        Ok(stream) => stream,
        Err(e) => {
            let mut client_stream = client_stream;
            let error_msg = format!("HTTP/1.1 502 Bad Gateway\r\n\r\n{}", e);
            client_stream.write_all(error_msg.as_bytes()).await?;
            return Err(Box::new(e));
        }
    };

    let mut client_stream = client_stream;
    // Send success response to client
    client_stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;

    // Split both streams
    let (mut client_read, mut client_write) = io::split(client_stream);
    let (mut server_read, mut server_write) = io::split(server_stream);

    // Copy bidirectionally
    let client_to_server = async {
        io::copy(&mut client_read, &mut server_write).await?;
        server_write.shutdown().await
    };

    let server_to_client = async {
        io::copy(&mut server_read, &mut client_write).await?;
        client_write.shutdown().await
    };

    // Run both directions concurrently
    tokio::try_join!(client_to_server, server_to_client)?;

    Ok(())
}

async fn forward_request(
    mut req: Request<Incoming>,
    client_stream: TcpStream,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
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

    // Set up client connection
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

    // Forward the request and get response
    let response = sender.send_request(req).await?;
    
    // Convert response body to BoxBody
    let (parts, body) = response.into_parts();
    let boxed_body = body.map_err(|e| e.into()).boxed();
    
    Ok(Response::from_parts(parts, boxed_body))
}

async fn connect_to_host(host: &str, port: u16) -> Result<TcpStream, std::io::Error> {
    TcpStream::connect((host, port)).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Bind to address
    let addr: SocketAddr = ([0, 0, 0, 0], 8080).into();
    let listener = TcpListener::bind(addr).await?;
    println!("Proxy server listening on http://{}", addr);

    // Accept connections
    loop {
        let (stream, _) = listener.accept().await?;
        
        // Spawn new task for each connection
        tokio::task::spawn(async move {
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
                                let error_body = Full::new(Bytes::from("Failed to establish tunnel"))
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
                match connect_to_host(host, port).await {
                    Ok(target_stream) => forward_request(req, target_stream).await,
                    Err(e) => {
                        eprintln!("Failed to connect to target server: {}", e);
                        let error_body = Full::new(Bytes::from("Failed to connect to target server"))
                            .map_err(|never| match never {})
                            .boxed();
                        Ok(Response::builder()
                            .status(502)
                            .body(error_body)
                            .unwrap())
                    }
                }
            });

            // Process HTTP connection
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service)
                .await {
                eprintln!("Server error: {}", err);
            }
        });
    }
}