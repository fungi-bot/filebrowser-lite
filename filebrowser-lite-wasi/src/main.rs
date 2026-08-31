use bytes::Bytes;
use http::response::Builder;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use percent_encoding::percent_decode_str;
use rust_embed::Embed;
use serde::Serialize;
use std::convert::Infallible;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::net::TcpListener;

const STORAGE_ROOT: &str = "data";
const RESOURCES_PREFIX: &str = "/api/resources";
const RAW_PREFIX: &str = "/api/raw";
const PREVIEW_PREFIX: &str = "/api/preview";

type AppRequest = Request<Vec<u8>>;
type AppResponse = Response<Vec<u8>>;

#[derive(Embed)]
#[folder = "../frontend/dist"]
struct Assets;

#[derive(Serialize)]
struct Sorting {
    by: &'static str,
    asc: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceItem {
    path: String,
    name: String,
    size: u64,
    extension: String,
    modified: String,
    mode: u32,
    is_dir: bool,
    is_symlink: bool,
    #[serde(rename = "type")]
    resource_type: String,
    url: String,
    index: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Resource {
    path: String,
    name: String,
    size: u64,
    extension: String,
    modified: String,
    mode: u32,
    is_dir: bool,
    is_symlink: bool,
    #[serde(rename = "type")]
    resource_type: String,
    url: String,
    index: usize,
    items: Vec<ResourceItem>,
    num_dirs: usize,
    num_files: usize,
    sorting: Sorting,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Serialize)]
struct StatusResponse<'a> {
    status: &'a str,
    message: &'a str,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewSize {
    Thumb,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeError {
    Invalid,
    Unsatisfiable,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = parse_listen_address()?;
    let listener = TcpListener::bind(address).await?;
    eprintln!("filebrowser-lite-wasi listening on {address}");

    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let connection = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service_fn(handle_hyper_request));
            if let Err(error) = connection.await {
                eprintln!("connection from {peer} failed: {error}");
            }
        });
    }
}

fn parse_listen_address() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut address = "127.0.0.1:8082".parse()?;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                address = args
                    .next()
                    .ok_or("missing socket address after --listen")?
                    .parse()?;
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(address)
}

async fn handle_hyper_request(
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = request.into_parts();
    let response = match body.collect().await {
        Ok(body) => match route(Request::from_parts(parts, body.to_bytes().to_vec())).await {
            Ok(response) => response,
            Err(error) => json_error(error),
        },
        Err(error) => json_error(ApiError::new(StatusCode::BAD_REQUEST, error.to_string())),
    };
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(parts, Full::new(Bytes::from(body))))
}

async fn route(mut request: AppRequest) -> Result<AppResponse, ApiError> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    match (method, path.as_str()) {
        (Method::GET, "/config.js") => Ok(config_js_response()),
        (Method::GET, "/api/health") => Ok(json_response(
            StatusCode::OK,
            &StatusResponse {
                status: "ok",
                message: "filebrowser-lite-wasi is running",
            },
        )),
        _ if path_matches_prefix(&path, RESOURCES_PREFIX) => handle_resources(&mut request).await,
        _ if path_matches_prefix(&path, RAW_PREFIX) => handle_raw(&request).await,
        _ if path_matches_prefix(&path, PREVIEW_PREFIX) => handle_preview(&request).await,
        _ => serve_asset_route(&path),
    }
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

fn serve_asset_route(path: &str) -> Result<AppResponse, ApiError> {
    let asset_path = normalize_asset_path(path);
    let fallback = if asset_path == "index.html" {
        None
    } else {
        Some("index.html")
    };

    if let Some(asset) = Assets::get(&asset_path) {
        return Ok(asset_response(&asset_path, asset.data.as_ref()));
    }

    if let Some(fallback_path) = fallback {
        if let Some(asset) = Assets::get(fallback_path) {
            return Ok(asset_response(fallback_path, asset.data.as_ref()));
        }
    }

    Err(ApiError::new(StatusCode::NOT_FOUND, "route not found"))
}

