use http::{Method, Request, Uri};
use hyper::{
    client::{conn::Builder, connect::HttpConnector, service::Connect, Client},
    service::Service,
    Body,
};
use tracing::info;
use tracing_tower::InstrumentableService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter("tower=trace")
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let req_span: fn(&http::Request<_>) -> tracing::Span = |req| {
        let span = tracing::info_span!(
            "request",
            req.method = ?req.method(),
            req.uri = ?req.uri(),
            req.version = ?req.version(),
            headers = ?req.headers()
        );
        {
            // TODO: this is a workaround because tracing_subscriber::fmt::Layer doesn't honor
            // overridden request parents.
            let _enter = span.enter();
            tracing::info!(parent: &span, "sending request");
        }
        span
    };

    let svc = Client::new();
    let mut svc = svc.trace_requests(req_span);
    let uri = Uri::from_static("http://httpbin.org");

    let body = Body::empty();
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(body)
        .expect("Unable to build request; this is a bug.");

    let res = svc.call(req).await?;
    info!(message = "got a response", res = ?res.headers());

    Ok(())
}
