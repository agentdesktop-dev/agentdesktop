pub mod fleet {
    tonic::include_proto!("agentdesktop.fleet.v1");
}

/// OTLP logs service subset used to receive agentgateway access logs.
pub mod otlp_logs {
    tonic::include_proto!("opentelemetry.proto.collector.logs.v1");
}
