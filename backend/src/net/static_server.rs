use std::fs::Metadata;
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_compression::tokio::bufread::BrotliEncoder;
use async_compression::tokio::bufread::GzipEncoder;
use color_eyre::eyre::eyre;
use digest::Digest;
use futures::FutureExt;
use http::header::ACCEPT_ENCODING;
use http::header::CONTENT_ENCODING;
use http::request::Parts as RequestParts;
use http::response::Builder;
use hyper::{Body, Method, Request, Response, StatusCode};
use openssl::hash::MessageDigest;
use openssl::x509::X509;
use rpc_toolkit::rpc_handler;
use tokio::fs::File;
use tokio::io::BufReader;
use tokio_util::io::ReaderStream;

use crate::context::{DiagnosticContext, InstallContext, RpcContext, SetupContext};
use crate::core::rpc_continuations::RequestGuid;
use crate::db::subscribe;
use crate::install::PKG_PUBLIC_DIR;
use crate::middleware::auth::{auth as auth_middleware, HasValidSession};
use crate::middleware::cors::cors;
use crate::middleware::db::db as db_middleware;
use crate::middleware::diagnostic::diagnostic as diagnostic_middleware;
use crate::net::HttpHandler;
use crate::{diagnostic_api, install_api, main_api, setup_api, Error, ErrorKind, ResultExt};

static NOT_FOUND: &[u8] = b"Not Found";
static METHOD_NOT_ALLOWED: &[u8] = b"Method Not Allowed";
static NOT_AUTHORIZED: &[u8] = b"Not Authorized";

pub const MAIN_UI_WWW_DIR: &str = "/var/www/html/main";
pub const SETUP_UI_WWW_DIR: &str = "/var/www/html/setup";
pub const DIAG_UI_WWW_DIR: &str = "/var/www/html/diagnostic";
pub const INSTALL_UI_WWW_DIR: &str = "/var/www/html/install";

fn status_fn(_: i32) -> StatusCode {
    StatusCode::OK
}

#[derive(Clone)]
pub enum UiMode {
    Setup,
    Diag,
    Install,
    Main,
}

pub async fn setup_ui_file_router(ctx: SetupContext) -> Result<HttpHandler, Error> {
    let handler: HttpHandler = Arc::new(move |req| {
        let ctx = ctx.clone();

        let ui_mode = UiMode::Setup;
        async move {
            let res = match req.uri().path() {
                path if path.starts_with("/rpc/") => {
                    let rpc_handler = rpc_handler!({
                        command: setup_api,
                        context: ctx,
                        status: status_fn,
                        middleware: [
                            cors,
                        ]
                    });

                    rpc_handler(req)
                        .await
                        .map_err(|err| Error::new(eyre!("{}", err), crate::ErrorKind::Network))
                }
                _ => alt_ui(req, ui_mode).await,
            };

            match res {
                Ok(data) => Ok(data),
                Err(err) => Ok(server_error(err)),
            }
        }
        .boxed()
    });

    Ok(handler)
}

pub async fn diag_ui_file_router(ctx: DiagnosticContext) -> Result<HttpHandler, Error> {
    let handler: HttpHandler = Arc::new(move |req| {
        let ctx = ctx.clone();
        let ui_mode = UiMode::Diag;
        async move {
            let res = match req.uri().path() {
                path if path.starts_with("/rpc/") => {
                    let rpc_handler = rpc_handler!({
                        command: diagnostic_api,
                        context: ctx,
                        status: status_fn,
                        middleware: [
                            cors,
                            diagnostic_middleware,
                        ]
                    });

                    rpc_handler(req)
                        .await
                        .map_err(|err| Error::new(eyre!("{}", err), crate::ErrorKind::Network))
                }
                _ => alt_ui(req, ui_mode).await,
            };

            match res {
                Ok(data) => Ok(data),
                Err(err) => Ok(server_error(err)),
            }
        }
        .boxed()
    });

    Ok(handler)
}

