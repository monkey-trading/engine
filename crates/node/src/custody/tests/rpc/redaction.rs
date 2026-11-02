//! What a provider's answer can put in an error.

use super::*;

// A provider's refusal can echo the key alone, which matches neither the URL nor its path.
#[tokio::test]
async fn rpc_errors_never_carry_a_key_a_provider_echoes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = "invalid api key: FAKE-echoed-key";
    let answer = format!("HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\n\r\n{body}", body.len());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        stream.write_all(answer.as_bytes()).await.unwrap();
    });
    let rpc = rpc_at(&format!("http://{address}/v2/FAKE-echoed-key"));

    let err = format!("{:#}", rpc.finality().await.unwrap_err());
    assert!(err.contains("401"), "the provider's answer did not reach the error: {err}");
    assert!(!err.contains("FAKE-echoed-key"), "{err}");
}
