use log::error;
use std::env;
use warp::Filter;

#[tokio::main]
async fn main() {
    if env::var_os("RUST_LOG").is_none() {
        env::set_var("RUST_LOG", "cnb-shim=info");
    }
    pretty_env_logger::init();

    let buildpack_dir = std::env::current_dir().unwrap_or_else(|_| {
        error!("Could not get the current directory.");
        std::process::exit(1);
    });

    let routes = filters::routes(buildpack_dir).with(warp::log("cnb-shim"));
    warp::serve(routes).run(([0, 0, 0, 0], 3000)).await;
}

mod filters {
    use super::{handlers, models};
    use std::path::PathBuf;
    use warp::{Filter, Rejection, Reply};

    pub fn routes(
        buildpack_dir: impl Into<PathBuf>,
    ) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
        shim(buildpack_dir).or(health())
    }

    /// GET /health
    pub fn health() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
        warp::path!("health")
            .and(warp::get())
            .and_then(handlers::health_check)
    }

    /// GET /v1/:namespace/:name
    pub fn shim(
        buildpack_dir: impl Into<PathBuf>,
    ) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
        warp::path!("v1" / String / String)
            .and(warp::get())
            .and(warp::query::<models::ShimOptions>())
            .and(with_buildpack_dir(buildpack_dir.into()))
            .and_then(handlers::shim)
            .recover(handlers::rejection)
    }

    fn with_buildpack_dir(
        buildpack_dir: PathBuf,
    ) -> impl Filter<Extract = (PathBuf,), Error = std::convert::Infallible> + Clone {
        warp::any().map(move || buildpack_dir.clone())
    }
}

mod handlers {
    use super::models;
    use flate2::{read::GzDecoder, write::GzEncoder, Compression};
    use libcnb::data::buildpack;
    use log::{error, info};
    use std::{
        convert::Infallible,
        fs,
        io::Write,
        path::{Path, PathBuf},
        str::FromStr,
    };
    use tar::Archive;
    use thiserror::Error;
    use tokio_stream::StreamExt;
    use warp::{
        http::StatusCode,
        reject::{Reject, Rejection},
        Reply,
    };

    const DEFAULT_API_VERSION: &str = "0.4";
    const DEFAULT_VERSION: &str = "0.1.0";
    const V2_BUILDPACK_REGISTRY_URL: &str =
        "https://buildpack-registry.s3.amazonaws.com/buildpacks";

    #[derive(Debug)]
    /// Unrecoverable Error, HTTP Status Code 500
    struct ServiceError(String);

    impl Reject for ServiceError {}

    impl ServiceError {
        fn new(msg: impl Into<String>) -> Self {
            ServiceError(msg.into())
        }
    }

    #[derive(Debug)]
    /// Bad Request Error, HTTP Status Code 400
    struct BadRequestError(String);

    impl Reject for BadRequestError {}

    impl BadRequestError {
        fn new(msg: impl Into<String>) -> Self {
            BadRequestError(msg.into())
        }
    }

    pub async fn rejection(err: Rejection) -> Result<impl Reply, Rejection> {
        if err.is_not_found() {
            return Err(warp::reject::not_found());
        }

        let code;
        let message;

        if let Some(service_error) = err.find::<ServiceError>() {
            error!("{}", service_error.0);
        }
        if let Some(request_error) = err.find::<BadRequestError>() {
            error!("{}", request_error.0);
        }
        message = "INTERNAL SERVER ERROR";
        code = StatusCode::INTERNAL_SERVER_ERROR;

        Ok(warp::reply::with_status(message, code))
    }

    pub async fn health_check() -> Result<impl Reply, Infallible> {
        Ok(warp::reply::with_status("health check ok", StatusCode::OK))
    }

