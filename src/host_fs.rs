//! Host-bridged filesystem implementation for the Extism guest.
//!
//! `HostFs` implements `AsyncFileSystem` by delegating all operations to
//! host functions. Since host function calls are synchronous from the guest's
//! perspective, the async methods return immediately-ready futures.

use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};

use diaryx_core::fs::{AsyncFileSystem, BoxFuture};

use diaryx_plugin_sdk::prelude::*;

/// Filesystem backed by Extism host function calls.
///
/// All I/O is delegated to the host via `host_read_file`, `host_write_file`, etc.
/// These calls are synchronous from the guest's perspective, so the async methods
/// complete immediately.
#[derive(Clone)]
pub struct HostFs;

impl AsyncFileSystem for HostFs {
    fn read_to_string<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            host::fs::read_file(&path_str).map_err(|e| Error::new(ErrorKind::Other, e))
        })
    }

    fn write_file<'a>(&'a self, path: &'a Path, content: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            host::fs::write_file(&path_str, content).map_err(|e| Error::new(ErrorKind::Other, e))
        })
    }

    fn create_new<'a>(&'a self, path: &'a Path, content: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            // Check if file exists first
            let exists =
                host::fs::file_exists(&path_str).map_err(|e| Error::new(ErrorKind::Other, e))?;
            if exists {
                return Err(Error::new(ErrorKind::AlreadyExists, "File already exists"));
            }
            host::fs::write_file(&path_str, content).map_err(|e| Error::new(ErrorKind::Other, e))
        })
    }

    fn delete_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            host::fs::delete_file(&path_str).map_err(|e| Error::new(ErrorKind::Other, e))
        })
    }

    fn list_md_files<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, Result<Vec<PathBuf>>> {
        Box::pin(async move {
            let prefix = dir.to_string_lossy();
            let files =
                host::fs::list_files(&prefix).map_err(|e| Error::new(ErrorKind::Other, e))?;
            Ok(files
                .into_iter()
                .filter(|f| f.ends_with(".md"))
                .map(PathBuf::from)
                .collect())
        })
    }

    fn exists<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            host::fs::file_exists(&path_str).unwrap_or(false)
        })
    }

    fn create_dir_all<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, Result<()>> {
        // Directories are implicit in the host filesystem
        Box::pin(async move { Ok(()) })
    }

    fn is_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            let normalized = path_str.trim_end_matches('/');
            if normalized.is_empty() || normalized == "." {
                return true;
            }

            // If path exists and has recursive children, treat as directory.
            // This works across native/web hosts where list_files is recursive.
            if host::fs::file_exists(normalized).unwrap_or(false)
                && let Ok(entries) = host::fs::list_files(normalized)
            {
                let prefix = format!("{normalized}/");
                if entries
                    .iter()
                    .any(|entry| entry != normalized && entry.starts_with(&prefix))
                {
                    return true;
                }
            }

            // Fallback heuristic when host can't answer directory-ness directly.
            path.extension().is_none()
        })
    }

    fn move_file<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let from_str = from.to_string_lossy();
            let to_str = to.to_string_lossy();

            let from_exists =
                host::fs::file_exists(&from_str).map_err(|e| Error::new(ErrorKind::Other, e))?;
            if !from_exists {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("Source file not found: {from_str}"),
                ));
            }

            let to_exists =
                host::fs::file_exists(&to_str).map_err(|e| Error::new(ErrorKind::Other, e))?;
            if to_exists {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    format!("Destination already exists: {to_str}"),
                ));
            }

            let content =
                host::fs::read_file(&from_str).map_err(|e| Error::new(ErrorKind::Other, e))?;
            host::fs::write_file(&to_str, &content)
                .map_err(|e| Error::new(ErrorKind::Other, e))?;
            host::fs::delete_file(&from_str).map_err(|e| Error::new(ErrorKind::Other, e))
        })
    }

    fn write_binary<'a>(&'a self, path: &'a Path, content: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let path_str = path.to_string_lossy();
            host::fs::write_binary(&path_str, content)
                .map_err(|e| Error::new(ErrorKind::Other, e))
        })
    }

    fn list_files<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, Result<Vec<PathBuf>>> {
        Box::pin(async move {
            let prefix = dir.to_string_lossy();
            let files =
                host::fs::list_files(&prefix).map_err(|e| Error::new(ErrorKind::Other, e))?;
            Ok(files.into_iter().map(PathBuf::from).collect())
        })
    }
}
