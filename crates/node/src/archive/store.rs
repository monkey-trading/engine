//! The archive's bucket: create-only uploads signed for its S3 API, and plain reads from its public
//! origin, as any reader of the archive makes them, each of a key its caller owns: a plain name under the key prefix
//! the store was made for, so no name taken from the host leads a request out of it.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, IF_NONE_MATCH};
use reqwest::{Body, Client, RequestBuilder, StatusCode, Url, redirect};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};

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

/// An object's length and SHA-256 as the bucket's public origin serves it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Digested {
    /// None stands at the key.
    Absent,
    /// The object's length and SHA-256.
    Read { bytes: u64, sha256: [u8; 32] },
    /// The object is longer than the reader's bound, and was not read past it.
    Oversized,
}

/// What a read of an object handed on.
enum Streamed {
    Absent,
    /// The whole object, this many bytes.
    Whole(u64),
    Oversized,
}

/// The bucket the archive uploads to and reads back from, under one key prefix.
pub(crate) struct Store {
    http: Client,
    bucket: Bucket,
    credentials: Credentials,
    public: Url,
    /// The key prefix of the objects the store reads and writes: its caller's.
    prefix: String,
}

impl Store {
    /// The bucket `section` names, written with the delivered `key_id` and `secret`, over `http`, at keys under
    /// `prefix` alone.
    /// Error cases:
    /// - the endpoint and bucket do not form a bucket URL
    pub(crate) fn new(
        section: &ArchiveSection,
        (key_id, secret): (&SecretValue, &SecretValue),
        http: Client,
        prefix: String,
    ) -> anyhow::Result<Self> {
        let (name, region) = (section.bucket.clone(), section.region.clone());
        let bucket = Bucket::new(section.endpoint.clone(), UrlStyle::Path, name, region).context("archive.bucket")?;
        let credentials = Credentials::new(key_id.expose(), secret.expose());
        Ok(Self { http, bucket, credentials, public: section.public_url.clone(), prefix })
    }

