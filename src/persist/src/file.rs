// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! File backed implementations for testing and benchmarking.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write as StdWrite};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ore::cast::CastFrom;

use crate::error::Error;
use crate::storage::{Blob, LockInfo, Log, SeqNo};

/// Inner struct handles to separate files that store the data and metadata about the
/// most recently truncated sequence number for [FileLog].
struct FileLogCore {
    dataz: File,
    metadata: File,
}

/// A naive implementation of [Log] backed by files.
pub struct FileLog {
    base_dir: Option<PathBuf>,
    dataz: Arc<Mutex<FileLogCore>>,
    seqno: Range<SeqNo>,
    buf: Vec<u8>,
}

impl FileLog {
    const DATA_PATH: &'static str = "DATA";
    const LOCKFILE_PATH: &'static str = "LOCK";
    const METADATA_PATH: &'static str = "META";

    /// Returns a new [FileLog] which stores files under the given dir.
    ///
    /// To ensure directory-wide mutual exclusion, a LOCK file is placed in
    /// base_dir at construction time. If this file already exists (indicating
    /// that another FileLog is already using the dir), an error is returned
    /// from `new`.
    ///
    /// The contents of `lock_info` are stored in the LOCK file and should
    /// include anything that would help debug an unexpected LOCK file, such as
    /// version, ip, worker number, etc.
    ///
    /// The data is stored in a separate file, and is formatted as a sequential list of
    /// chunks corresponding to each write. Each chunk consists of:
    /// `length` - A 64 bit unsigned int (little-endian) indicating the size of `data`.
    /// `data` - `length` bytes of data.
    /// `sequence_number` - A 64 bit unsigned int (little-endian) indicating the sequence number
    /// assigned to `data`.
    ///
    /// Additionally, the metadata about the last truncated sequence number is stored in a
    /// metadata file, which only ever contains a single 64 bit unsigned integer (also little-endian)
    /// that indicates the most recently truncated offset (ie all offsets less than this are truncated).
    pub fn new<P: AsRef<Path>>(base_dir: P, lock_info: LockInfo) -> Result<Self, Error> {
        let base_dir = base_dir.as_ref();
        fs::create_dir_all(&base_dir)?;
        {
            // TODO: flock this for good measure?
            let _ = file_storage_lock(&Self::lockfile_path(&base_dir), lock_info)?;
        }
        let data_path = Self::data_path(&base_dir);
        let mut data_file = OpenOptions::new()
            .append(true)
            .read(true)
            .create(true)
            .open(&data_path)?;

        let metadata_path = Self::metadata_path(&base_dir);
        let mut metadata_file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(&metadata_path)?;

        // Retrieve the last truncated sequence number from the metadata file
        // unless that file was just created, in which case default to 0.
        let mut bytes = Vec::new();
        metadata_file.read_to_end(&mut bytes)?;
        let len = bytes.len();
        let seqno_start = if len != 0 {
            let mut bytes = &bytes[..];
            let mut buf = [0u8; 8];
            // Decode sequence number
            bytes.read_exact(&mut buf).map_err(|e| {
                format!(
                    "could not read log metadata file: found {} bytes (expected 8) {:?}",
                    len, e,
                )
            })?;
            SeqNo(u64::from_le_bytes(buf))
        } else {
            metadata_file.write_all(&0u64.to_le_bytes())?;
            metadata_file.sync_all()?;
            SeqNo(0)
        };

        // Retrieve the most recently written sequence number in the log,
        // unless the log is empty in which case default to 0.
        let seqno_end = {
            let len = data_file.metadata()?.len();

            if len > 0 {
                let mut buf = [0u8; 8];
                data_file.seek(SeekFrom::End(-8))?;
                data_file.read_exact(&mut buf)?;
                // NB: seqno_end is exclusive, so we have to add one to the
                // seqno of the most recent write to reconstruct it.
                SeqNo(u64::from_le_bytes(buf) + 1)
            } else {
                SeqNo(0)
            }
        };

        if seqno_start > seqno_end {
            return Err(format!(
                "invalid sequence number range found for file log: start: {:?} end: {:?}",
                seqno_start, seqno_end,
            )
            .into());
        }

        Ok(FileLog {
            base_dir: Some(base_dir.to_owned()),
            dataz: Arc::new(Mutex::new(FileLogCore {
                dataz: data_file,
                metadata: metadata_file,
            })),
            seqno: seqno_start..seqno_end,
            buf: Vec::new(),
        })
    }

    fn lockfile_path(base_dir: &Path) -> PathBuf {
        base_dir.join(Self::LOCKFILE_PATH)
    }

    fn data_path(base_dir: &Path) -> PathBuf {
        base_dir.join(Self::DATA_PATH)
    }

    fn metadata_path(base_dir: &Path) -> PathBuf {
        base_dir.join(Self::METADATA_PATH)
    }