pub async fn install_ui_file_router(ctx: InstallContext) -> Result<HttpHandler, Error> {
    let handler: HttpHandler = Arc::new(move |req| {
        let ctx = ctx.clone();
        let ui_mode = UiMode::Install;
        async move {
            let res = match req.uri().path() {
                path if path.starts_with("/rpc/") => {
                    let rpc_handler = rpc_handler!({
                        command: install_api,
                        context: ctx,
                        status: status_fn,
                        middleware: [
                            cors,
                        ]
                    });

                    rpc_handler(req)
                        .await
                        .map_err(|err| Error::new(eyre!("{}", err), crate::ErrorKind::Network))
                }
                _ => alt_ui(req, ui_mode).await,
            };

            match res {
                Ok(data) => Ok(data),
                Err(err) => Ok(server_error(err)),
            }
        }
        .boxed()
    });

    Ok(handler)
}

pub async fn main_ui_server_router(ctx: RpcContext) -> Result<HttpHandler, Error> {
    let handler: HttpHandler = Arc::new(move |req| {
        let ctx = ctx.clone();

        async move {
            let res = match req.uri().path() {
                path if path.starts_with("/rpc/") => {
                    let auth_middleware = auth_middleware(ctx.clone());
                    let db_middleware = db_middleware(ctx.clone());
                    let rpc_handler = rpc_handler!({
                        command: main_api,
                        context: ctx,
                        status: status_fn,
                        middleware: [
                            cors,
                            auth_middleware,
                            db_middleware,
                        ]
                    });

                    rpc_handler(req)
                        .await
                        .map_err(|err| Error::new(eyre!("{}", err), crate::ErrorKind::Network))
                }
                "/ws/db" => subscribe(ctx, req).await,
                path if path.starts_with("/ws/rpc/") => {
                    match RequestGuid::from(path.strip_prefix("/ws/rpc/").unwrap()) {
                        None => {
                            tracing::debug!("No Guid Path");
                            Ok::<_, Error>(bad_request())
                        }
                        Some(guid) => match ctx.get_ws_continuation_handler(&guid).await {
                            Some(cont) => match cont(req).await {
                                Ok::<_, Error>(r) => Ok::<_, Error>(r),
                                Err(err) => Ok::<_, Error>(server_error(err)),
                            },
                            _ => Ok::<_, Error>(not_found()),
                        },
                    }
                }
                path if path.starts_with("/rest/rpc/") => {
                    match RequestGuid::from(path.strip_prefix("/rest/rpc/").unwrap()) {
                        None => {
                            tracing::debug!("No Guid Path");
                            Ok::<_, Error>(bad_request())
                        }
                        Some(guid) => match ctx.get_rest_continuation_handler(&guid).await {
                            None => Ok::<_, Error>(not_found()),
                            Some(cont) => match cont(req).await {
                                Ok::<_, Error>(r) => Ok::<_, Error>(r),
                                Err(e) => Ok::<_, Error>(server_error(e)),
                            },
                        },
                    }
                }
                _ => main_embassy_ui(req, ctx).await,
            };

            match res {
                Ok(data) => Ok(data),
                Err(err) => Ok(server_error(err)),
            }
        }
        .boxed()
    });

    Ok(handler)
}

async fn alt_ui(req: Request<Body>, ui_mode: UiMode) -> Result<Response<Body>, Error> {
    let selected_root_dir = match ui_mode {
        UiMode::Setup => SETUP_UI_WWW_DIR,
        UiMode::Diag => DIAG_UI_WWW_DIR,
        UiMode::Install => INSTALL_UI_WWW_DIR,
        UiMode::Main => MAIN_UI_WWW_DIR,
    };

    let (request_parts, _body) = req.into_parts();
    let accept_encoding = request_parts
        .headers
        .get_all(ACCEPT_ENCODING)
        .into_iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(","))
        .filter_map(|s| s.split(";").next())
        .map(|s| s.trim())
        .collect::<Vec<_>>();
    match &request_parts.method {
        &Method::GET => {
            let uri_path = request_parts
                .uri
                .path()
                .strip_prefix('/')
                .unwrap_or(request_parts.uri.path());

            let full_path = Path::new(selected_root_dir).join(uri_path);
            file_send(
                &request_parts,
                if tokio::fs::metadata(&full_path)
                    .await
                    .ok()
                    .map(|f| f.is_file())
                    .unwrap_or(false)
                {
                    full_path
                } else {
                    Path::new(selected_root_dir).join("index.html")
                },
                &accept_encoding,
            )
            .await
        }
        _ => Ok(method_not_allowed()),
    }
}

