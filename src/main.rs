#![allow(dead_code, unused_imports)]

extern crate bytecount;
#[macro_use]
extern crate clap;
extern crate ctrlc;
extern crate env_logger;
extern crate grep;
extern crate ignore;
#[cfg(windows)]
extern crate kernel32;
#[macro_use]
extern crate lazy_static;
extern crate libc;
#[macro_use]
extern crate log;
extern crate memchr;
extern crate memmap;
extern crate num_cpus;
extern crate regex;
extern crate termcolor;
#[cfg(windows)]
extern crate winapi;

use std::error::Error;
use std::io;
use std::io::Write;
use std::process;
use std::result;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use termcolor::WriteColor;

use args::Args;
use worker::Work;

macro_rules! errored {
    ($($tt:tt)*) => {
        return Err(From::from(format!($($tt)*)));
    }
}

macro_rules! eprintln {
    ($($tt:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(&mut ::std::io::stderr(), $($tt)*);
    }}
}

mod app;
mod args;
mod atty;
mod pathutil;
mod printer;
mod search_buffer;
mod search_stream;
mod unescape;
mod worker;

pub type Result<T> = result::Result<T, Box<Error + Send + Sync>>;

fn main() {
    match Args::parse().map(Arc::new).and_then(run) {
        Ok(0) => process::exit(1),
        Ok(_) => process::exit(0),
        Err(err) => {
            eprintln!("{}", err);
            process::exit(1);
        }
    }
}

fn run(args: Arc<Args>) -> Result<u64> {
    if args.never_match() {
        return Ok(0);
    }
    {
        let args = args.clone();
        ctrlc::set_handler(move || {
            let mut writer = args.stdout();
            let _ = writer.reset();
            let _ = writer.flush();
            process::exit(1);
        });
    }
    let threads = args.threads();
    if args.files() {
        if threads == 1 || args.is_one_path() {
            run_files_one_thread(args)
        } else {
            run_files_parallel(args)
        }
    } else if args.type_list() {
        run_types(args)
    } else if threads == 1 || args.is_one_path() {
        run_one_thread(args)
    } else {
        run_parallel(args)
    }
}

fn run_parallel(args: Arc<Args>) -> Result<u64> {
    let bufwtr = Arc::new(args.buffer_writer());
    let quiet_matched = QuietMatched::new(args.quiet());
    let paths_searched = Arc::new(AtomicUsize::new(0));
    let match_count = Arc::new(AtomicUsize::new(0));

    args.walker_parallel().run(|| {
        let args = args.clone();
        let quiet_matched = quiet_matched.clone();
        let paths_searched = paths_searched.clone();
        let match_count = match_count.clone();
        let bufwtr = bufwtr.clone();
        let mut buf = bufwtr.buffer();
        let mut worker = args.worker();
        Box::new(move |result| {
            use ignore::WalkState::*;

            if quiet_matched.has_match() {
                return Quit;
            }
            let dent = match get_or_log_dir_entry(result, args.no_messages()) {
                None => return Continue,
                Some(dent) => dent,
            };
            paths_searched.fetch_add(1, Ordering::SeqCst);
            buf.clear();
            {
                // This block actually executes the search and prints the
                // results into outbuf.
                let mut printer = args.printer(&mut buf);
                let count =
                    if dent.is_stdin() {
                        worker.run(&mut printer, Work::Stdin)
                    } else {
                        worker.run(&mut printer, Work::DirEntry(dent))
                    };
                match_count.fetch_add(count as usize, Ordering::SeqCst);
                if quiet_matched.set_match(count > 0) {
                    return Quit;
                }
            }
            // BUG(burntsushi): We should handle this error instead of ignoring
            // it. See: https://github.com/BurntSushi/ripgrep/issues/200
            let _ = bufwtr.print(&buf);
            Continue
        })
    });
    if !args.paths().is_empty() && paths_searched.load(Ordering::SeqCst) == 0 {
        if !args.no_messages() {
            eprint_nothing_searched();
        }
    }
    Ok(match_count.load(Ordering::SeqCst) as u64)
}

fn run_one_thread(args: Arc<Args>) -> Result<u64> {
    let stdout = args.stdout();
    let mut stdout = stdout.lock();
    let mut worker = args.worker();
    let mut paths_searched: u64 = 0;
    let mut match_count = 0;
    for result in args.walker() {
        let dent = match get_or_log_dir_entry(result, args.no_messages()) {
            None => continue,
            Some(dent) => dent,
        };
        let mut printer = args.printer(&mut stdout);
        if match_count > 0 {
            if args.quiet() {
                break;
            }
            if let Some(sep) = args.file_separator() {
                printer = printer.file_separator(sep);
            }
        }
        paths_searched += 1;
        match_count +=
            if dent.is_stdin() {
                worker.run(&mut printer, Work::Stdin)
            } else {
                worker.run(&mut printer, Work::DirEntry(dent))
            };
    }
    if !args.paths().is_empty() && paths_searched == 0 {
        if !args.no_messages() {
            eprint_nothing_searched();
        }
    }
    Ok(match_count)
}

