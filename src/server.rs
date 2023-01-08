use actix_files::NamedFile;
use actix_web::error::ResponseError;
use actix_web::http::StatusCode;
use actix_web::middleware::Logger;
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use anyhow::Context;
use std::fmt::{Debug, Display};
use std::path::{Path, PathBuf};

use crate::db::Cache;
use crate::store::{get_file_for_source, realise};

#[derive(Debug)]
struct NotFoundError<E: Display + Debug>(E);
impl<E: Display + Debug> ResponseError for NotFoundError<E> {
    fn status_code(&self) -> StatusCode {
        log::info!("returning 404 because of {}", &self);
        StatusCode::NOT_FOUND
    }
}
impl<E: Display + Debug> Display for NotFoundError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", &self.0)
    }
}

async fn unwrap_file<T: AsRef<Path>>(path: anyhow::Result<Option<T>>) -> impl Responder {
    match path {
        Ok(Some(p)) => {
            let exists = realise(p.as_ref()).await;
            match exists {
                Ok(()) => Ok(NamedFile::open(p.as_ref())),
                Err(e) => Err(NotFoundError(e)),
            }
        }
        Ok(None) => Err(NotFoundError(anyhow::anyhow!("not found"))),
        Err(e) => Err(NotFoundError(e.into())),
    }
}

#[get("/buildid/{buildid}/debuginfo")]
async fn get_debuginfo(
    buildid: web::Path<String>,
    cache: web::Data<&'static Cache>,
) -> impl Responder {
    let res = cache.get_debuginfo(&buildid).await;
    unwrap_file(res).await
}

#[get("/buildid/{buildid}/executable")]
async fn get_executable(
    buildid: web::Path<String>,
    cache: web::Data<&'static Cache>,
) -> impl Responder {
    let res = cache.get_executable(&buildid).await;
    unwrap_file(res).await
}

async fn fetch_and_get_source(
    buildid: String,
    request: PathBuf,
    cache: &'static Cache,
) -> anyhow::Result<Option<PathBuf>> {
    let source = cache
        .get_source(&buildid)
        .await
        .with_context(|| format!("getting source of {} from cache", &buildid))?;
    let source = match source {
        None => return Ok(None),
        Some(x) => PathBuf::from(x),
    };
    realise(source.as_ref())
        .await
        .with_context(|| format!("realizing source {}", source.display()))?;
    let file =
        tokio::task::spawn_blocking(move || get_file_for_source(source.as_ref(), request.as_ref()))
            .await?
            .context("looking in source")?;
    Ok(file)
}

#[get("/buildid/{buildid}/source/{path:.*}")]
async fn get_source(
    param: web::Path<(String, String)>,
    cache: web::Data<&'static Cache>,
) -> impl Responder {
    let path: &str = &param.1;
    let request = PathBuf::from(path);
    unwrap_file(fetch_and_get_source(param.0.to_owned(), request, &cache).await).await
}

#[get("/buildid/{buildid}/section/{section}")]
async fn get_section(_param: web::Path<(String, String)>) -> impl Responder {
    HttpResponse::NotImplemented().finish()
}

pub async fn run_server() -> anyhow::Result<()> {
    let cache = Cache::open().await.context("opening global cache")?;
    let cache: &'static Cache = Box::leak(Box::new(cache));
    crate::store::spawn_store_watcher(cache);
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(cache))
            .wrap(Logger::default())
            .service(get_debuginfo)
            .service(get_executable)
            .service(get_source)
            .service(get_section)
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await?;
    Ok(())
}