async fn main_embassy_ui(req: Request<Body>, ctx: RpcContext) -> Result<Response<Body>, Error> {
    let selected_root_dir = MAIN_UI_WWW_DIR;

    let (request_parts, _body) = req.into_parts();
    let accept_encoding = request_parts
        .headers
        .get_all(ACCEPT_ENCODING)
        .into_iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(","))
        .filter_map(|s| s.split(";").next())
        .map(|s| s.trim())
        .collect::<Vec<_>>();
    match (
        &request_parts.method,
        request_parts
            .uri
            .path()
            .strip_prefix('/')
            .unwrap_or(request_parts.uri.path())
            .split_once('/'),
    ) {
        (&Method::GET, Some(("public", path))) => {
            match HasValidSession::from_request_parts(&request_parts, &ctx).await {
                Ok(_) => {
                    let sub_path = Path::new(path);
                    if let Ok(rest) = sub_path.strip_prefix("package-data") {
                        file_send(
                            &request_parts,
                            ctx.datadir.join(PKG_PUBLIC_DIR).join(rest),
                            &accept_encoding,
                        )
                        .await
                    } else if let Ok(rest) = sub_path.strip_prefix("eos") {
                        match rest.to_str() {
                            Some("local.crt") => cert_send(&ctx.account.read().await.root_ca_cert),
                            None => Ok(bad_request()),
                            _ => Ok(not_found()),
                        }
                    } else {
                        Ok(not_found())
                    }
                }
                Err(e) => un_authorized(e, &format!("public/{path}")),
            }
        }
        (&Method::GET, Some(("eos", "local.crt"))) => {
            match HasValidSession::from_request_parts(&request_parts, &ctx).await {
                Ok(_) => cert_send(&ctx.account.read().await.root_ca_cert),
                Err(e) => un_authorized(e, "eos/local.crt"),
            }
        }
        (&Method::GET, _) => {
            let uri_path = request_parts
                .uri
                .path()
                .strip_prefix('/')
                .unwrap_or(request_parts.uri.path());

            let full_path = Path::new(selected_root_dir).join(uri_path);
            file_send(
                &request_parts,
                if tokio::fs::metadata(&full_path)
                    .await
                    .ok()
                    .map(|f| f.is_file())
                    .unwrap_or(false)
                {
                    full_path
                } else {
                    Path::new(selected_root_dir).join("index.html")
                },
                &accept_encoding,
            )
            .await
        }
        _ => Ok(method_not_allowed()),
    }
}

fn un_authorized(err: Error, path: &str) -> Result<Response<Body>, Error> {
    tracing::warn!("unauthorized for {} @{:?}", err, path);
    tracing::debug!("{:?}", err);
    Ok(Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(NOT_AUTHORIZED.into())
        .unwrap())
}

/// HTTP status code 404
fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(NOT_FOUND.into())
        .unwrap()
}

/// HTTP status code 405
fn method_not_allowed() -> Response<Body> {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body(METHOD_NOT_ALLOWED.into())
        .unwrap()
}

fn server_error(err: Error) -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(err.to_string().into())
        .unwrap()
}

fn bad_request() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::empty())
        .unwrap()
}

fn cert_send(cert: &X509) -> Result<Response<Body>, Error> {
    let pem = cert.to_pem()?;
    Response::builder()
        .status(StatusCode::OK)
        .header(
            http::header::ETAG,
            base32::encode(
                base32::Alphabet::RFC4648 { padding: false },
                &*cert.digest(MessageDigest::sha256())?,
            )
            .to_lowercase(),
        )
        .header(http::header::CONTENT_TYPE, "application/x-pem-file")
        .header(http::header::CONTENT_LENGTH, pem.len())
        .body(Body::from(pem))
        .with_kind(ErrorKind::Network)
}

