//! The archive's bucket: create-only uploads signed for its S3 API, and plain reads from its public
//! origin, as any reader of the archive makes them.

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, IF_NONE_MATCH};
use reqwest::{Client, StatusCode, Url, redirect};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use crate::config::{ArchiveSection, SecretValue};

/// How long one request to the bucket may take, its body included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a signed upload's URL stays valid. It is signed at the host's clock: a clock the bucket finds
/// skewed only fails uploads, which `archive_upload_lagging` reports.
const SIGNED_FOR: Duration = Duration::from_secs(300);
/// The most bytes of an error answer read for its code.
const ERROR_BYTES: usize = 4096;

/// What a create-only upload found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Created {
    /// The bucket took the object.
    New,
    /// An object already stood at the key, and was left as it was.
    Existing,
}

/// An object as the bucket's public origin serves it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Object {
    /// None stands at the key.
    Absent,
    /// The object's bytes.
    Read(Vec<u8>),
    /// The object is longer than the reader's bound, and was not read past it.
    Oversized,
}

/// The bucket the archive uploads to and reads back from.
pub(crate) struct Store {
    http: Client,
    bucket: Bucket,
    credentials: Credentials,
    public: Url,
}

impl Store {
    /// The bucket `section` names, written with the delivered `key_id` and `secret`, over `http`.
    /// Error cases:
    /// - the endpoint and bucket do not form a bucket URL
    pub(crate) fn new(
        section: &ArchiveSection,
        (key_id, secret): (&SecretValue, &SecretValue),
        http: Client,
    ) -> anyhow::Result<Self> {
        let (name, region) = (section.bucket.clone(), section.region.clone());
        let bucket = Bucket::new(section.endpoint.clone(), UrlStyle::Path, name, region).context("archive.bucket")?;
        let credentials = Credentials::new(key_id.expose(), secret.expose());
        Ok(Self { http, bucket, credentials, public: section.public_url.clone() })
    }

    /// The client for a bucket's requests: TLS to the public roots alone, no redirect and no proxy, so a
    /// request reaches only the origin the attested config names.
    /// Error cases:
    /// - the roots or the client do not build
    pub(crate) fn client() -> anyhow::Result<Client> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| reqwest::Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .context("loading the archive's TLS roots")?;
        Client::builder()
            .user_agent(concat!("engine-archive/", env!("CARGO_PKG_VERSION")))
            .tls_certs_only(roots)
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the archive's client")
    }

    /// Creates the object `key` holding `body`, typed `content_type`, unless an object already stands
    /// there. The request is signed over its `If-None-Match: *`, so the bucket creates or refuses it
    /// atomically: `412`, or `409` from a bucket lock, is an object already there.
    /// Error cases:
    /// - the request fails or times out, or the bucket answers anything else
    pub(crate) async fn create(&self, key: &str, body: Vec<u8>, content_type: &'static str) -> anyhow::Result<Created> {
        let mut action = self.bucket.put_object(Some(&self.credentials), key);
        action.headers_mut().insert("content-type", content_type);
        action.headers_mut().insert("if-none-match", "*");
        let length = body.len();
        // The signed URL is a credential for its lifetime, so no error carries it.
        let response = self
            .http
            .put(action.sign(SIGNED_FOR))
            .header(CONTENT_TYPE, content_type)
            .header(IF_NONE_MATCH, "*")
            .header(CONTENT_LENGTH, length)
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e.without_url()).context(format!("uploading {key}")))?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(Created::New),
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT => Ok(Created::Existing),
            status => bail!("uploading {key}: the bucket answered {status} {}", error_code(response).await),
        }
    }

    /// The object `key` as the bucket's public origin serves it, read up to `limit` bytes.
    /// Error cases:
    /// - the request fails or times out, or the origin answers other than `200` or `404`
    pub(crate) async fn read(&self, key: &str, limit: u64) -> anyhow::Result<Object> {
        let url = self.public_object(key)?;
        let response = self.http.get(url.clone()).send().await.with_context(|| format!("reading {url}"))?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Ok(Object::Absent),
            status => bail!("reading {url}: the origin answered {status} {}", error_code(response).await),
        }
        if response.content_length().is_some_and(|length| length > limit) {
            return Ok(Object::Oversized);
        }
        let (mut body, mut stream) = (Vec::new(), response.bytes_stream());
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("reading {url}"))?;
            if body.len() as u64 + chunk.len() as u64 > limit {
                return Ok(Object::Oversized);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Object::Read(body))
    }

    /// Whether the bucket's public origin serves an object at `key`.
    /// Error cases:
    /// - the request fails or times out, or the origin answers other than `200` or `404`
    pub(crate) async fn holds(&self, key: &str) -> anyhow::Result<bool> {
        let url = self.public_object(key)?;
        let response = self.http.head(url.clone()).send().await.with_context(|| format!("asking for {url}"))?;
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status => bail!("asking for {url}: the origin answered {status}"),
        }
    }

    /// The public URL of the object at `key`.
    /// Error cases:
    /// - the key does not form a URL with the origin
    fn public_object(&self, key: &str) -> anyhow::Result<Url> {
        self.public.join(key).with_context(|| format!("the public URL of {key}"))
    }
}

/// The S3 error code an error answer names, for the log, as `(<code>)`, read from at most `ERROR_BYTES` of its body.
/// Nothing else of the body is kept: an S3 error can echo the signed request, the access key id included.
async fn error_code(response: reqwest::Response) -> String {
    let (mut body, mut stream) = (Vec::new(), response.bytes_stream());
    while body.len() < ERROR_BYTES {
        match stream.next().await {
            Some(Ok(chunk)) => body.extend_from_slice(&chunk[..chunk.len().min(ERROR_BYTES - body.len())]),
            Some(Err(_)) | None => break,
        }
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let code = text.split_once("<Code>").and_then(|(_, rest)| rest.split_once("</Code>")).map(|(code, _)| code);
    match code {
        Some(code) if code.bytes().all(|b| b.is_ascii_alphanumeric()) => format!("({code})"),
        _ => "(no error code)".to_owned(),
    }
}