async fn handle_resources(request: &mut AppRequest) -> Result<AppResponse, ApiError> {
    let uri_path = request.uri().path().to_string();
    let resource_path = extract_route_path(&uri_path, RESOURCES_PREFIX);
    let path_info = resolve_storage_path(&resource_path)?;
    let query = request.uri().query().unwrap_or("");
    let method = request.method().clone();
    let dir_request = uri_path.ends_with('/');

    match method {
        Method::GET => {
            let resource = read_resource(&path_info.guest_path, &path_info.host_path)?;
            Ok(json_response(StatusCode::OK, &resource))
        }
        Method::POST => {
            if dir_request {
                fs::create_dir_all(&path_info.host_path)
                    .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
                let resource = read_resource(&path_info.guest_path, &path_info.host_path)?;
                return Ok(json_response(StatusCode::OK, &resource));
            }

            let override_existing = query_flag(query, "override");
            if path_info.host_path.exists() && !override_existing {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "target already exists; pass override=true to replace it",
                ));
            }

            write_request_body(request, &path_info.host_path).await?;
            let resource = read_resource(&path_info.guest_path, &path_info.host_path)?;
            Ok(json_response(StatusCode::OK, &resource))
        }
        Method::PUT => {
            if !path_info.host_path.exists() {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "target file does not exist",
                ));
            }

            if path_info.host_path.is_dir() {
                return Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "PUT only supports files",
                ));
            }

            write_request_body(request, &path_info.host_path).await?;
            let resource = read_resource(&path_info.guest_path, &path_info.host_path)?;
            Ok(json_response(StatusCode::OK, &resource))
        }
        Method::PATCH => handle_patch(query, &path_info).await,
        Method::DELETE => {
            if path_info.guest_path == "/" {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "refusing to delete storage root",
                ));
            }

            delete_path(&path_info.host_path)?;
            Ok(json_response(
                StatusCode::OK,
                &StatusResponse {
                    status: "ok",
                    message: "deleted",
                },
            ))
        }
        _ => Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
        )),
    }
}

async fn handle_patch(query: &str, source: &ResolvedPath) -> Result<AppResponse, ApiError> {
    if source.guest_path == "/" {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "refusing to modify storage root",
        ));
    }

    let action = query_value(query, "action")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "missing action query param"))?;
    let destination_raw = query_value(query, "destination")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "missing destination query param"))?;
    let destination = resolve_storage_path(&destination_raw)?;
    let override_existing = query_flag(query, "override");

    if destination.host_path.exists() && !override_existing {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "destination exists; pass override=true to replace it",
        ));
    }

    if override_existing && destination.host_path.exists() {
        delete_path(&destination.host_path)?;
    }

    match action.as_str() {
        "rename" => rename_path(&source.host_path, &destination.host_path)?,
        "copy" => copy_path(&source.host_path, &destination.host_path)?,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported action; use rename or copy",
            ));
        }
    }

    let resource = read_resource(&destination.guest_path, &destination.host_path)?;
    Ok(json_response(StatusCode::OK, &resource))
}

async fn handle_raw(request: &AppRequest) -> Result<AppResponse, ApiError> {
    if request.method() != Method::GET {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
        ));
    }

    let uri_path = request.uri().path().to_string();
    let resource_path = extract_route_path(&uri_path, RAW_PREFIX);
    let path_info = resolve_storage_path(&resource_path)?;
    raw_file_response(request, &path_info)
}