    fn ensure_open(&self) -> Result<PathBuf, Error> {
        self.base_dir
            .clone()
            .ok_or_else(|| return Error::from("FileLog unexpectedly closed"))
    }
}

impl Log for FileLog {
    fn write_sync(&mut self, buf: Vec<u8>) -> Result<SeqNo, Error> {
        self.ensure_open()?;

        let write_seqno = self.seqno.end;
        self.seqno = self.seqno.start..SeqNo(write_seqno.0 + 1);
        // Write length prefixed data, and then the sequence number.
        let len = u64::cast_from(buf.len());

        // NB: the write buffer never shrinks, and this pattern may not be what we want
        // under write workloads with rare very large writes.
        self.buf.clear();
        self.buf.extend(&len.to_le_bytes());
        self.buf.extend(buf);
        self.buf.extend(&write_seqno.0.to_le_bytes());

        let mut guard = self.dataz.lock()?;
        guard.dataz.write_all(&self.buf)?;
        guard.dataz.sync_all()?;
        Ok(write_seqno)
    }

    fn snapshot<F>(&self, mut logic: F) -> Result<Range<SeqNo>, Error>
    where
        F: FnMut(SeqNo, &[u8]) -> Result<(), Error>,
    {
        self.ensure_open()?;
        let mut bytes = Vec::new();

        {
            let mut guard = self.dataz.lock()?;
            // This file was opened with append mode, so any future writes
            // will reset the file cursor to the end before writing.
            guard.dataz.seek(SeekFrom::Start(0))?;
            guard.dataz.read_to_end(&mut bytes)?;
        }

        let mut bytes = &bytes[..];
        let mut len_raw = [0u8; 8];
        while !bytes.is_empty() {
            // Decode the data
            bytes.read_exact(&mut len_raw)?;
            let data_len = u64::from_le_bytes(len_raw);
            // TODO: could reuse the underlying buffer here to avoid allocating each time.
            let mut data = vec![0u8; usize::cast_from(data_len)];
            bytes.read_exact(&mut data[..])?;

            // Decode sequence number
            bytes.read_exact(&mut len_raw)?;
            let seqno = SeqNo(u64::from_le_bytes(len_raw));

            if seqno < self.seqno.start {
                // This record has already been truncated so we can ignore it.
                continue;
            } else if seqno >= self.seqno.end {
                return Err(format!(
                    "invalid sequence number {:?} found for log containing {:?}",
                    seqno, self.seqno
                )
                .into());
            }

            logic(seqno, &data)?;
        }
        Ok(self.seqno.clone())
    }

    /// Logically truncates the log so that future reads ignore writes at sequence numbers
    /// less than `upper`.
    ///
    /// TODO: actually reclaim disk space as part of truncating.
    fn truncate(&mut self, upper: SeqNo) -> Result<(), Error> {
        self.ensure_open()?;
        // TODO: Test the edge cases here.
        if upper <= self.seqno.start || upper > self.seqno.end {
            return Err(format!(
                "invalid truncation {:?} for log containing: {:?}",
                upper, self.seqno
            )
            .into());
        }
        self.seqno = upper..self.seqno.end;

        let mut guard = self.dataz.lock()?;
        guard.metadata.seek(SeekFrom::Start(0))?;
        guard.metadata.set_len(0)?;
        guard
            .metadata
            .write_all(&self.seqno.start.0.to_le_bytes())?;
        guard.metadata.sync_all()?;

        Ok(())
    }

    fn close(&mut self) -> Result<bool, Error> {
        if let Ok(base_dir) = self.ensure_open() {
            let lockfile_path = Self::lockfile_path(&base_dir);
            fs::remove_file(lockfile_path)?;
            self.base_dir = None;
            Ok(true)
        } else {
            // Already closed. Close implementations must be idempotent.
            Ok(false)
        }
    }
}

/// Implementation of [Blob] backed by files.
pub struct FileBlob {
    base_dir: Option<PathBuf>,
}

impl FileBlob {
    const LOCKFILE_PATH: &'static str = "LOCK";

    /// Returns a new [FileBlob] which stores files under the given dir.
    ///
    /// To ensure directory-wide mutual exclusion, a LOCK file is placed in
    /// base_dir at construction time. If this file already exists (indicating
    /// that another FileBlob is already using the dir), an error is returned
    /// from `new`.
    ///
    /// The contents of `lock_info` are stored in the LOCK file and should
    /// include anything that would help debug an unexpected LOCK file, such as
    /// version, ip, worker number, etc.
    pub fn new<P: AsRef<Path>>(base_dir: P, lock_info: LockInfo) -> Result<Self, Error> {
        let base_dir = base_dir.as_ref();
        fs::create_dir_all(&base_dir)?;
        {
            let _ = file_storage_lock(&Self::lockfile_path(&base_dir), lock_info)?;
        }
        Ok(FileBlob {
            base_dir: Some(base_dir.to_path_buf()),
        })
    }

    fn lockfile_path(base_dir: &Path) -> PathBuf {
        base_dir.join(Self::LOCKFILE_PATH)
    }

