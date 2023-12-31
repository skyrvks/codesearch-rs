// Copyright 2015 Vernon Jones.
// Original code Copyright 2011 The Go Authors.  All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

#[macro_use]
extern crate clap;
extern crate glob;
extern crate regex;
#[macro_use]
extern crate log;
extern crate walkdir;

extern crate consts;
extern crate libcindex;
extern crate libcsearch;
extern crate libcustomlogger;
extern crate libprofiling;
extern crate libvarint;

use libcindex::writer::{IndexErrorKind, IndexWriter};
use libcsearch::reader::IndexReader;
use log::LevelFilter;
use walkdir::WalkDir;

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, FileType};
use std::io::{self, BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(windows)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;

#[cfg(not(unix))]
fn is_regular_file(meta: FileType) -> bool {
    !meta.is_dir()
}

#[cfg(unix)]
fn is_regular_file(meta: FileType) -> bool {
    meta.is_file()
        && !meta.is_fifo()
        && !meta.is_socket()
        && !meta.is_block_device()
        && !meta.is_char_device()
}

#[cfg(windows)]
fn normalize<P: AsRef<Path>>(p: P) -> io::Result<PathBuf> {
    let mut out = env::current_dir()?;
    let mut it = p.as_ref().components();
    // only drop current directory if the first part of p is a prefix (C:/) or root (/)
    if let Some(c) = it.next() {
        match c {
            r @ Component::Prefix(_) | r @ Component::RootDir => {
                out = PathBuf::new();
                out.push(r.as_os_str());
            }
            r @ _ => out.push(r.as_os_str()),
        }
    } else {
        return Ok(out);
    };
    for each_part in it {
        match each_part {
            Component::CurDir => (),
            Component::ParentDir => {
                out.pop();
            }
            r @ _ => out.push(r.as_os_str()),
        }
    }
    Ok(out)
}

#[cfg(not(windows))]
fn normalize<P: AsRef<Path>>(p: P) -> io::Result<PathBuf> {
    fs::canonicalize(p.as_ref())
}

fn get_value_from_matches<F: FromStr>(matches: &clap::ArgMatches, name: &str) -> Option<F> {
    match matches.value_of(name) {
        Some(s) => {
            if let Ok(t) = s.parse::<F>() {
                Some(t)
            } else {
                panic!("can't convert value '{}' to number", s);
            }
        }
        _ => None,
    }
}

const ABOUT: &str = "
cindex prepares the trigram index for use by csearch.  The index
is the file named by $CSEARCHINDEX, or else $HOME/.csearchindex.
The simplest invocation is

	cindex path...

which adds the file or directory tree named by each path to the index.
For example:

    cindex $HOME/src /usr/include

or, equivalently:

	cindex $HOME/src
	cindex /usr/include

If cindex is invoked with no paths, it reindexes the paths that have
already been added, in case the files have changed.  Thus, 'cindex' by
itself is a useful command to run in a nightly cron job.

By default cindex adds the named paths to the index but preserves
information about other paths that might already be indexed
(the ones printed by cindex --list).  The --reset flag causes cindex to
delete the existing index before indexing the new paths.
With no path arguments, cindex -reset removes the index.";

fn main() {
    let matches = clap::App::new("cindex")
        .version(crate_version!())
        .author(
            "Vernon Jones <vernonrjones@gmail.com> (original code copyright 2011 the Go \
                 authors)",
        )
        .about(ABOUT)
        .arg(
            clap::Arg::with_name("path")
                .index(1)
                .multiple(true)
                .help("path to index"),
        )
        .arg(
            clap::Arg::with_name("list-paths")
                .long("list")
                .help("list indexed paths and exit"),
        )
        .arg(
            clap::Arg::with_name("reset-index")
                .long("reset")
                .conflicts_with("path")
                .conflicts_with("list-paths")
                .help("discard existing index"),
        )
        .arg(
            clap::Arg::with_name("INDEX_FILE")
                .long("indexpath")
                .takes_value(true)
                .help("use specified INDEX_FILE as the index path. overrides $CSEARCHINDEX"),
        )
        .arg(
            clap::Arg::with_name("no-follow-simlinks")
                .long("no-follow-simlinks")
                .help("do not follow symlinked files and directories"),
        )
        .arg(
            clap::Arg::with_name("MAX_FILE_SIZE_BYTES")
                .long("maxFileLen")
                .takes_value(true)
                .help("skip indexing a file if longer than this size in bytes"),
        )
        .arg(
            clap::Arg::with_name("MAX_LINE_LEN_BYTES")
                .long("maxLineLen")
                .takes_value(true)
                .help("skip indexing a file if it has a line longer than this size in bytes"),
        )
        .arg(
            clap::Arg::with_name("MAX_TRIGRAMS_COUNT")
                .long("maxtrigrams")
                .takes_value(true)
                .help("skip indexing a file if it has more than this number of trigrams"),
        )
        .arg(
            clap::Arg::with_name("MAX_INVALID_UTF8_RATIO")
                .long("maxinvalidutf8ratio")
                .takes_value(true)
                .help(
                    "skip indexing a file if it has more than this ratio of invalid UTF-8 \
                   sequences",
                ),
        )
        .arg(
            clap::Arg::with_name("EXCLUDE_FILE")
                .long("exclude")
                .takes_value(true)
                .help("path to file containing a list of file patterns to exclude from indexing"),
        )
        .arg(
            clap::Arg::with_name("FILE")
                .long("filelist")
                .takes_value(true)
                .help("path to file containing a list of file paths to index"),
        )
        .arg(
            clap::Arg::with_name("verbose")
                .long("verbose")
                .help("print extra information"),
        )
        .arg(
            clap::Arg::with_name("logskip")
                .long("logskip")
                .help("print why a file was skipped from indexing"),
        )
        .get_matches();

    let max_log_level = if matches.is_present("verbose") {
        LevelFilter::Trace
    } else {
        LevelFilter::Info
    };
    libcustomlogger::init(max_log_level).unwrap();

    let mut excludes: Vec<glob::Pattern> = vec![glob::Pattern::new(".csearchindex").unwrap()];
    let mut args = Vec::<String>::new();

    if let Some(p) = matches.values_of("path") {
        args.extend(p.map(String::from));
    }

    if let Some(p) = matches.value_of("INDEX_FILE") {
        env::set_var("CSEARCHINDEX", p);
    }

    if matches.is_present("list-paths") {
        let i = open_index_or_fail();
        for each_file in i.indexed_paths() {
            println!("{}", each_file);
        }
        return;
    }
    if matches.is_present("reset-index") {
        let index_path = libcsearch::csearch_index();
        let p = Path::new(&index_path);
        if !p.exists() {
            // does not exist so nothing to do
            return;
        }
        let meta = p
            .metadata()
            .expect("failed to get metadata for file!")
            .file_type();
        if is_regular_file(meta) {
            std::fs::remove_file(p).expect("failed to remove file");
        }
        return;
    }
    if let Some(exc_path_str) = matches.value_of("EXCLUDE_FILE") {
        let exclude_path = Path::new(exc_path_str);
        let f = BufReader::new(File::open(exclude_path).expect("exclude file open error"));
        excludes.extend(
            f.lines()
                .map(|f| glob::Pattern::new(f.unwrap().trim()).unwrap()),
        );
    }
    if let Some(file_list_str) = matches.value_of("FILE") {
        let file_list = Path::new(file_list_str);
        let f = BufReader::new(File::open(file_list).expect("filelist file open error"));
        args.extend(f.lines().map(|f| f.unwrap().trim().to_string()));
    }

    if args.is_empty() {
        let i = open_index_or_fail();
        for each_file in i.indexed_paths() {
            args.push(each_file);
        }
    }

    let log_skipped = matches.is_present("logskip");
    let mut paths: Vec<PathBuf> = args
        .iter()
        .filter(|f| !f.is_empty())
        .map(|f| env::current_dir().unwrap().join(f))
        .filter_map(|f| match normalize(&f) {
            Ok(p) => Some(p),
            Err(e) => {
                if log_skipped {
                    warn!("{}: skipped. {}", f.to_str().unwrap_or_default(), e.kind());
                }
                None
            }
        })
        .collect();
    paths.sort();

    let mut index_path = libcsearch::csearch_index();
    let needs_merge = if Path::new(&index_path).exists() {
        index_path.push('~');
        true
    } else {
        false
    };

    let (tx, rx) = mpsc::channel::<OsString>();
    // copying these variables into the worker thread
    let index_path_cloned = index_path.clone();
    let paths_cloned = paths.clone();
    let h = thread::spawn(move || {
        let mut seen = HashSet::<OsString>::new();
        let mut i = match IndexWriter::new(index_path_cloned) {
            Ok(i) => i,
            Err(e) => panic!("IndexWriter: {}", e),
        };
        if let Some(t) = get_value_from_matches::<u64>(&matches, "MAX_TRIGRAMS_COUNT") {
            i.max_trigram_count = t;
        }
        if let Some(u) = get_value_from_matches::<f64>(&matches, "MAX_INVALID_UTF8_RATIO") {
            i.max_utf8_invalid = u;
        }
        if let Some(s) = get_value_from_matches::<u64>(&matches, "MAX_FILE_SIZE_BYTES") {
            i.max_file_len = s;
        }
        if let Some(b) = get_value_from_matches::<u64>(&matches, "MAX_LINE_LEN_BYTES") {
            i.max_line_len = b;
        }
        i.add_paths(paths_cloned.into_iter().map(PathBuf::into_os_string));
        let _frame = libprofiling::profile("Index files");
        while let Ok(f) = rx.recv() {
            if seen.contains(&f) {
                continue;
            }
            if let Err(ref e) = i.add_file(&f) {
                match e.kind() {
                    IndexErrorKind::IoError(_) => warn!("{}: {}", Path::new(&f).display(), e),
                    _ if log_skipped => warn!("{:?}: skipped. {}", f, e),
                    _ => (),
                }
            }
            seen.insert(f);
        }
        info!("flush index");
        i.flush().expect("failed to flush index to disk");
        // drop(_frame);
        libprofiling::print_profiling();
    });

    for each_path in paths {
        if !each_path.exists() {
            warn!("{} - path doesn't exist. Skipping...", each_path.display());
            continue;
        }
        if each_path.is_dir() {
            debug!("index {}", each_path.display());
            let tx = tx.clone();
            let files = WalkDir::new(each_path)
                .follow_links(true)
                .into_iter()
                .filter_entry(|d| {
                    let p = d.path();
                    !excludes.iter().any(|r| r.matches_path(p))
                })
                .filter_map(Result::ok)
                .filter(|d| !d.file_type().is_dir());

            for d in files {
                tx.send(OsString::from(d.path())).unwrap();
            }
        } else if each_path.is_file() {
            debug!("index file {}", each_path.display());
            tx.send(OsString::from(each_path)).unwrap();
        }
    }
    drop(tx);
    h.join().unwrap();
    if needs_merge {
        let dest_path = index_path.clone() + "~";
        let src1_path = libcsearch::csearch_index();
        let src2_path = index_path.clone();
        info!("merge {} {}", src1_path, src2_path);
        libcindex::merge::merge(dest_path, src1_path, src2_path).unwrap();
        fs::remove_file(index_path.clone()).unwrap();
        fs::remove_file(libcsearch::csearch_index()).unwrap();
        fs::rename(index_path + "~", libcsearch::csearch_index()).unwrap();
    }

    info!("done");
    libprofiling::print_profiling();
}

fn open_index_or_fail() -> IndexReader {
    let index_path = libcsearch::csearch_index();
    match IndexReader::open(&index_path) {
        Ok(i) => i,
        Err(e) => {
            error!("open {}: {}", index_path, e);
            std::process::exit(101);
        }
    }
}