async fn handle_preview(request: &AppRequest) -> Result<AppResponse, ApiError> {
    if request.method() != Method::GET {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
        ));
    }

    let uri_path = request.uri().path().to_string();
    let preview_path = extract_route_path(&uri_path, PREVIEW_PREFIX);
    let (preview_size, resource_path) = parse_preview_path(&preview_path)?;
    let path_info = resolve_storage_path(&resource_path)?;
    let metadata = file_metadata(&path_info.host_path)?;

    if metadata.is_dir() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "preview only supports files",
        ));
    }

    if should_preview_raw(&path_info.host_path) {
        return raw_file_response(request, &path_info);
    }

    let filename = file_name_from_guest_path(&path_info.guest_path);
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let modified_header = format_http_date(modified);
    let etag = preview_etag(&path_info.guest_path, modified, preview_size);

    if preview_not_modified(request, &etag, &modified_header) {
        return build_response(
            Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header("Cache-Control", "private")
                .header("Last-Modified", modified_header)
                .header("ETag", etag),
            Vec::new(),
        );
    }

    match create_image_preview(&path_info.host_path, preview_size) {
        Ok(bytes) => build_response(
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "image/jpeg")
                .header("Content-Disposition", content_disposition(&filename, true))
                .header("Cache-Control", "private")
                .header("Last-Modified", modified_header)
                .header("ETag", etag),
            bytes,
        ),
        Err(_) => raw_file_response(request, &path_info),
    }
}

fn raw_file_response(
    request: &AppRequest,
    path_info: &ResolvedPath,
) -> Result<AppResponse, ApiError> {
    let metadata = file_metadata(&path_info.host_path)?;

    if metadata.is_dir() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "raw download only supports files",
        ));
    }

    let filename = file_name_from_guest_path(&path_info.guest_path);
    let inline = query_flag(request.uri().query().unwrap_or(""), "inline");
    let content_disposition = content_disposition(&filename, inline);
    let total_len = metadata.len();
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let modified_header = format_http_date(modified);
    let range_header = request
        .headers()
        .get("range")
        .and_then(|value| value.to_str().ok());

    match range_header {
        Some(value) => {
            let range = match parse_range_header(value, total_len) {
                Ok(range) => range,
                Err(RangeError::Invalid) => {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid Range header",
                    ));
                }
                Err(RangeError::Unsatisfiable) => {
                    return Ok(range_not_satisfiable_response(total_len));
                }
            };
            let bytes = read_file_range(&path_info.host_path, range)?;
            let content_range = format!("bytes {}-{}/{}", range.start, range.end, total_len);

            build_response(
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header("Content-Type", content_type_for(&path_info.host_path))
                    .header("Content-Disposition", content_disposition)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Range", content_range)
                    .header("Content-Length", bytes.len().to_string())
                    .header("Cache-Control", "private")
                    .header("Last-Modified", modified_header),
                bytes,
            )
        }
        None => {
            let bytes = fs::read(&path_info.host_path)
                .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;

            build_response(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", content_type_for(&path_info.host_path))
                    .header("Content-Disposition", content_disposition)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", bytes.len().to_string())
                    .header("Cache-Control", "private")
                    .header("Last-Modified", modified_header),
                bytes,
            )
        }
    }
}

fn file_metadata(path: &Path) -> Result<fs::Metadata, ApiError> {
    fs::metadata(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => ApiError::new(StatusCode::NOT_FOUND, "file not found"),
        _ => io_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    })
}

fn read_resource(guest_path: &str, host_path: &Path) -> Result<Resource, ApiError> {
    let metadata = fs::metadata(host_path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => ApiError::new(StatusCode::NOT_FOUND, "path not found"),
        _ => io_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    })?;

    let name = file_name_from_guest_path(guest_path);
    let modified = format_system_time(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));

    if metadata.is_dir() {
        let mut items = Vec::new();
        let mut num_dirs = 0usize;
        let mut num_files = 0usize;

        let entries = fs::read_dir(host_path)
            .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;

        for entry in entries {
            let entry = entry.map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
            let child_name = entry.file_name().to_string_lossy().to_string();
            let child_guest_path = join_guest_path(guest_path, &child_name);
            let child_item = read_resource_item(&child_guest_path, &entry.path())?;
            if child_item.is_dir {
                num_dirs += 1;
            } else {
                num_files += 1;
            }
            items.push(child_item);
        }

        items.sort_by(|left, right| match (left.is_dir, right.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
        });

        for (index, item) in items.iter_mut().enumerate() {
            item.index = index;
        }

        return Ok(Resource {
            path: guest_path.to_string(),
            name,
            size: 0,
            extension: String::new(),
            modified,
            mode: 0,
            is_dir: true,
            is_symlink: false,
            resource_type: "dir".to_string(),
            url: String::new(),
            index: 0,
            items,
            num_dirs,
            num_files,
            sorting: default_sorting(),
            content: None,
        });
    }

    let extension = extension_for_name(&name);
    let resource_type = detect_file_type(&name);
    let content = if is_text_resource(&resource_type) {
        Some(
            fs::read_to_string(host_path)
                .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?,
        )
    } else {
        None
    };

    Ok(Resource {
        path: guest_path.to_string(),
        name: name.clone(),
        size: metadata.len(),
        modified,
        extension,
        mode: 0,
        is_dir: false,
        is_symlink: false,
        resource_type,
        url: String::new(),
        index: 0,
        items: Vec::new(),
        num_dirs: 0,
        num_files: 0,
        sorting: default_sorting(),
        content,
    })
}

