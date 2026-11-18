pub mod fleet {
    tonic::include_proto!("agentdesktop.fleet.v1");
}

/// OIDC `nonce` that binds a browser sign-in to the device key being enrolled:
/// base64url (no padding) of the SHA-256 of the key's DER SubjectPublicKeyInfo.
///
/// The agent sends it in the authorization request, and the controller requires
/// the ID token presented to `EnrollDevice` to carry the value computed from the
/// CSR's key. A token obtained without signing in for that key, such as one from
/// a refresh grant, therefore cannot enroll a device.
pub fn enrollment_nonce(subject_public_key_info_der: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest as _, Sha256};

    URL_SAFE_NO_PAD.encode(Sha256::digest(subject_public_key_info_der))
}

/// OTLP logs service subset used to receive agentgateway access logs.
pub mod otlp_logs {
    tonic::include_proto!("opentelemetry.proto.collector.logs.v1");
}