async fn file_send(
    req: &RequestParts,
    path: impl AsRef<Path>,
    accept_encoding: &[&str],
) -> Result<Response<Body>, Error> {
    // Serve a file by asynchronously reading it by chunks using tokio-util crate.

    let path = path.as_ref();

    let file = File::open(path)
        .await
        .with_ctx(|_| (ErrorKind::Filesystem, path.display().to_string()))?;
    let metadata = file
        .metadata()
        .await
        .with_ctx(|_| (ErrorKind::Filesystem, path.display().to_string()))?;

    let e_tag = e_tag(path, &metadata)?;

    let mut builder = Response::builder();
    builder = with_content_type(path, builder);
    builder = builder.header(http::header::ETAG, &e_tag);
    builder = builder.header(
        http::header::CACHE_CONTROL,
        "public, max-age=21000000, immutable",
    );

    if req
        .headers
        .get_all(http::header::CONNECTION)
        .iter()
        .flat_map(|s| s.to_str().ok())
        .flat_map(|s| s.split(","))
        .any(|s| s.trim() == "keep-alive")
    {
        builder = builder.header(http::header::CONNECTION, "keep-alive");
    }

    if req
        .headers
        .get("if-none-match")
        .and_then(|h| h.to_str().ok())
        == Some(e_tag.as_str())
    {
        builder = builder.status(StatusCode::NOT_MODIFIED);
        builder.body(Body::empty())
    } else {
        let body = if false && accept_encoding.contains(&"br") && metadata.len() > u16::MAX as u64 {
            builder = builder.header(CONTENT_ENCODING, "br");
            Body::wrap_stream(ReaderStream::new(BrotliEncoder::new(BufReader::new(file))))
        } else if accept_encoding.contains(&"gzip") && metadata.len() > u16::MAX as u64 {
            builder = builder.header(CONTENT_ENCODING, "gzip");
            Body::wrap_stream(ReaderStream::new(GzipEncoder::new(BufReader::new(file))))
        } else {
            builder = with_content_length(&metadata, builder);
            Body::wrap_stream(ReaderStream::new(file))
        };
        builder.body(body)
    }
    .with_kind(ErrorKind::Network)
}

fn e_tag(path: &Path, metadata: &Metadata) -> Result<String, Error> {
    let modified = metadata.modified().with_kind(ErrorKind::Filesystem)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(format!("{:?}", path).as_bytes());
    hasher.update(
        format!(
            "{}",
            modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        )
        .as_bytes(),
    );
    let res = hasher.finalize();
    Ok(format!(
        "\"{}\"",
        base32::encode(base32::Alphabet::RFC4648 { padding: false }, res.as_slice()).to_lowercase()
    ))
}

///https://en.wikipedia.org/wiki/Media_type
fn with_content_type(path: &Path, builder: Builder) -> Builder {
    let content_type = match path.extension() {
        Some(os_str) => match os_str.to_str() {
            Some("apng") => "image/apng",
            Some("avif") => "image/avif",
            Some("flif") => "image/flif",
            Some("gif") => "image/gif",
            Some("jpg") | Some("jpeg") | Some("jfif") | Some("pjpeg") | Some("pjp") => "image/jpeg",
            Some("jxl") => "image/jxl",
            Some("png") => "image/png",
            Some("svg") => "image/svg+xml",
            Some("webp") => "image/webp",
            Some("mng") | Some("x-mng") => "image/x-mng",
            Some("css") => "text/css",
            Some("csv") => "text/csv",
            Some("html") => "text/html",
            Some("php") => "text/php",
            Some("plain") | Some("md") | Some("txt") => "text/plain",
            Some("xml") => "text/xml",
            Some("js") => "text/javascript",
            Some("wasm") => "application/wasm",
            None | Some(_) => "text/plain",
        },
        None => "text/plain",
    };
    builder.header(http::header::CONTENT_TYPE, content_type)
}

fn with_content_length(metadata: &Metadata, builder: Builder) -> Builder {
    builder.header(http::header::CONTENT_LENGTH, metadata.len())
}
