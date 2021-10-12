use std::{convert::Infallible, net::SocketAddr, time::Duration};

use hyper::{service::service_fn, Body, Request, Response, Server};
use tower::make::Shared;
use underload::LoadShed;

#[tokio::main]
async fn main() {
    // We'll bind to 127.0.0.1:3000
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    let service = service_fn(hello_world);
    let service = LoadShed::new(service, 0.2, Duration::from_millis(100));

    let server = Server::bind(&addr).serve(Shared::new(service));

    // Run this server for... forever!
    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

async fn hello_world(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    tokio::time::sleep(Duration::from_millis(10)).await;
    Ok(Response::new("Hello, World".into()))
}