    pub async fn shim(
        namespace: String,
        name: String,
        query_params: models::ShimOptions,
        buildpack_dir: PathBuf,
    ) -> Result<impl Reply, Rejection> {
        info!("shimming: {}/{}", namespace, name);

        let id = buildpack::BuildpackId::from_str(&format!("{}/{}", namespace, name))
            .map_err(|_| BadRequestError::new("invalid buildpack id"))?;
        let version = buildpack::Version::parse(
            &query_params
                .version
                .unwrap_or_else(|| String::from(DEFAULT_VERSION)),
        )
        .map_err(|err| BadRequestError::new(format!("invalid buildpack version: {:?}", err)))?;
        let name = query_params
            .name
            .unwrap_or_else(|| String::from(id.as_str()));
        let api = buildpack::BuildpackApi::from_str(
            &query_params
                .api
                .unwrap_or_else(|| String::from(DEFAULT_API_VERSION)),
        )
        .map_err(|_| BadRequestError::new("invalid buildpack api"))?;
        let stacks = query_params
            .stacks
            .unwrap_or_else(|| [String::from("heroku-18"), String::from("heroku-20")].into())
            .iter()
            .map(|stack| {
                Ok(buildpack::Stack {
                    id: buildpack::StackId::from_str(stack)?,
                    mixins: Vec::new(),
                })
            })
            .collect::<Result<Vec<buildpack::Stack>, libcnb::Error>>()
            .map_err(|_| BadRequestError::new("invalid stack"))?;

        let shimmed_buildpack = format!("{}.tgz", uuid::Uuid::new_v4());
        let v2_buildpack_url = format!("{}/{}.tgz", V2_BUILDPACK_REGISTRY_URL, &id.as_str());

        let tmp_dir = tempfile::tempdir().map_err(|_| ServiceError::new("Can't create tmp dir"))?;

        let shimmed_buildpack_dir = tmp_dir.path().join("buildpack");
        let bin_dir = shimmed_buildpack_dir.join("bin");
        fs::create_dir_all(&bin_dir).map_err(|_| ServiceError::new("Can't create bin dir"))?;
        for bin in ["detect", "build", "release", "exports"].iter() {
            fs::copy(buildpack_dir.join("bin").join(bin), bin_dir.join(bin))
                .map_err(|_| ServiceError::new("Can't copy file"))?;
        }

        let buildpack_toml = buildpack::BuildpackToml {
            api,
            buildpack: buildpack::Buildpack {
                id,
                name,
                version,
                homepage: None,
                clear_env: false,
            },
            stacks,
            order: Vec::new(),
            metadata: toml::value::Table::new(),
        };

        let buildpack_toml_path = shimmed_buildpack_dir.join("buildpack.toml");
        fs::write(
            buildpack_toml_path,
            toml::to_string(&buildpack_toml).map_err(|err| {
                ServiceError::new(format!("Can't convert buildpack.toml to string: {:?}", err))
            })?,
        )
        .map_err(|_| ServiceError::new("Can't write buildpack.toml to disk"))?;

        let v2_buildpack_path = tmp_dir.path().join("buildpack.tgz");
        download(v2_buildpack_url, &v2_buildpack_path)
            .await
            .map_err(|err| match err {
                DownloadError::IOError(_) => {
                    Rejection::from(ServiceError::new("Can't download v2 buildpack"))
                }
                DownloadError::ReqwestError(_) => {
                    Rejection::from(BadRequestError::new("Can't download v2 buildpack"))
                }
            })?;

        untar(&v2_buildpack_path, shimmed_buildpack_dir.join("target"))
            .map_err(|_| ServiceError::new("Could not untar v2 buildpack"))?;
        let shimmed_buildpack_archive = tmp_dir.path().join(&shimmed_buildpack);
        archive(&shimmed_buildpack_archive, shimmed_buildpack_dir)
            .map_err(|_| ServiceError::new("Could not create shimmed tarball"))?;

        Ok(http::response::Builder::new()
            .status(200)
            .header("Content-Type", "application/x-gzip")
            .header(
                "Content-Disposition",
                format!("attachment; filename=\"{}\"", &shimmed_buildpack),
            )
            .body(
                fs::read(&shimmed_buildpack_archive)
                    .map_err(|_| ServiceError::new("Could not read shimmed buildpack"))?,
            )
            .map_err(|_| ServiceError::new("Could not send response."))?)
    }

    async fn download(uri: impl AsRef<str>, dst: impl AsRef<Path>) -> Result<(), DownloadError> {
        let response = reqwest::get(uri.as_ref()).await?;
        let mut stream = response.bytes_stream();
        let mut file = fs::File::create(dst)?;

        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?)?;
        }

        Ok(())
    }

    fn untar(file: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), DownloadError> {
        let tar_gz = fs::File::open(file.as_ref())?;
        let tar = GzDecoder::new(tar_gz);
        let mut archive = Archive::new(tar);
        archive.unpack(dst.as_ref())?;

        Ok(())
    }

    fn archive(dst: impl AsRef<Path>, src: impl AsRef<Path>) -> Result<(), ArchiveError> {
        let file = fs::File::create(dst.as_ref())?;
        let enc = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(enc);

        builder.append_dir_all(".", src)?;

        Ok(())
    }

    #[derive(Error, Debug)]
    enum DownloadError {
        #[error("failed to write to disk")]
        IOError(#[from] std::io::Error),
        #[error("failed to download file")]
        ReqwestError(#[from] reqwest::Error),
    }

    #[derive(Error, Debug)]
    enum ArchiveError {
        #[error("failed to write to disk")]
        IOError(#[from] std::io::Error),
    }
}

mod models {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct ShimOptions {
        pub version: Option<String>,
        pub name: Option<String>,
        pub api: Option<String>,
        pub stacks: Option<Vec<String>>,
    }
}