fn read_resource_item(guest_path: &str, host_path: &Path) -> Result<ResourceItem, ApiError> {
    let metadata = fs::metadata(host_path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => ApiError::new(StatusCode::NOT_FOUND, "path not found"),
        _ => io_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    })?;

    let name = file_name_from_guest_path(guest_path);
    let extension = extension_for_name(&name);

    Ok(ResourceItem {
        path: guest_path.to_string(),
        name: name.clone(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        extension,
        modified: format_system_time(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
        mode: 0,
        is_dir: metadata.is_dir(),
        is_symlink: false,
        resource_type: if metadata.is_dir() {
            "dir".to_string()
        } else {
            detect_file_type(&name)
        },
        url: String::new(),
        index: 0,
    })
}

async fn write_request_body(request: &mut AppRequest, host_path: &Path) -> Result<(), ApiError> {
    if let Some(parent) = host_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    }

    let body = std::mem::take(request.body_mut());

    let mut file =
        File::create(host_path).map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    file.write_all(&body)
        .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    file.sync_all()
        .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(())
}

fn rename_path(source: &Path, destination: &Path) -> Result<(), ApiError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    }

    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }

    copy_path(source, destination)?;
    delete_path(source)?;
    Ok(())
}

fn copy_path(source: &Path, destination: &Path) -> Result<(), ApiError> {
    let metadata = fs::metadata(source).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => ApiError::new(StatusCode::NOT_FOUND, "source not found"),
        _ => io_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    })?;

    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
        let entries =
            fs::read_dir(source).map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
            copy_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    }

    fs::copy(source, destination)
        .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(())
}

fn delete_path(target: &Path) -> Result<(), ApiError> {
    let metadata = fs::metadata(target).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => ApiError::new(StatusCode::NOT_FOUND, "path not found"),
        _ => io_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    })?;

    if metadata.is_dir() {
        fs::remove_dir_all(target)
            .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    } else {
        fs::remove_file(target).map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    }

    Ok(())
}

fn parse_range_header(value: &str, total_len: u64) -> Result<ByteRange, RangeError> {
    if total_len == 0 {
        return Err(RangeError::Unsatisfiable);
    }

    let range = value
        .trim()
        .strip_prefix("bytes=")
        .ok_or(RangeError::Invalid)?;

    if range.contains(',') {
        return Err(RangeError::Invalid);
    }

    let (start_raw, end_raw) = range.split_once('-').ok_or(RangeError::Invalid)?;
    if start_raw.is_empty() && end_raw.is_empty() {
        return Err(RangeError::Invalid);
    }

    if start_raw.is_empty() {
        let suffix_len = end_raw.parse::<u64>().map_err(|_| RangeError::Invalid)?;
        if suffix_len == 0 {
            return Err(RangeError::Unsatisfiable);
        }

        let start = total_len.saturating_sub(suffix_len);
        return Ok(ByteRange {
            start,
            end: total_len - 1,
        });
    }

    let start = start_raw.parse::<u64>().map_err(|_| RangeError::Invalid)?;
    if start >= total_len {
        return Err(RangeError::Unsatisfiable);
    }

    let end = if end_raw.is_empty() {
        total_len - 1
    } else {
        let requested_end = end_raw.parse::<u64>().map_err(|_| RangeError::Invalid)?;
        if requested_end < start {
            return Err(RangeError::Invalid);
        }
        requested_end.min(total_len - 1)
    };

    Ok(ByteRange { start, end })
}

