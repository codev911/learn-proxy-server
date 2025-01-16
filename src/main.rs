use std::net::SocketAddr;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, client::conn::http1::Builder, Uri};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use bytes::Bytes;

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
            let service = hyper::service::service_fn(|req: Request<Incoming>| async {
                // Extract host and port from request
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
                        // Create an error response with matching error type
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