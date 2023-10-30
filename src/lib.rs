use std::{sync::Arc, time::Instant};

#[cfg(feature = "actix")]
use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{Method, StatusCode},
    web::Data,
    Error,
};
#[cfg(feature = "actix")]
use actix_web_lab::middleware::{from_fn, Next};

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder,
};

const DEFAULT_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0,
];

pub struct HttpMetricsCollectorBuilder {
    registry: Registry,
    endpoint: Option<String>,
    buckets: Vec<f64>,
}

impl HttpMetricsCollectorBuilder {
    pub fn new() -> Self {
        Self {
            endpoint: None,
            buckets: DEFAULT_BUCKETS.to_vec(),
            registry: Registry::new(),
        }
    }

    pub fn registry(mut self, registry: Registry) -> Self {
        self.registry = registry;
        self
    }

    pub fn buckets(mut self, buckets: &[f64]) -> Self {
        self.buckets = buckets.to_vec();
        self
    }

    pub fn endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.to_string());
        self
    }

    pub fn build(self) -> HttpMetricsCollector {
        let http_requests_total_opts =
            Opts::new("http_requests_total", "Total number of HTTP requests");

        let label_names = ["method", "handler", "code"];

        let http_requests_total =
            IntCounterVec::new(http_requests_total_opts, &label_names).unwrap();

        let http_requests_duration_seconds_opts = HistogramOpts::new(
            "http_requests_duration_seconds",
            "HTTP request duration in seconds for all requests",
        )
        .buckets(self.buckets);

        let http_requests_duration_seconds =
            HistogramVec::new(http_requests_duration_seconds_opts, &label_names).unwrap();

        self.registry
            .register(Box::new(http_requests_total.clone()))
            .unwrap();
        self.registry
            .register(Box::new(http_requests_duration_seconds.clone()))
            .unwrap();

        HttpMetricsCollector {
            registry: self.registry,
            http_requests_duration_seconds,
            http_requests_total,
            endpoint: self.endpoint.unwrap_or("/metrics".to_string()),
        }
    }
}

impl Default for HttpMetricsCollectorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct HttpMetricsCollector {
    registry: Registry,
    http_requests_total: IntCounterVec,
    http_requests_duration_seconds: HistogramVec,
    endpoint: String,
}

impl HttpMetricsCollector {
    pub fn update_metrics(
        &self,
        method: &Method,
        handler: &str,
        code: StatusCode,
        timestamp: Instant,
    ) {
        let label_values = [method.as_str(), handler, code.as_str()];

        let elapsed = timestamp.elapsed();
        let duration =
            (elapsed.as_secs() as f64) + f64::from(elapsed.subsec_nanos()) / 1_000_000_000_f64;

        self.http_requests_duration_seconds
            .with_label_values(&label_values)
            .observe(duration);

        self.http_requests_total
            .with_label_values(&label_values)
            .inc();
    }

    pub fn collect(&self) -> Result<String, String> {
        let encoder = TextEncoder::new();
        let mut buffer = vec![];

        if let Err(err) = encoder.encode(&self.registry.gather(), &mut buffer) {
            return Err(err.to_string());
        }

        match String::from_utf8(buffer) {
            Ok(metrics) => Ok(metrics),
            Err(_) => Err("Metrics corrupted".to_string()),
        }
    }

    pub fn is_endpoint(&self, path: &str, method: &Method) -> bool {
        path == self.endpoint && method == Method::GET
    }
}

struct MetricLog {
    collector: Arc<HttpMetricsCollector>,
    handler: String,
    method: Method,
    code: StatusCode,
    timestamp: Instant,
}

impl Drop for MetricLog {
    fn drop(&mut self) {
        self.collector
            .update_metrics(&self.method, &self.handler, self.code, self.timestamp)
    }
}

#[cfg(feature = "actix")]
pub fn metrics<S, B>() -> impl Transform<
    S,
    ServiceRequest,
    Response = ServiceResponse<impl MessageBody>,
    Error = Error,
    InitError = (),
>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
{
    from_fn(move |req: ServiceRequest, next: Next<B>| {
        let timestamp = Instant::now();

        let method = req.method().clone();
        let collector = req
            .app_data::<Data<HttpMetricsCollector>>()
            .unwrap()
            .clone();

        let handler = {
            let path = req
                .match_pattern()
                .unwrap_or_else(|| req.path().to_string());

            if req.resource_map().has_resource(&path) {
                path
            } else {
                "*".to_string() // 404
            }
        };

        async move {
            let mut log = MetricLog {
                collector: collector.clone().into_inner(),
                method,
                timestamp,
                code: StatusCode::OK,
                handler,
            };

            if collector.is_endpoint(req.path(), req.method()) {
                Ok(req
                    .into_response(collector.collect().unwrap())
                    .map_into_right_body())
            } else {
                match next.call(req).await {
                    Ok(res) => {
                        let status = res.status();
                        log.code = status;
                        Ok(res.map_into_left_body())
                    }
                    Err(err) => {
                        let status = err.error_response().status();
                        log.code = status;
                        Err(err)
                    }
                }
            }
        }
    })
}