fn read_file_range(path: &Path, range: ByteRange) -> Result<Vec<u8>, ApiError> {
    let mut file =
        File::open(path).map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    file.seek(SeekFrom::Start(range.start))
        .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;

    let length = range.end - range.start + 1;
    let mut bytes = Vec::with_capacity(length.min(1024 * 1024) as usize);
    file.take(length)
        .read_to_end(&mut bytes)
        .map_err(|err| io_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(bytes)
}

fn range_not_satisfiable_response(total_len: u64) -> AppResponse {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header("Content-Range", format!("bytes */{}", total_len))
        .header("Accept-Ranges", "bytes")
        .body(Vec::new())
        .unwrap()
}

fn parse_preview_path(path: &str) -> Result<(PreviewSize, String), ApiError> {
    let trimmed = path.trim_start_matches('/');
    let (size_raw, resource_raw) = trimmed
        .split_once('/')
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "missing preview path"))?;

    let size = match size_raw {
        "thumb" => PreviewSize::Thumb,
        "big" => PreviewSize::Big,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported preview size",
            ));
        }
    };

    Ok((size, format!("/{resource_raw}")))
}

fn should_preview_raw(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "gif" | "svg"
    )
}

fn create_image_preview(path: &Path, preview_size: PreviewSize) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|err| err.to_string())?;
    let image = image::load_from_memory(&bytes).map_err(|err| err.to_string())?;
    let filter = image::imageops::FilterType::Triangle;
    let preview = match preview_size {
        PreviewSize::Thumb => image.resize_to_fill(256, 256, filter),
        PreviewSize::Big => image.resize(1080, 1080, filter),
    };

    let mut output = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 82);
    encoder
        .encode_image(&preview)
        .map_err(|err| err.to_string())?;
    Ok(output)
}

fn preview_etag(path: &str, modified: SystemTime, preview_size: PreviewSize) -> String {
    let modified = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let size = match preview_size {
        PreviewSize::Thumb => "thumb",
        PreviewSize::Big => "big",
    };
    format!("\"{}:{modified}:{size}\"", rfc5987_encode(path))
}

fn preview_not_modified(request: &AppRequest, etag: &str, last_modified: &str) -> bool {
    headers_match_preview_cache(request.headers(), etag, last_modified)
}

fn headers_match_preview_cache(headers: &HeaderMap, etag: &str, last_modified: &str) -> bool {
    if headers
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == etag || candidate == "*")
        })
        .unwrap_or(false)
    {
        return true;
    }

    headers
        .get("if-modified-since")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == last_modified)
        .unwrap_or(false)
}

fn content_disposition(filename: &str, inline: bool) -> String {
    let disposition_type = if inline { "inline" } else { "attachment" };
    format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        disposition_type,
        ascii_filename_fallback(filename),
        rfc5987_encode(filename)
    )
}

fn ascii_filename_fallback(filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .map(|ch| match ch {
            '\x20'..='\x7e' if ch != '"' && ch != '\\' && ch != ';' => ch,
            _ => '_',
        })
        .collect();

    if fallback.is_empty() {
        "download".to_string()
    } else {
        fallback
    }
}

fn rfc5987_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'0'..=b'9'
            | b'a'..=b'z'
            | b'A'..=b'Z'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => encoded.push(*byte as char),
            _ => encoded.push_str(&format!("%{:02X}", byte)),
        }
    }
    encoded
}