    fn blob_path(&self, key: &str) -> Result<PathBuf, Error> {
        self.base_dir
            .as_ref()
            .map(|base_dir| base_dir.join(key))
            .ok_or_else(|| return Error::from("FileBlob unexpectedly closed"))
    }
}

impl Blob for FileBlob {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let file_path = self.blob_path(key)?;
        let mut file = match File::open(file_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(Some(buf))
    }

    fn set(&mut self, key: &str, value: Vec<u8>, allow_overwrite: bool) -> Result<(), Error> {
        let file_path = self.blob_path(key)?;
        if allow_overwrite {
            // allow_overwrite=true requires atomic writes, so write to a temp
            // file and rename it into place.
            let mut tmp_name = file_path.clone();
            debug_assert_eq!(tmp_name.extension(), None);
            tmp_name.set_extension("tmp");
            // NB: Don't use create_new(true) for this so that if we have a
            // partial one from a previous crash, it will just get overwritten
            // (which is safe).
            let mut file = File::create(&tmp_name)?;
            file.write_all(&value[..])?;
            file.sync_all()?;
            fs::rename(tmp_name, &file_path)?;
            // TODO: We also need to fsync the directory to be truly confidant
            // that this is permanently there. It doesn't seem like this is
            // available in the stdlib, find a crate for it?
        } else {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)?;
            file.write_all(&value[..])?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn delete(&mut self, key: &str) -> Result<(), Error> {
        let file_path = self.blob_path(key)?;
        // TODO: strict correctness requires that we fsync the parent directory
        // as well after file removal.
        if let Err(err) = fs::remove_file(&file_path) {
            // delete is documented to succeed if the key doesn't exist.
            if err.kind() != ErrorKind::NotFound {
                return Err(err.into());
            }
        };

        Ok(())
    }
    fn close(&mut self) -> Result<bool, Error> {
        if let Some(base_dir) = self.base_dir.as_ref() {
            let lockfile_path = Self::lockfile_path(&base_dir);
            fs::remove_file(lockfile_path)?;
            self.base_dir = None;
            Ok(true)
        } else {
            // Already closed. Close implementations must be idempotent.
            Ok(false)
        }
    }
}

fn file_storage_lock(lockfile_path: &Path, new_lock: LockInfo) -> Result<File, Error> {
    // TODO: flock this for good measure? There's all sorts of tricky edge cases
    // here when this gets called concurrently, and we'll have the same issues
    // when we add an s3 impl of Blob. Revisit this in a principled way.
    let mut lockfile = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lockfile_path)?;
    let _ = new_lock.check_reentrant_for(&lockfile_path, &mut lockfile)?;
    // Overwrite the data and then truncate the length if necessary. Truncating
    // first could produce a race condition where the file looks empty to a
    // process concurrently trying to lock it.
    lockfile.seek(SeekFrom::Start(0))?;
    let contents = new_lock.to_string().into_bytes();
    lockfile.write_all(&contents)?;
    lockfile.set_len(u64::cast_from(contents.len()))?;
    lockfile.sync_all()?;
    Ok(lockfile)
}

#[cfg(test)]
mod tests {
    use crate::storage::tests::{blob_impl_test, log_impl_test};

    use super::*;

    #[test]
    fn file_blob() -> Result<(), Error> {
        let temp_dir = tempfile::tempdir()?;
        blob_impl_test(move |t| {
            let instance_dir = temp_dir.path().join(t.path);
            FileBlob::new(instance_dir, (t.reentrance_id, "file_blob_test").into())
        })
    }

    #[test]
    fn file_log() -> Result<(), Error> {
        let temp_dir = tempfile::tempdir()?;
        log_impl_test(move |t| {
            let instance_dir = temp_dir.path().join(t.path);
            FileLog::new(instance_dir, (t.reentrance_id, "file_log_test").into())
        })
    }

    #[test]
    fn file_storage_lock_reentrance() -> Result<(), Error> {
        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().join("file_storage_lock_reentrance");

        // Sanity check that overwriting the contents with shorter contents as
        // well as with longer contents both work.
        let _f1 = file_storage_lock(
            &path,
            LockInfo::new("reentrance0".to_owned(), "foo".repeat(5))?,
        )?;
        assert_eq!(fs::read_to_string(&path)?, "reentrance0\nfoofoofoofoofoo");
        let _f2 = file_storage_lock(
            &path,
            LockInfo::new("reentrance0".to_owned(), "foo".to_owned())?,
        )?;
        assert_eq!(fs::read_to_string(&path)?, "reentrance0\nfoo");
        let _f3 = file_storage_lock(
            &path,
            LockInfo::new("reentrance0".to_owned(), "foo".repeat(3))?,
        )?;
        assert_eq!(fs::read_to_string(&path)?, "reentrance0\nfoofoofoo");

        drop(_f1);
        drop(_f2);
        drop(_f3);
        Ok(())
    }
}