    /// The key prefix of the objects the store reads and writes.
    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The same bucket under `prefix`, a directory below this store's own: its prefix followed by one or more plain
    /// names (`plain`), each ending in `/`. A caller narrows the keys it owns this way, and never widens them.
    /// Error cases:
    /// - `prefix` is not such a directory below the store's prefix
    pub(crate) fn within(&self, prefix: &str) -> anyhow::Result<Self> {
        let below = prefix.strip_prefix(self.prefix.as_str()).and_then(|below| below.strip_suffix('/'));
        ensure!(
            below.is_some_and(|below| below.split('/').all(plain)),
            "{prefix:?} is not a directory under {}",
            self.prefix
        );
        let (http, bucket, credentials) = (self.http.clone(), self.bucket.clone(), self.credentials.clone());
        Ok(Self { http, bucket, credentials, public: self.public.clone(), prefix: prefix.to_owned() })
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
    /// - the key is not the store's (`owned`), before any request
    /// - the request fails or times out, or the bucket answers anything else
    pub(crate) async fn create(&self, key: &str, body: Vec<u8>, content_type: &'static str) -> anyhow::Result<Created> {
        let length = body.len() as u64;
        self.create_from(key, (Body::from(body), length), content_type, REQUEST_TIMEOUT).await
    }

    /// `create` of the `length` bytes `body` streams, within `timeout`, body included: a body that ends in an error
    /// before its last byte creates nothing.
    /// Error cases:
    /// - as `create`, or the body fails
    pub(crate) async fn create_from(
        &self,
        key: &str,
        (body, length): (Body, u64),
        content_type: &'static str,
        timeout: Duration,
    ) -> anyhow::Result<Created> {
        self.owned(key)?;
        let mut action = self.bucket.put_object(Some(&self.credentials), key);
        action.headers_mut().insert("content-type", content_type);
        action.headers_mut().insert("if-none-match", "*");
        // The signed URL is a credential for its lifetime, so no error carries it.
        let response = self
            .http
            .put(action.sign(SIGNED_FOR))
            .timeout(timeout)
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
    /// - the key is not the store's (`owned`), before any request
    /// - see `read_public`
    pub(crate) async fn read(&self, key: &str, limit: u64) -> anyhow::Result<Object> {
        read_public(&self.http, self.public_object(key)?, limit, REQUEST_TIMEOUT).await
    }

    /// The length and SHA-256 of the object `key` as the bucket's public origin serves it, read up to `limit` bytes
    /// within `timeout`, body included, and held a chunk at a time.
    /// Error cases:
    /// - the key is not the store's (`owned`), before any request
    /// - as `read_public`, or the read takes longer than `timeout`
    pub(crate) async fn digest(&self, key: &str, limit: u64, timeout: Duration) -> anyhow::Result<Digested> {
        let url = self.public_object(key)?;
        let mut hash = Sha256::new();
        let read = stream_public(self.http.get(url.clone()).timeout(timeout), &url, limit, REQUEST_TIMEOUT, |chunk| {
            hash.update(chunk);
        });
        Ok(match read.await? {
            Streamed::Absent => Digested::Absent,
            Streamed::Oversized => Digested::Oversized,
            Streamed::Whole(bytes) => Digested::Read { bytes, sha256: hash.finalize().into() },
        })
    }

    /// Whether the bucket's public origin serves an object at `key`.
    /// Error cases:
    /// - the key is not the store's (`owned`), before any request
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

    /// The log under key `prefix`, read over the public origin with this store's client.
    /// Error cases:
    /// - the prefix does not join the origin
    pub(crate) fn reader(&self, prefix: &str) -> anyhow::Result<(Client, crate::archive::Bucket)> {
        Ok((self.http.clone(), crate::archive::Bucket::new(&self.public, prefix)?))
    }

    /// The public URL of the object at `key`.
    /// Error cases:
    /// - the key is not the store's (`owned`), or does not form a URL with the origin
    fn public_object(&self, key: &str) -> anyhow::Result<Url> {
        self.owned(key)?;
        self.public.join(key).with_context(|| format!("the public URL of {key}"))
    }

    /// Requires `key` to be a plain name (`plain`) directly under the store's prefix, so no key climbs out of the
    /// prefix, reaches below it or names another's.
    /// Error cases:
    /// - the key is not such a name under the prefix
    fn owned(&self, key: &str) -> anyhow::Result<()> {
        let name = key.strip_prefix(self.prefix.as_str()).unwrap_or_default();
        ensure!(plain(name), "{key:?} is not an object name under {}", self.prefix);
        Ok(())
    }
}

/// Whether `name` is one plain path segment: ASCII letters, digits, `-`, `_` and `.`, not empty and not starting with
/// `.`, so neither `.` nor `..`.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

/// The object at `url`, a key at a bucket's public origin, read over `http` up to `limit` bytes, as every reader of
/// the archive reads it, its answer's headers within `answered`.
/// Error cases:
/// - the request fails or times out, the headers take longer than `answered`, or the origin answers other than
///   `200` or `404`
pub(crate) async fn read_public(http: &Client, url: Url, limit: u64, answered: Duration) -> anyhow::Result<Object> {
    let mut body = Vec::new();
    let read = stream_public(http.get(url.clone()), &url, limit, answered, |chunk| body.extend_from_slice(chunk));
    Ok(match read.await? {
        Streamed::Absent => Object::Absent,
        Streamed::Oversized => Object::Oversized,
        Streamed::Whole(_) => Object::Read(body),
    })
}

/// Sends `request`, a read of `url` at a bucket's public origin, and hands each chunk of the object to `take`, up to
/// `limit` bytes, its answer's headers within `answered`.
/// Error cases:
/// - as `read_public`
async fn stream_public(
    request: RequestBuilder,
    url: &Url,
    limit: u64,
    answered: Duration,
    mut take: impl FnMut(&[u8]),
) -> anyhow::Result<Streamed> {
    let sent = tokio::time::timeout(answered, request.send()).await;
    let response = sent.map_err(|_| anyhow::anyhow!("{url} did not answer within {answered:?}"))?;
    let response = response.with_context(|| format!("reading {url}"))?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => return Ok(Streamed::Absent),
        status => bail!("reading {url}: the origin answered {status} {}", error_code(response).await),
    }
    if response.content_length().is_some_and(|length| length > limit) {
        return Ok(Streamed::Oversized);
    }
    let (mut read, mut stream) = (0u64, response.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        read += chunk.len() as u64;
        if read > limit {
            return Ok(Streamed::Oversized);
        }
        take(&chunk);
    }
    Ok(Streamed::Whole(read))
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