fn format_http_date(value: SystemTime) -> String {
    let value = OffsetDateTime::from(value).to_offset(time::UtcOffset::UTC);
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let weekday = WEEKDAYS[value.weekday().number_days_from_monday() as usize];
    let month = MONTHS[value.month() as u8 as usize - 1];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        weekday,
        value.day(),
        month,
        value.year(),
        value.hour(),
        value.minute(),
        value.second()
    )
}

fn build_response(builder: Builder, body: Vec<u8>) -> Result<AppResponse, ApiError> {
    builder
        .body(body)
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

fn extract_route_path(path: &str, prefix: &str) -> String {
    match path.strip_prefix(prefix) {
        Some("") | None => "/".to_string(),
        Some(rest) => {
            if rest.is_empty() {
                "/".to_string()
            } else {
                rest.to_string()
            }
        }
    }
}

struct ResolvedPath {
    guest_path: String,
    host_path: PathBuf,
}

fn resolve_storage_path(raw_path: &str) -> Result<ResolvedPath, ApiError> {
    let decoded = percent_decode_str(raw_path)
        .decode_utf8()
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid UTF-8 in path"))?;

    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }

        if segment == ".." {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "path traversal is not allowed",
            ));
        }

        segments.push(segment.to_string());
    }

    let guest_path = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };

    let mut host_path = PathBuf::from(STORAGE_ROOT);
    for segment in &segments {
        host_path.push(segment);
    }

    Ok(ResolvedPath {
        guest_path,
        host_path,
    })
}

fn join_guest_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn file_name_from_guest_path(path: &str) -> String {
    if path == "/" {
        return String::new();
    }

    path.rsplit('/').next().unwrap_or_default().to_string()
}

fn extension_for_name(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default()
}

fn detect_file_type(name: &str) -> String {
    match Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "webm" | "mov" | "mkv" => "video".to_string(),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "audio".to_string(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => "image".to_string(),
        "pdf" => "pdf".to_string(),
        "md" | "txt" | "json" | "toml" | "yaml" | "yml" | "rs" | "go" | "js" | "ts" | "tsx"
        | "jsx" | "html" | "css" | "csv" | "xml" | "sh" | "py" | "java" | "c" | "cc" | "cpp"
        | "h" | "hpp" => "text".to_string(),
        _ => "blob".to_string(),
    }
}

fn is_text_resource(resource_type: &str) -> bool {
    matches!(resource_type, "text" | "textImmutable")
}

fn default_sorting() -> Sorting {
    Sorting {
        by: "name",
        asc: true,
    }
}

fn format_system_time(value: SystemTime) -> String {
    OffsetDateTime::from(value)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "md" | "txt" | "rs" | "go" | "toml" | "yaml" | "yml" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (current_key, current_value) = pair.split_once('=')?;
        if current_key != key {
            return None;
        }

        percent_decode_str(current_value)
            .decode_utf8()
            .ok()
            .map(|value| value.to_string())
    })
}

fn query_flag(query: &str, key: &str) -> bool {
    query_value(query, key)
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn io_error(status: StatusCode, err: std::io::Error) -> ApiError {
    ApiError::new(status, err.to_string())
}

fn normalize_asset_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return "index.html".to_string();
    }

    if trimmed.ends_with('/') {
        return format!("{}index.html", trimmed);
    }

    trimmed.to_string()
}

fn config_js_response() -> AppResponse {
    let body = r#"window.__FILEBROWSER_CONFIG__ = {
    AuthMethod: "json",
    BaseURL: "",
    CSS: false,
    Color: "",
    DisableExternal: false,
    DisableUsedPercentage: true,
    EnableExec: false,
    EnableThumbs: true,
    LogoutPage: "",
    LoginPage: false,
    Name: "File Browser Lite",
    NoAuth: true,
    ReCaptcha: false,
    ResizePreview: true,
    Signup: false,
    StaticURL: "",
    Theme: "",
    TusSettings: null,
    Version: "lite-wasi",
    LiteMode: true,
};"#;

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/javascript; charset=utf-8")
        .body(body.as_bytes().to_vec())
        .unwrap()
}

fn asset_response(path: &str, bytes: &[u8]) -> AppResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type_for_asset(path))
        .body(bytes.to_vec())
        .unwrap()
}

