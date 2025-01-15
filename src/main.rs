use hyper::service::{make_service_fn, service_fn};
use hyper::{Client, Request, Response, Server, Body};
use hyper::client::HttpConnector;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use std::convert::Infallible;
use std::net::SocketAddr;

async fn forward_request(req: Request<Body>, client: Client<HttpConnector>) -> Result<Response<Body>, hyper::Error> {
    // Forward the request to the target server
    client.request(req).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Bind the proxy server to the address
    let addr: SocketAddr = ([0, 0, 0, 0], 8080).into();
    let listener = TcpListener::bind(addr).await?;
    let client = Client::new();

    // Create the service function
    let make_svc = make_service_fn(move |_| {
        let client = client.clone();
        async {
            Ok::<_, Infallible>(service_fn(move |req| {
                forward_request(req, client.clone())
            }))
        }
    });

    // Create and start the server
    let server = Server::builder(hyper::server::accept::from_stream(TcpListenerStream::new(listener)))
        .serve(make_svc);

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}