fn run_files_parallel(args: Arc<Args>) -> Result<u64> {
    let print_args = args.clone();
    let (tx, rx) = mpsc::channel::<ignore::DirEntry>();
    let print_thread = thread::spawn(move || {
        let stdout = print_args.stdout();
        let mut printer = print_args.printer(stdout.lock());
        let mut file_count = 0;
        for dent in rx.iter() {
            printer.path(dent.path());
            file_count += 1;
        }
        file_count
    });
    let no_messages = args.no_messages();
    args.walker_parallel().run(move || {
        let tx = tx.clone();
        Box::new(move |result| {
            if let Some(dent) = get_or_log_dir_entry(result, no_messages) {
                tx.send(dent).unwrap();
            }
            ignore::WalkState::Continue
        })
    });
    Ok(print_thread.join().unwrap())
}

fn run_files_one_thread(args: Arc<Args>) -> Result<u64> {
    let stdout = args.stdout();
    let mut printer = args.printer(stdout.lock());
    let mut file_count = 0;
    for result in args.walker() {
        let dent = match get_or_log_dir_entry(result, args.no_messages()) {
            None => continue,
            Some(dent) => dent,
        };
        printer.path(dent.path());
        file_count += 1;
    }
    Ok(file_count)
}

fn run_types(args: Arc<Args>) -> Result<u64> {
    let stdout = args.stdout();
    let mut printer = args.printer(stdout.lock());
    let mut ty_count = 0;
    for def in args.type_defs() {
        printer.type_def(def);
        ty_count += 1;
    }
    Ok(ty_count)
}

fn get_or_log_dir_entry(
    result: result::Result<ignore::DirEntry, ignore::Error>,
    no_messages: bool,
) -> Option<ignore::DirEntry> {
    match result {
        Err(err) => {
            if !no_messages {
                eprintln!("{}", err);
            }
            None
        }
        Ok(dent) => {
            if let Some(err) = dent.error() {
                if !no_messages {
                    eprintln!("{}", err);
                }
            }
            let ft = match dent.file_type() {
                None => return Some(dent), // entry is stdin
                Some(ft) => ft,
            };
            // A depth of 0 means the user gave the path explicitly, so we
            // should always try to search it.
            if dent.depth() == 0 && !ft.is_dir() {
                Some(dent)
            } else if ft.is_file() {
                Some(dent)
            } else {
                None
            }
        }
    }
}

fn version() -> String {
    let (maj, min, pat) = (
        option_env!("CARGO_PKG_VERSION_MAJOR"),
        option_env!("CARGO_PKG_VERSION_MINOR"),
        option_env!("CARGO_PKG_VERSION_PATCH"),
    );
    match (maj, min, pat) {
        (Some(maj), Some(min), Some(pat)) => {
            format!("ripgrep {}.{}.{}", maj, min, pat)
        }
        _ => "".to_owned(),
    }
}

fn eprint_nothing_searched() {
    eprintln!("No files were searched, which means ripgrep probably \
               applied a filter you didn't expect. \
               Try running again with --debug.");
}

/// A simple thread safe abstraction for determining whether a search should
/// stop if the user has requested quiet mode.
#[derive(Clone, Debug)]
pub struct QuietMatched(Arc<Option<AtomicBool>>);

impl QuietMatched {
    /// Create a new QuietMatched value.
    ///
    /// If quiet is true, then set_match and has_match will reflect whether
    /// a search should quit or not because it found a match.
    ///
    /// If quiet is false, then set_match is always a no-op and has_match
    /// always returns false.
    pub fn new(quiet: bool) -> QuietMatched {
        let atomic = if quiet { Some(AtomicBool::new(false)) } else { None };
        QuietMatched(Arc::new(atomic))
    }

    /// Returns true if and only if quiet mode is enabled and a match has
    /// occurred.
    pub fn has_match(&self) -> bool {
        match *self.0 {
            None => false,
            Some(ref matched) => matched.load(Ordering::SeqCst),
        }
    }

    /// Sets whether a match has occurred or not.
    ///
    /// If quiet mode is disabled, then this is a no-op.
    pub fn set_match(&self, yes: bool) -> bool {
        match *self.0 {
            None => false,
            Some(_) if !yes => false,
            Some(ref m) => { m.store(true, Ordering::SeqCst); true }
        }
    }
}