fn content_type_for_asset(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> AppResponse {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(body.into_bytes())
        .unwrap()
}

fn json_error(err: ApiError) -> AppResponse {
    let payload = serde_json::json!({
        "status": err.status.as_u16(),
        "error": err.message,
    });
    json_response(err.status, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_closed_range() {
        assert_eq!(
            parse_range_header("bytes=10-19", 100),
            Ok(ByteRange { start: 10, end: 19 })
        );
    }

    #[test]
    fn parses_open_ended_range() {
        assert_eq!(
            parse_range_header("bytes=90-", 100),
            Ok(ByteRange { start: 90, end: 99 })
        );
    }

    #[test]
    fn parses_suffix_range() {
        assert_eq!(
            parse_range_header("bytes=-25", 100),
            Ok(ByteRange { start: 75, end: 99 })
        );
    }

    #[test]
    fn clamps_range_end_to_file_length() {
        assert_eq!(
            parse_range_header("bytes=90-200", 100),
            Ok(ByteRange { start: 90, end: 99 })
        );
    }

    #[test]
    fn rejects_invalid_ranges() {
        assert_eq!(
            parse_range_header("items=0-10", 100),
            Err(RangeError::Invalid)
        );
        assert_eq!(
            parse_range_header("bytes=10-5", 100),
            Err(RangeError::Invalid)
        );
        assert_eq!(
            parse_range_header("bytes=0-1,4-5", 100),
            Err(RangeError::Invalid)
        );
        assert_eq!(parse_range_header("bytes=-", 100), Err(RangeError::Invalid));
    }

    #[test]
    fn rejects_unsatisfiable_ranges() {
        assert_eq!(
            parse_range_header("bytes=100-101", 100),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range_header("bytes=-0", 100),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range_header("bytes=0-0", 0),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn builds_range_not_satisfiable_response() {
        let response = range_not_satisfiable_response(123);
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response
                .headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok()),
            Some("bytes */123")
        );
    }

    #[test]
    fn parses_preview_paths() {
        let (size, path) = parse_preview_path("/thumb/photos/a.jpg").unwrap();
        assert_eq!(size, PreviewSize::Thumb);
        assert_eq!(path, "/photos/a.jpg");

        let (size, path) = parse_preview_path("/big/photos/a.jpg").unwrap();
        assert_eq!(size, PreviewSize::Big);
        assert_eq!(path, "/photos/a.jpg");

        assert!(parse_preview_path("/tiny/photos/a.jpg").is_err());
        assert!(parse_preview_path("/thumb").is_err());
    }

    #[test]
    fn formats_http_date_for_last_modified() {
        assert_eq!(
            format_http_date(SystemTime::UNIX_EPOCH),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn content_disposition_uses_safe_fallback_and_rfc5987_filename() {
        assert_eq!(
            content_disposition("a\";中.jpg", true),
            "inline; filename=\"a___.jpg\"; filename*=UTF-8''a%22%3B%E4%B8%AD.jpg"
        );
    }

    #[test]
    fn preview_etag_is_ascii_safe() {
        assert_eq!(
            preview_etag("/图/a.jpg", SystemTime::UNIX_EPOCH, PreviewSize::Thumb),
            "\"%2F%E5%9B%BE%2Fa.jpg:0:thumb\""
        );
    }

    #[test]
    fn matches_preview_cache_conditions() {
        let mut headers = HeaderMap::new();
        headers.insert("if-none-match", "\"abc\"".parse().unwrap());
        assert!(headers_match_preview_cache(
            &headers,
            "\"abc\"",
            "Thu, 01 Jan 1970 00:00:00 GMT"
        ));

        headers.clear();
        headers.insert(
            "if-modified-since",
            "Thu, 01 Jan 1970 00:00:00 GMT".parse().unwrap(),
        );
        assert!(headers_match_preview_cache(
            &headers,
            "\"abc\"",
            "Thu, 01 Jan 1970 00:00:00 GMT"
        ));
    }
}
